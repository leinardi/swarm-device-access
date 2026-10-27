//go:build wirecapture

/*
 * Copyright 2026 Roberto Leinardi.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// Package wirecapture records what the Go daemon and launcher send the Docker Engine and what
// they get back, for the Rust port's engine-wire fixtures (tests/testdata/engine_wire). It is
// built only with the wirecapture tag: the capture tests need a live Docker daemon and are run by
// hand with make wire-capture, never by a normal go test run.
//
// A Proxy listens on a unix socket in front of the Docker socket and forwards each HTTP request
// as it arrives, keeping every request and the response its client received. Two responses can
// be narrowed to the containers a capture creates, so the host's other containers neither appear
// in the fixtures nor drive requests of their own: the container list and the event stream.
package wirecapture

import (
	"bufio"
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"unicode/utf8"
)

// DockerSocket is the socket the proxy forwards to.
const DockerSocket = "/var/run/docker.sock"

// Modes of what a capture creates: directories and files only the owner writes.
const (
	dirMode  = 0o750
	fileMode = 0o600
)

// Exchange is one request and the response its client received.
type Exchange struct {
	// Seq numbers the requests in the order they arrived, over all connections.
	Seq int `json:"seq"`
	// Conn numbers the client connection the request came on.
	Conn int `json:"conn"`
	// Method, URI and Host are the request line and Host header as sent.
	Method string `json:"method"`
	URI    string `json:"uri"`
	Host   string `json:"host"`
	// Header holds the other request headers, keys as Go canonicalizes them.
	Header http.Header `json:"header"`
	// TransferEncoding is the request's, when it set one.
	TransferEncoding []string `json:"transferEncoding,omitempty"`
	// Body is the request body.
	Body string `json:"body,omitempty"`

	// Status is the response status line without the protocol.
	Status string `json:"status"`
	// ResponseHeader holds the response headers, keys as Go canonicalizes them.
	ResponseHeader http.Header `json:"responseHeader"`
	// ResponseTransferEncoding is the response's, when it set one.
	ResponseTransferEncoding []string `json:"responseTransferEncoding,omitempty"`
	// ResponseBody is the body the client received, after any narrowing, when it is UTF-8;
	// ResponseBodyBase64 holds it otherwise.
	ResponseBody       string `json:"responseBody,omitempty"`
	ResponseBodyBase64 string `json:"responseBodyBase64,omitempty"`
	// Upgraded is set for a 101 response; UpgradedFromServer then holds the raw bytes the
	// server sent on the hijacked connection, and UpgradedFromClient those the client sent.
	Upgraded           bool   `json:"upgraded,omitempty"`
	UpgradedFromServer string `json:"upgradedFromServerBase64,omitempty"`
	UpgradedFromClient string `json:"upgradedFromClientBase64,omitempty"`
	// Cut is set when the proxy closed the connection while the response streamed.
	Cut bool `json:"cut,omitempty"`
	// Synthetic is set when the proxy answered itself instead of forwarding (RefuseHeadPing).
	Synthetic bool `json:"synthetic,omitempty"`

	body       bytes.Buffer
	fromServer bytes.Buffer
	fromClient bytes.Buffer
}

// Options shape what the proxy forwards.
type Options struct {
	// Keep, when not nil, narrows the container list and the event stream to the containers it
	// accepts by ID.
	Keep func(id string) bool
	// RefuseHeadPing answers every HEAD /_ping with 405 itself, as a daemon without HEAD support
	// would, so moby falls back to GET /_ping.
	RefuseHeadPing bool
}

// Proxy is a recording proxy in front of the Docker socket.
type Proxy struct {
	// Socket is the path clients dial.
	Socket string

	listener net.Listener
	options  Options

	mu        sync.Mutex
	exchanges []*Exchange
	conns     map[int]*proxyConn
	nextConn  int
	waiting   sync.WaitGroup
}

// proxyConn is one client connection and the request it is serving.
type proxyConn struct {
	client   net.Conn
	upstream net.Conn
	current  *Exchange
}

// Start listens on socket and forwards to DockerSocket until Close.
func Start(ctx context.Context, socket string, options Options) (*Proxy, error) {
	var config net.ListenConfig

	listener, err := config.Listen(ctx, "unix", socket)
	if err != nil {
		return nil, fmt.Errorf("listen on %s: %w", socket, err)
	}

	proxy := &Proxy{
		Socket:   socket,
		listener: listener,
		options:  options,
		conns:    make(map[int]*proxyConn),
	}

	proxy.waiting.Go(func() { proxy.accept(ctx) })

	return proxy, nil
}

// Cut closes every connection whose current request path contains pathPart, marking its
// exchange as cut, and returns how many it closed.
func (p *Proxy) Cut(pathPart string) int {
	p.mu.Lock()
	defer p.mu.Unlock()

	closed := 0

	for _, conn := range p.conns {
		if conn.current == nil || !strings.Contains(conn.current.URI, pathPart) {
			continue
		}

		conn.current.Cut = true
		_ = conn.client.Close()
		_ = conn.upstream.Close()
		closed++
	}

	return closed
}

// Requests returns how many requests have arrived so far.
func (p *Proxy) Requests() int {
	p.mu.Lock()
	defer p.mu.Unlock()

	return len(p.exchanges)
}

// Close stops listening, closes every connection and waits for them.
func (p *Proxy) Close() {
	_ = p.listener.Close()

	p.mu.Lock()

	for _, conn := range p.conns {
		_ = conn.client.Close()
		_ = conn.upstream.Close()
	}

	p.mu.Unlock()
	p.waiting.Wait()
}

// Write closes the proxy and writes every exchange, in arrival order, to path as JSON Lines.
func (p *Proxy) Write(path string) error {
	p.Close()

	var out bytes.Buffer

	encoder := json.NewEncoder(&out)
	encoder.SetEscapeHTML(false)

	for _, exchange := range p.exchanges {
		exchange.finish()

		err := encoder.Encode(exchange)
		if err != nil {
			return fmt.Errorf("encode exchange %d: %w", exchange.Seq, err)
		}
	}

	err := os.MkdirAll(filepath.Dir(path), dirMode)
	if err != nil {
		return fmt.Errorf("create the directory of %s: %w", path, err)
	}

	err = os.WriteFile(path, out.Bytes(), fileMode)
	if err != nil {
		return fmt.Errorf("write %s: %w", path, err)
	}

	return nil
}

// Bodies returns the response bodies recorded so far of the exchanges whose URI contains
// pathPart, streams still open included.
func (p *Proxy) Bodies(pathPart string) []string {
	p.mu.Lock()
	defer p.mu.Unlock()

	var bodies []string

	for _, exchange := range p.exchanges {
		if strings.Contains(exchange.URI, pathPart) {
			bodies = append(bodies, exchange.body.String())
		}
	}

	return bodies
}

func (p *Proxy) accept(ctx context.Context) {
	for {
		client, err := p.listener.Accept()
		if err != nil {
			return
		}

		var dialer net.Dialer

		upstream, err := dialer.DialContext(ctx, "unix", DockerSocket)
		if err != nil {
			_ = client.Close()

			continue
		}

		p.mu.Lock()
		p.nextConn++
		index := p.nextConn
		conn := &proxyConn{client: client, upstream: upstream}
		p.conns[index] = conn
		p.mu.Unlock()

		p.waiting.Go(func() {
			p.serve(index, conn)

			_ = client.Close()
			_ = upstream.Close()

			p.mu.Lock()
			delete(p.conns, index)
			p.mu.Unlock()
		})
	}
}

// serve forwards the requests of one connection, one at a time, as HTTP/1.1 keep-alive does.
func (p *Proxy) serve(index int, conn *proxyConn) {
	fromClient := bufio.NewReader(conn.client)
	fromServer := bufio.NewReader(conn.upstream)

	for {
		request, err := http.ReadRequest(fromClient)
		if err != nil {
			return
		}

		exchange, err := p.begin(index, conn, request)
		if err != nil {
			return
		}

		if p.options.RefuseHeadPing && request.Method == http.MethodHead &&
			strings.HasSuffix(request.URL.Path, "/_ping") {
			err = refuseHead(conn.client, exchange)
			if err != nil {
				return
			}

			p.mu.Lock()
			conn.current = nil
			p.mu.Unlock()

			continue
		}

		err = request.Write(conn.upstream)
		if err != nil {
			return
		}

		response, err := http.ReadResponse(fromServer, request)
		if err != nil {
			return
		}

		done := p.relay(conn, fromClient, fromServer, request, response, exchange)

		_ = response.Body.Close()

		if done {
			return
		}

		p.mu.Lock()
		conn.current = nil
		p.mu.Unlock()
	}
}

// relay hands one response to the client and reports whether the connection is done.
func (p *Proxy) relay(
	conn *proxyConn,
	fromClient, fromServer *bufio.Reader,
	request *http.Request,
	response *http.Response,
	exchange *Exchange,
) bool {
	exchange.Status = response.Status
	exchange.ResponseHeader = response.Header.Clone()
	exchange.ResponseTransferEncoding = response.TransferEncoding

	if response.StatusCode == http.StatusSwitchingProtocols {
		exchange.Upgraded = true
		pipeUpgraded(conn, fromClient, fromServer, response, exchange)

		return true
	}

	err := p.forward(conn.client, request, response, exchange)

	return err != nil || request.Close || response.Close
}

// refuseHead answers a HEAD request with 405 and no body, recording the answer as synthetic.
func refuseHead(client net.Conn, exchange *Exchange) error {
	const answer = "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n"

	exchange.Status = "405 Method Not Allowed"
	exchange.ResponseHeader = http.Header{"Content-Length": {"0"}}
	exchange.Synthetic = true

	_, err := io.WriteString(client, answer)
	if err != nil {
		return fmt.Errorf("answer HEAD: %w", err)
	}

	return nil
}

// begin records a request and reads its body, which it leaves for forwarding.
func (p *Proxy) begin(index int, conn *proxyConn, request *http.Request) (*Exchange, error) {
	body, err := io.ReadAll(request.Body)
	if err != nil {
		return nil, fmt.Errorf("read request body: %w", err)
	}

	// An empty body is sent as none: a body reader of unknown length would go out chunked, and
	// dockerd refuses a start with a body.
	request.Body = http.NoBody
	if len(body) > 0 {
		request.Body = io.NopCloser(bytes.NewReader(body))
	}

	exchange := &Exchange{
		Conn:             index,
		Method:           request.Method,
		URI:              request.RequestURI,
		Host:             request.Host,
		Header:           request.Header.Clone(),
		TransferEncoding: request.TransferEncoding,
		Body:             string(body),
	}

	p.mu.Lock()
	exchange.Seq = len(p.exchanges) + 1
	p.exchanges = append(p.exchanges, exchange)
	conn.current = exchange
	p.mu.Unlock()

	return exchange, nil
}

// forward writes response to the client as it streams, narrowed when it is the container list or
// the event stream, recording the body the client receives.
func (p *Proxy) forward(
	client net.Conn,
	request *http.Request,
	response *http.Response,
	exchange *Exchange,
) error {
	path := request.URL.Path

	switch {
	case p.options.Keep != nil && strings.HasSuffix(path, "/containers/json") && response.StatusCode == http.StatusOK:
		body, err := io.ReadAll(response.Body)
		if err != nil {
			return fmt.Errorf("read the container list: %w", err)
		}

		narrowed, err := p.narrowList(body)
		if err != nil {
			return err
		}

		response.Body = io.NopCloser(bytes.NewReader(narrowed))
		response.ContentLength = int64(len(narrowed))
		response.TransferEncoding = nil
		exchange.ResponseTransferEncoding = nil
		exchange.ResponseHeader.Set("Content-Length", strconv.Itoa(len(narrowed)))
	case p.options.Keep != nil && strings.HasSuffix(path, "/events"):
		response.Body = io.NopCloser(
			&eventFilter{lines: bufio.NewReader(response.Body), keep: p.options.Keep},
		)
	}

	response.Body = io.NopCloser(
		io.TeeReader(response.Body, &locked{mu: &p.mu, buffer: &exchange.body}),
	)

	err := response.Write(client)
	if err != nil {
		return fmt.Errorf("forward the response: %w", err)
	}

	return nil
}

// narrowList keeps the entries of a container-list body whose Id keep accepts, writing the
// kept entries as they came, joined as Docker joins them.
func (p *Proxy) narrowList(body []byte) ([]byte, error) {
	var entries []json.RawMessage

	err := json.Unmarshal(body, &entries)
	if err != nil {
		return nil, fmt.Errorf("decode the container list: %w", err)
	}

	kept := make([][]byte, 0, len(entries))

	for _, entry := range entries {
		var summary map[string]any

		err = json.Unmarshal(entry, &summary)
		if err != nil {
			return nil, fmt.Errorf("decode a container list entry: %w", err)
		}

		containerID, _ := summary["Id"].(string)
		if p.options.Keep(containerID) {
			kept = append(kept, entry)
		}
	}

	trailer := body[bytes.LastIndexByte(body, ']')+1:]

	return slices.Concat([]byte("["), bytes.Join(kept, []byte(",")), []byte("]"), trailer), nil
}

// eventFilter passes the lines of an event stream whose actor keep accepts.
type eventFilter struct {
	lines   *bufio.Reader
	keep    func(id string) bool
	pending []byte
}

func (f *eventFilter) Read(out []byte) (int, error) {
	for len(f.pending) == 0 {
		line, err := f.lines.ReadBytes('\n')
		if len(line) > 0 && f.kept(line) {
			// An error that came with the line comes again on the next read.
			f.pending = line

			break
		}

		if errors.Is(err, io.EOF) {
			return 0, io.EOF
		}

		if err != nil {
			return 0, fmt.Errorf("read the event stream: %w", err)
		}
	}

	written := copy(out, f.pending)
	f.pending = f.pending[written:]

	return written, nil
}

func (f *eventFilter) kept(line []byte) bool {
	var message map[string]any

	if json.Unmarshal(line, &message) != nil {
		return false
	}

	actor, _ := message["Actor"].(map[string]any)
	actorID, _ := actor["ID"].(string)

	return f.keep(actorID)
}

// pipeUpgraded writes the 101 response and then copies the hijacked connection both ways until
// each side ends, recording the bytes.
func pipeUpgraded(
	conn *proxyConn,
	fromClient, fromServer *bufio.Reader,
	response *http.Response,
	exchange *Exchange,
) {
	var head bytes.Buffer

	head.WriteString("HTTP/1.1 " + response.Status + "\r\n")
	_ = response.Header.Write(&head)
	head.WriteString("\r\n")

	_, err := conn.client.Write(head.Bytes())
	if err != nil {
		return
	}

	var copies sync.WaitGroup

	copies.Go(func() {
		_, _ = io.Copy(conn.client, io.TeeReader(fromServer, &exchange.fromServer))
		closeWrite(conn.client)
	})
	copies.Go(func() {
		_, _ = io.Copy(conn.upstream, io.TeeReader(fromClient, &exchange.fromClient))
		closeWrite(conn.upstream)
	})
	copies.Wait()
}

func closeWrite(conn net.Conn) {
	unix, ok := conn.(*net.UnixConn)
	if ok {
		_ = unix.CloseWrite()
	}
}

// finish moves the recorded bodies into their fields.
func (e *Exchange) finish() {
	body := e.body.Bytes()
	if utf8.Valid(body) {
		e.ResponseBody = string(body)
	} else {
		e.ResponseBodyBase64 = base64.StdEncoding.EncodeToString(body)
	}

	if e.Upgraded {
		e.UpgradedFromServer = base64.StdEncoding.EncodeToString(e.fromServer.Bytes())
		e.UpgradedFromClient = base64.StdEncoding.EncodeToString(e.fromClient.Bytes())
	}
}

// locked is a buffer written under a mutex, so a streaming response can be recorded while Cut
// runs.
type locked struct {
	mu     *sync.Mutex
	buffer *bytes.Buffer
}

func (l *locked) Write(data []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()

	// bytes.Buffer.Write never fails.
	written, _ := l.buffer.Write(data)

	return written, nil
}

# HTTP server captures

What the Go daemon's metrics server (`StartMetricsServer`, `internal/observability/http.go`, Go
1.26) answered on the wire, for the Rust server to reproduce byte for byte
(`src/server/tests/wire.rs`; `docs/rust-port.md`, Q11). They replace the captures of
swarm-scheduler-exporter's own server, whose routes and write deadline differ.

Each `go-<case>.txt` was written by a scratch Go test, since deleted, that started the Go server
on loopback and sent one raw request over TCP:

- the first line names the case and the server's port: 18080 for a ready daemon, 18081 for one
  that is not ready yet (`/readyz` answers 503);
- the second line is the request, with `\r\n` written out;
- then the response exactly as read, and `[end: <nil>]` once the server closed the connection.

`go-get-metrics.txt` was cut at 1200 bytes, before its end: its body is the Go runtime families
(Q3), which change with every run. Every other capture is complete, its `Date` included, which the
replay reproduces with a fake clock set to it.

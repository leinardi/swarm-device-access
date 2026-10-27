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

// Adapted from swarm-scheduler-exporter/src/server/tests/shutdown.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The listener and the drain on real loopback sockets: Go's start and stop tests
//! (`http_test.go`), the shutdown probe of swarm-scheduler-exporter's Go server, `SO_REUSEADDR`,
//! and how `net.Listen` binds.

use tokio::net::TcpStream;
use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;

use super::*;

/// How long a test waits for something that should happen at once.
const BOUND: Duration = Duration::from_secs(5);

async fn get(stream: &mut TcpStream, path: &str) -> String {
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = vec![0_u8; 4096];
    let read = stream.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..read]).into_owned()
}

/// Go's shutdown probe: after the signal new connects are refused, an idle keep-alive
/// connection is closed at once, and the request in flight is answered, with `Connection:
/// close`, before the drain ends.
#[tokio::test]
async fn the_drain_waits_for_the_request_in_flight_and_refuses_new_ones() {
    let listener = bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let served = tokio::spawn(serve(
        listener,
        Arc::new(slow_app(Duration::from_millis(600))),
        Timeouts::DAEMON,
        async {
            let _ = stopped.await;
        },
        || Instant::now() + Duration::from_secs(10),
        |error| panic!("{error}"),
    ));
    let mut idle = TcpStream::connect(address).await.unwrap();
    assert!(
        get(&mut idle, "/healthz")
            .await
            .starts_with("HTTP/1.1 200 OK")
    );
    let mut in_flight = TcpStream::connect(address).await.unwrap();
    in_flight
        .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let signalled = Instant::now();
    stop.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let refused = TcpStream::connect(address).await.unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::ConnectionRefused);
    let mut rest = Vec::new();
    assert_eq!(
        idle.read_to_end(&mut rest).await.unwrap(),
        0,
        "the idle keep-alive closed at once"
    );
    assert!(
        !served.is_finished(),
        "the drain must wait for the request in flight"
    );
    let mut response = Vec::new();
    in_flight.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("\r\nConnection: close\r\n"), "{response}");
    let served = served.await.unwrap();
    assert_eq!(served.drained, Drained::Clean);
    assert!(signalled.elapsed() < Duration::from_millis(900));
}

/// The drain gives up at the deadline it is handed, not after a grace period of its own.
#[tokio::test]
async fn the_drain_stops_at_the_deadline() {
    let listener = bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let served = tokio::spawn(serve(
        listener,
        Arc::new(slow_app(Duration::from_secs(30))),
        Timeouts::DAEMON,
        async {
            let _ = stopped.await;
        },
        || Instant::now() + Duration::from_millis(300),
        |error| panic!("{error}"),
    ));
    let mut in_flight = TcpStream::connect(address).await.unwrap();
    in_flight
        .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let signalled = Instant::now();
    stop.send(()).unwrap();
    assert_eq!(served.await.unwrap().drained, Drained::DeadlineExceeded);
    let took = signalled.elapsed();
    assert!(
        // Well short of any grace period of its own, with room for a loaded runner.
        took >= Duration::from_millis(300) && took < Duration::from_secs(3),
        "{took:?}"
    );
}

/// `SO_REUSEADDR` is on, read back with getsockopt: on the Go-style bind, and on tokio's own
/// bind too, which mio sets on Unix.
#[tokio::test]
async fn listeners_have_so_reuseaddr() {
    let tokio_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    assert!(
        socket2::SockRef::from(&tokio_listener)
            .reuse_address()
            .unwrap()
    );
    let listener = bind("127.0.0.1:0").await.unwrap();
    assert!(socket2::SockRef::from(&listener).reuse_address().unwrap());
}

/// Whether a socket on local `port` is in `TIME_WAIT`, from `/proc/net/tcp`.
fn in_time_wait(port: u16) -> bool {
    let table = std::fs::read_to_string("/proc/net/tcp").unwrap();
    let local = format!(":{port:04X}");
    table.lines().skip(1).any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields
            .get(1)
            .is_some_and(|address| address.ends_with(&local))
            && fields.get(3) == Some(&"06")
    })
}

/// A restart rebinds while the old server-side connection is in `TIME_WAIT`; a socket without
/// `SO_REUSEADDR` cannot (EADDRINUSE).
#[tokio::test]
async fn a_rebind_succeeds_with_a_connection_in_time_wait() {
    let first = bind("127.0.0.1:0").await.unwrap();
    let address = first.local_addr().unwrap();
    // The server closes first, so its side of the connection goes to TIME_WAIT.
    let client = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let mut sink = Vec::new();
        let _ = stream.read_to_end(&mut sink).await;
    });
    let (accepted, _) = first.accept().await.unwrap();
    drop(accepted);
    client.await.unwrap();
    drop(first);
    assert!(in_time_wait(address.port()), "no TIME_WAIT entry");

    let control = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    let error = control.bind(&address.into()).unwrap_err();
    assert_eq!(
        error.raw_os_error(),
        Some(98),
        "EADDRINUSE without SO_REUSEADDR"
    );
    drop(bind(&address.to_string()).await.unwrap());
    drop(TcpListener::bind(address).await.unwrap());
}

/// A busy port, a missing port and a bad port fail as Go's `net.Listen` does; tokio's own bind
/// cannot even read Go's `:port` (it resolves it, and fails in the resolver's words).
#[tokio::test]
async fn a_bind_failure_is_go_s_error() {
    let busy = bind("127.0.0.1:0").await.unwrap();
    let busy_address = busy.local_addr().unwrap();
    assert_eq!(
        bind(&busy_address.to_string())
            .await
            .unwrap_err()
            .to_string(),
        format!("listen tcp {busy_address}: bind: address already in use")
    );
    assert_eq!(
        bind("127.0.0.1").await.unwrap_err().to_string(),
        "listen tcp: address 127.0.0.1: missing port in address"
    );
    assert_eq!(
        bind("127.0.0.1:70000").await.unwrap_err().to_string(),
        "listen tcp: address 70000: invalid port"
    );
    assert_eq!(
        bind("127.0.0.1:nosuchsvc").await.unwrap_err().to_string(),
        "listen tcp: lookup tcp/nosuchsvc: unknown port"
    );
    assert!(TcpListener::bind(":8888").await.is_err());
}

/// The address Go's errors show for a busy port, by how the host was written, from a Go 1.26
/// probe of `net.Listen`: the wildcard literals all read `0.0.0.0`, an empty host `:port`, a
/// mapped address and a name their IPv4 address.
#[tokio::test]
async fn a_busy_port_is_shown_as_go_shows_it() {
    let busy = bind(":0").await.unwrap();
    let port = busy.local_addr().unwrap().port();
    for (host, shown) in [
        ("0.0.0.0", format!("0.0.0.0:{port}")),
        ("[::]", format!("0.0.0.0:{port}")),
        ("[::ffff:0.0.0.0]", format!("0.0.0.0:{port}")),
        ("", format!(":{port}")),
        ("127.0.0.1", format!("127.0.0.1:{port}")),
        ("[::ffff:127.0.0.1]", format!("127.0.0.1:{port}")),
        ("localhost", format!("127.0.0.1:{port}")),
    ] {
        assert_eq!(
            bind(&format!("{host}:{port}"))
                .await
                .unwrap_err()
                .to_string(),
            format!("listen tcp {shown}: bind: address already in use"),
            "{host}"
        );
    }
}

/// An IPv6 zone is an interface name, else a number, else 0, and is shown as given; a zone on an
/// IPv4 address makes it a name, which getaddrinfo refuses. From the same Go probe.
#[tokio::test]
async fn zones_are_go_s() {
    if !ipv6_available() {
        return;
    }
    let listener = bind("[::1%lo]:0").await.unwrap();
    assert_eq!(listener.local_addr().unwrap().ip().to_string(), "::1");
    for (address, cause) in [
        ("[fe80::1%lo]:0", "cannot assign requested address"),
        ("[fe80::1%nosuchif]:0", "invalid argument"),
        ("[fe80::1%999]:0", "no such device"),
    ] {
        assert_eq!(
            bind(address).await.unwrap_err().to_string(),
            format!("listen tcp {address}: bind: {cause}")
        );
    }
    assert_eq!(super::super::zone_index("lo"), 1);
    assert_eq!(super::super::zone_index("12ab"), 12);
    assert_eq!(super::super::zone_index("99999999"), 0x00FF_FFFF);
    assert_eq!(super::super::zone_index("nosuchif"), 0);
}

/// Go's resolver looks `inet_aton`'s shorthands up as names and finds nothing; getaddrinfo would
/// read `0` as `0.0.0.0` and bind the wildcard. A name that does not resolve has the resolver's
/// wording after Go's prefix (Q12).
#[tokio::test]
async fn names_resolve_as_go_s_resolver_does() {
    for host in ["0", "127.1", "0x7f.0.0.1", "010.0.0.1"] {
        assert_eq!(
            bind(&format!("{host}:0")).await.unwrap_err().to_string(),
            format!("listen tcp: lookup {host}: no such host")
        );
    }
    let error = bind("nonexistent.invalid:0").await.unwrap_err().to_string();
    assert!(
        error.starts_with("listen tcp: lookup nonexistent.invalid: "),
        "{error}"
    );
    let listener = bind("localhost:0").await.unwrap();
    assert_eq!(listener.local_addr().unwrap().ip().to_string(), "127.0.0.1");
}

/// The unspecified literals are the dual-stack wildcard too, as Go's `favoriteAddrFamily` makes
/// any wildcard; a mapped address listens as IPv4.
#[tokio::test]
async fn wildcard_literals_are_dual_stack() {
    for address in ["0.0.0.0:0", "[::]:0", "[::ffff:0.0.0.0]:0"] {
        let listener = bind(address).await.unwrap();
        let local = listener.local_addr().unwrap();
        assert!(local.ip().is_unspecified(), "{address}: {local}");
        if ipv6_available() {
            assert!(local.is_ipv6(), "{address}: {local}");
            assert!(!socket2::SockRef::from(&listener).only_v6().unwrap());
        }
    }
    let mapped = bind("[::ffff:127.0.0.1]:0").await.unwrap();
    assert_eq!(mapped.local_addr().unwrap().ip().to_string(), "127.0.0.1");
}

/// Whether the host can make IPv6 sockets at all.
fn ipv6_available() -> bool {
    Socket::new(Domain::IPV6, Type::STREAM, None).is_ok()
}

/// `:port` is the dual-stack wildcard, as Go's is: an IPv4 client reaches it.
#[tokio::test]
async fn an_empty_host_is_dual_stack() {
    let listener = bind(":0").await.unwrap();
    let local = listener.local_addr().unwrap();
    assert!(local.ip().is_unspecified(), "{local}");
    if ipv6_available() {
        assert!(local.is_ipv6(), "{local}");
        assert!(!socket2::SockRef::from(&listener).only_v6().unwrap());
    }
    let port = local.port();
    let accepted = tokio::spawn(async move { listener.accept().await.map(|(_, peer)| peer) });
    TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let peer = accepted.await.unwrap().unwrap();
    assert_eq!(peer.ip().to_canonical().to_string(), "127.0.0.1");
}

/// Go: swarm-scheduler-exporter's `TestServeHTTP_ShutdownWaitsForInFlightRequest`: once shutdown
/// starts, the server keeps waiting for the request in flight, and returns once it is answered.
#[tokio::test]
async fn the_shutdown_waits_for_the_request_in_flight() {
    let listener = bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut app = healthy_app();
    let (route_started, route_release) = (Arc::clone(&started), Arc::clone(&release));
    app.slow = Some(Arc::new(move || {
        let (started, release) = (Arc::clone(&route_started), Arc::clone(&route_release));
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
        })
    }));
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let served = tokio::spawn(async move {
        serve(
            listener,
            Arc::new(app),
            Timeouts::DAEMON,
            shutdown.cancelled_owned(),
            || Instant::now() + Duration::from_secs(10),
            |error| panic!("{error}"),
        )
        .await
    });
    let response = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response).await;
        String::from_utf8_lossy(&response).into_owned()
    });
    tokio::time::timeout(BOUND, started.notified())
        .await
        .expect("request not received");
    cancel.cancel();
    // It must keep waiting for the request in flight.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !served.is_finished(),
        "returned while a request was in flight"
    );
    release.notify_one();
    let served = tokio::time::timeout(BOUND, served).await.unwrap().unwrap();
    assert_eq!(served.drained, Drained::Clean);
    let response = tokio::time::timeout(BOUND, response)
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
}

/// A server started on a free loopback port, with `log` and `drain`.
async fn started(log: LogHandle, drain: Duration) -> (MetricsServer, SocketAddr) {
    let mut app = slow_app(Duration::from_secs(30));
    app.log = log;
    let server = start_draining_for(&CancellationToken::new(), "127.0.0.1:0", app, drain)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    (server, address)
}

/// Go: `TestStartServers_BusyAddressIsAnError`. A busy address is an error when the server
/// starts, worded as Go's; the debug server it also covered is Q2's refusal.
#[tokio::test]
async fn start_on_a_busy_address_is_an_error() {
    let busy = bind("127.0.0.1:0").await.unwrap();
    let address = busy.local_addr().unwrap().to_string();
    let error = start_metrics_server(&CancellationToken::new(), &address, healthy_app())
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "metrics server: listen on {address}: listen tcp {address}: bind: address already in \
             use"
        )
    );
    assert!(refuse_debug_addr(&address).is_err());
}

/// Go: `TestServe_StopReturnsAndClosesTheListener`. The stop returns, twice, and the server no
/// longer answers; the start and the stop are logged as Go logs them.
#[tokio::test]
async fn stop_returns_and_closes_the_listener() {
    let (log, sink) = capture_logs(Level::INFO);
    let (server, address) = started(log, SHUTDOWN_TIMEOUT).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    assert!(get(&mut stream, "/healthz").await.ends_with("\r\n\r\nok"));
    drop(stream);

    tokio::time::timeout(SHUTDOWN_TIMEOUT * 2, async {
        server.stop().await;
        server.stop().await;
    })
    .await
    .expect("stop did not return");
    assert!(
        TcpStream::connect(address).await.is_err(),
        "still answers after stop"
    );
    assert_eq!(
        sink.contents(),
        format!("level=INFO metrics server listening addr={address}\n")
    );
}

/// Go: `TestServe_ContextCancelStops`. The context's cancellation stops the server, and a stop
/// after it returns.
#[tokio::test]
async fn context_cancel_stops() {
    let context = CancellationToken::new();
    let server = start_metrics_server(&context, "127.0.0.1:0", healthy_app())
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    context.cancel();
    tokio::time::timeout(SHUTDOWN_TIMEOUT * 2, server.stop())
        .await
        .expect("stop after cancel did not return");
    assert!(TcpStream::connect(address).await.is_err());
}

/// A stop whose caller gave up waiting still leaves the next stop waiting for the drain, as Go's
/// `sync.OnceFunc` makes every call wait for the server.
#[tokio::test]
async fn a_stop_given_up_on_leaves_the_next_one_waiting() {
    let (log, _sink) = capture_logs(Level::INFO);
    let (server, address) = started(log, Duration::from_millis(500)).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let first = Instant::now();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), server.stop())
            .await
            .is_err()
    );
    tokio::time::timeout(BOUND, server.stop()).await.unwrap();
    assert!(
        first.elapsed() >= Duration::from_millis(400),
        "{:?}",
        first.elapsed()
    );
}

/// A drain cut short by its deadline is warned about as Go's failed `Shutdown` is.
#[tokio::test]
async fn a_drain_past_the_deadline_is_warned_about() {
    let (log, sink) = capture_logs(Level::INFO);
    let (server, address) = started(log, Duration::from_millis(200)).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::timeout(BOUND, server.stop()).await.unwrap();
    assert_eq!(
        sink.contents(),
        format!(
            "level=INFO metrics server listening addr={address}\n\
             level=WARN metrics server shutdown error err=\"context deadline exceeded\"\n"
        )
    );
}

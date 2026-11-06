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

//! The start and the stop on real loopback sockets: Go's start and stop tests (`http_test.go`),
//! and a listen error worded as Go's `net.Listen` words it, with the daemon's (empty) prefix.
//! How `net.Listen` binds, `SO_REUSEADDR` and the drain are go-http's tests.

use tokio::net::{TcpListener, TcpStream};
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

/// The daemon's bind: Go's `net.Listen`, its error with [`LISTEN_ERROR_PREFIX`].
async fn bind(address: &str) -> Result<TcpListener, ListenError> {
    go_http::bind(address, LISTEN_ERROR_PREFIX).await
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

/// A server started on a free loopback port, with `log` and `drain`.
async fn started(log: LogHandle, drain: Duration) -> (MetricsServer, SocketAddr) {
    let app = App::with_mux(
        slow_mux(sleeping(Duration::from_secs(30))),
        Arc::new(FakeClock::new(capture_time())),
        log,
    );
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

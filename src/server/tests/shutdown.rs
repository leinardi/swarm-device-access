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

//! The start and the stop on real loopback sockets: the listen errors, the stop that runs once,
//! the context that stops the server, and the drain within its budget or cut at its end.

use super::*;
use crate::test_support::capture_logs;
use tracing::level_filters::LevelFilter;

/// A busy address and an invalid one are startup errors, worded as the start error words
/// them: the prefix, the address and the cause.
#[tokio::test]
async fn a_bind_failure_is_a_start_error() {
    let busy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = busy.local_addr().unwrap().to_string();
    let error = start_metrics_server(
        &CancellationToken::new(),
        &address,
        App::new(readiness(true), Recorder::noop()),
    )
    .await
    .unwrap_err();
    let text = error.to_string();
    assert!(
        text.starts_with(&format!("metrics server: listen on {address}: ")),
        "{text}"
    );
    assert!(text.to_lowercase().contains("in use"), "{text}");
    assert!(refuse_debug_addr(&address).is_err());

    for invalid in ["127.0.0.1", "127.0.0.1:70000", "nonsense"] {
        let error = start_metrics_server(
            &CancellationToken::new(),
            invalid,
            App::new(readiness(true), Recorder::noop()),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with(&format!("metrics server: listen on {invalid}: ")),
            "{error}"
        );
    }
}

/// Go's `:port` binds every interface, dual-stack: it takes an IPv4 connection to 127.0.0.1 and,
/// where the host has IPv6, one to `::1`, whatever `net.ipv6.bindv6only` says.
#[tokio::test]
async fn a_port_alone_binds_every_interface() {
    let server = start_metrics_server(
        &CancellationToken::new(),
        ":0",
        App::new(readiness(true), Recorder::noop()),
    )
    .await
    .unwrap();
    let port = server.local_addr().unwrap().port();
    let v4: SocketAddr = ([127, 0, 0, 1], port).into();
    assert_eq!(status(&get(v4, "/healthz").await), "HTTP/1.1 200 OK");
    if std::net::TcpListener::bind("[::1]:0").is_ok() {
        let v6: SocketAddr = (std::net::Ipv6Addr::LOCALHOST, port).into();
        assert_eq!(status(&get(v6, "/healthz").await), "HTTP/1.1 200 OK");
    }
    server.stop().await;
}

/// A busy port stays the first error, not a fallback's: a second `:port` on it is "in use".
#[tokio::test]
async fn a_busy_wildcard_port_reports_the_first_error() {
    let first = start_metrics_server(
        &CancellationToken::new(),
        ":0",
        App::new(readiness(true), Recorder::noop()),
    )
    .await
    .unwrap();
    let port = first.local_addr().unwrap().port();
    let error = start_metrics_server(
        &CancellationToken::new(),
        &format!(":{port}"),
        App::new(readiness(true), Recorder::noop()),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().to_lowercase().contains("in use"),
        "{error}"
    );
    first.stop().await;
}

/// The stop returns, twice, and the server no longer answers; the start is logged.
#[tokio::test]
async fn stop_returns_and_closes_the_listener() {
    let logs = capture_logs(LevelFilter::INFO);
    let (server, address) = serving(slow_app(Duration::ZERO), short(SHUTDOWN_TIMEOUT)).await;
    let healthz = get(address, "/healthz").await;
    assert!(healthz.ends_with("\r\n\r\nok"), "{healthz}");

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
        logs.contents(),
        format!("level=INFO metrics server listening addr={address}\n")
    );
}

/// The context's cancellation stops the server, and a stop after it returns.
#[tokio::test]
async fn context_cancel_stops() {
    let context = CancellationToken::new();
    let server = start_metrics_server(
        &context,
        "127.0.0.1:0",
        App::new(readiness(true), Recorder::noop()),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    context.cancel();
    tokio::time::timeout(SHUTDOWN_TIMEOUT * 2, server.stop())
        .await
        .expect("stop after cancel did not return");
    assert!(TcpStream::connect(address).await.is_err());
}

/// A request in flight at the stop is answered in full when it ends within the drain, and the
/// stop returns once it has, with nothing to warn about.
#[tokio::test]
async fn the_drain_lets_a_request_in_flight_finish() {
    let logs = capture_logs(LevelFilter::INFO);
    let (server, address) = serving(
        slow_app(Duration::from_millis(300)),
        short(Duration::from_secs(3)),
    )
    .await;
    let request = tokio::spawn(async move { get(address, SLOW_PATH).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let start = Instant::now();
    tokio::time::timeout(BOUND, server.stop()).await.unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "the drain waited past the request: {:?}",
        start.elapsed()
    );
    let response = request.await.unwrap();
    assert_eq!(status(&response), "HTTP/1.1 200 OK", "{response}");
    assert!(response.contains("slow done"), "{response}");
    assert_eq!(
        logs.contents(),
        format!("level=INFO metrics server listening addr={address}\n")
    );
}

/// A stop whose caller gave up waiting still leaves the next stop waiting for the drain.
#[tokio::test]
async fn a_stop_given_up_on_leaves_the_next_one_waiting() {
    let (server, address) = serving(
        slow_app(Duration::from_secs(30)),
        short(Duration::from_millis(500)),
    )
    .await;
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

/// A drain cut short by its deadline is warned about, and the stop returns by then.
#[tokio::test]
async fn a_drain_past_the_deadline_is_warned_about() {
    let logs = capture_logs(LevelFilter::INFO);
    let (server, address) = serving(
        slow_app(Duration::from_secs(30)),
        short(Duration::from_millis(200)),
    )
    .await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let start = Instant::now();
    tokio::time::timeout(BOUND, server.stop()).await.unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        logs.contents(),
        format!(
            "level=INFO metrics server listening addr={address}\n\
             level=WARN metrics server shutdown error error=context deadline exceeded\n"
        )
    );
}

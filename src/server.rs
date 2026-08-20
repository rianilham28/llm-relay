//! The accept loop: one tokio task per connection, h1 or h2 chosen by preface,
//! and a drain on SIGTERM so a container stop does not cut live SSE streams.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};

use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;

use crate::{BoxError, State, route};

/// A connection that opens and then sends nothing is a slowloris; the platform
/// used to absorb that for us.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Long enough for an in-flight completion to finish. Keep the orchestrator's
/// stop grace period above this or it will SIGKILL mid-drain.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(25);
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn serve(state: Arc<State>, addr: SocketAddr) -> Result<(), BoxError> {
    let listener = TcpListener::bind(addr).await?;
    eprintln!(
        "llm-relay {} listening on http://{addr} → {}",
        env!("CARGO_PKG_VERSION"),
        state.cfg.upstream
    );

    // Not awaited: the listener must be accepting immediately, and the
    // handshake is useful whenever it lands.
    let warming = Arc::clone(&state);
    tokio::spawn(async move { crate::relay::prewarm(&warming).await });

    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    builder.http2().timer(TokioTimer::new());

    let graceful = GracefulShutdown::new();
    let mut shutdown = std::pin::pin!(shutdown_signal());

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _peer) = match accepted {
                    Ok(pair) => pair,
                    // A per-connection accept failure (fd exhaustion, a reset
                    // between queue and accept) must not take the listener down.
                    Err(e) => { eprintln!("accept failed: {e}"); continue }
                };
                // SSE frames are small and latency-critical: never wait on Nagle.
                let _ = stream.set_nodelay(true);

                let state = Arc::clone(&state);
                let conn = builder.serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |req| route(req, Arc::clone(&state))),
                );
                let conn = graceful.watch(conn.into_owned());
                tokio::spawn(async move {
                    if let Err(e) = conn.await {
                        // A client that hangs up mid-stream lands here; it is
                        // ordinary traffic, not a fault of this process.
                        eprintln!("connection closed: {e}");
                    }
                });
            }
            _ = shutdown.as_mut() => break,
        }
    }

    eprintln!(
        "shutdown signal received; draining for up to {}s",
        SHUTDOWN_GRACE.as_secs()
    );
    tokio::select! {
        _ = graceful.shutdown() => eprintln!("all connections drained"),
        _ = tokio::time::sleep(SHUTDOWN_GRACE) => {
            eprintln!("grace period elapsed; dropping remaining connections");
        }
    }
    Ok(())
}

/// SIGTERM is what `docker stop` sends; Ctrl-C covers a foreground run.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `--healthcheck`: the container has no shell and no curl, so the health probe
/// is this binary calling its own `/healthz` over the loopback.
///
/// Deliberately blocking std sockets and a hand-written request line: the probe
/// runs for the life of the container, and its resident memory counts toward the
/// container's own. Spinning up an async runtime and an HTTP client to read one
/// status line would cost several MiB, on a schedule, forever.
pub fn healthcheck(port: u16) -> Result<(), BoxError> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, HEALTHCHECK_TIMEOUT)?;
    stream.set_read_timeout(Some(HEALTHCHECK_TIMEOUT))?;
    stream.set_write_timeout(Some(HEALTHCHECK_TIMEOUT))?;
    stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;

    // Only the status line matters, but a read is not guaranteed to deliver all
    // of it at once.
    const WANT: usize = 12; // "HTTP/1.1 200"
    let mut buf = [0u8; 64];
    let mut have = 0;
    while have < WANT {
        match stream.read(&mut buf[have..])? {
            0 => return Err(format!("/healthz closed after {have} bytes").into()),
            n => have += n,
        }
    }
    if buf[..WANT].starts_with(b"HTTP/1.1 200") {
        Ok(())
    } else {
        Err(format!(
            "/healthz answered: {}",
            String::from_utf8_lossy(&buf[..have]).trim()
        )
        .into())
    }
}

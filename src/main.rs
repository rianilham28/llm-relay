use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use llm_relay::{Config, State, server};

/// The relay allocates a `Bytes` per SSE frame and a header vector per request,
/// so allocator throughput sits directly on the hot path.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    // Config is resolved before anything binds — and before the runtime is
    // built, since the worker count comes from it — so a bad value fails the
    // container start rather than every request.
    let cfg = match Config::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("configuration error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The probe is synchronous and starts no runtime at all — see
    // `server::healthcheck` for why that matters on a per-GB platform.
    if std::env::args().skip(1).any(|arg| arg == "--healthcheck") {
        return match server::healthcheck(cfg.port) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("healthcheck failed: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.worker_threads)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime failed to start: {e}");
            return ExitCode::FAILURE;
        }
    };

    let addr = SocketAddr::from((cfg.bind, cfg.port));
    let state = Arc::new(State::new(cfg));
    match rt.block_on(server::serve(state, addr)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fatal: {e}");
            ExitCode::FAILURE
        }
    }
}

//! `web-dl` entry point: CLI -> Config -> load queue -> start server.
mod config;
mod events;
mod library;
mod parse;
mod persist;
mod render;
mod server;
mod state;
mod thumb;
mod worker;
mod ytdlp;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::signal;

use crate::config::Cli;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.verbose);

    let cfg = cli.into_config()?;

    // Validate yt-dlp is callable before binding.
    if let Err(e) = config::yt_dlp_check(&cfg.yt_dlp) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    // ffmpeg is only used for thumbnail generation (best-effort), so a missing
    // binary is a warning, not fatal.
    if let Err(e) = config::ffmpeg_check(&cfg.ffmpeg) {
        tracing::warn!("{e}");
    }

    tracing::info!(
        download_dir = %cfg.download_dir.display(),
        state_dir = %cfg.state_dir.display(),
        addr = %cfg.addr,
        cookies = cfg.cookies_from_browser.as_deref().unwrap_or("none"),
        "web-dl starting"
    );

    // Load persisted queue (restart requeue: active -> pending).
    let queue = persist::load(&cfg.state_dir, &cfg.cache_dir).await?;
    let pending = queue
        .items
        .iter()
        .filter(|i| i.status == state::ItemStatus::Pending)
        .count();
    if pending > 0 {
        tracing::info!("{pending} pending item(s) will be re-started");
    }

    let state = AppState::new(cfg.clone(), queue);

    // Clone for the graceful-shutdown future (state itself is used after serve).
    let state_clone = state.clone();

    // Spawn the single background worker.
    let worker_handle = server::spawn_worker(state.clone());

    // Build router.
    let app = server::router(state.clone());

    // Bind listener.
    let listener = TcpListener::bind(&cfg.addr).await?;
    tracing::info!(addr = %cfg.addr, "listening");

    // Graceful shutdown: SIGINT / SIGTERM trip the global shutdown token.
    let shutdown_token = state.shutdown.clone();
    let timeout = cfg.timeout;
    let shutdown_signal = async move {
        let ctrl_c = async {
            let _ = signal::ctrl_c().await;
        };
        #[cfg(unix)]
        let term = async {
            if let Ok(mut s) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
                s.recv().await;
            }
        };
        #[cfg(not(unix))]
        let term = std::future::pending::<()>();

        // Optional self-termination after N seconds (testing aid).
        let timer = async {
            match timeout {
                Some(secs) => {
                    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                }
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            _ = ctrl_c => {}
            _ = term => {}
            _ = timer => tracing::info!(?timeout, "--timeout expired, shutting down"),
        }
        tracing::info!("shutdown signal received");
        shutdown_token.cancel();
    };
    tokio::spawn(shutdown_signal);

    // Serve with graceful shutdown. In-flight SSE streams select against the
    // shutdown token, so they close promptly.
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        state_clone.shutdown.cancelled().await;
    });

    serve.await?;

    // Wait for the worker to finish its shutdown handling: it kills the
    // active yt-dlp, flips that item to `Pending` (so it re-starts on next
    // launch -- see DESIGN Sec. 8), emits final events, and persists. Awaiting
    // here avoids a race where the final flush below would otherwise land
    // before the flip and leave the item serialized as `active`.
    let _ = worker_handle.await;

    // Final flush: write the queue so pending items (and a re-queued active
    // item) survive the restart.
    tracing::info!("flushing queue on shutdown");
    let pending = {
        let q = state.queue.lock().await;

        q.items
            .iter()
            .filter(|i| i.status == state::ItemStatus::Pending)
            .count()
    };
    state.persist().await;
    tracing::info!("{pending} item(s) queued for re-start; bye");

    Ok(())
}

/// Initialise the `tracing` subscriber.
///
/// Under systemd (journald reachable) logs go to the journal with native
/// priorities, so `journalctl -p err` / `-p warning` filter by level. When
/// journald is unavailable (e.g. run in a terminal) it falls back to a
/// human-readable stderr formatter.
///
/// Filter: default `info`; `-v` bumps to `web_dl=debug,info`; `RUST_LOG` is
/// honoured when set explicitly (see DESIGN Sec. 3).
fn init_tracing(verbose: bool) {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter = if std::env::var_os("RUST_LOG").is_some() {
        EnvFilter::from_default_env()
    } else if verbose {
        EnvFilter::new("web_dl=debug,info")
    } else {
        EnvFilter::new("info")
    };

    // `tracing_journald::layer()` opens the journal socket; it errors when
    // journald isn't reachable (non-systemd Linux, non-Linux), in which case
    // we fall back to a stderr formatter.
    let journald = tracing_journald::layer().ok();
    let stderr = if journald.is_none() {
        // Only colorise when stderr is a real terminal; under `nohup`, a
        // pipe, or systemd's `StandardError=journal` we emit plain text so
        // escape codes don't land in the journal/log file.
        let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
        Some(fmt::layer().with_target(false).with_ansi(ansi))
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(journald)
        .with(stderr)
        .init();
}

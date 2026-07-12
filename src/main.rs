//! `web-dl` entry point: CLI -> Config -> load queue -> start server.
#![allow(dead_code)]
mod config;
mod events;
mod library;
mod parse;
mod persist;
mod render;
mod server;
mod state;
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

    // Logging: default info, `-v` bumps to web_dl=debug, RUST_LOG honored if set.
    let filter = if std::env::var_os("RUST_LOG").is_some() {
        env_logger::Env::default().default_filter_or("info")
    } else if cli.verbose {
        env_logger::Env::default().default_filter_or("web_dl=debug,info")
    } else {
        env_logger::Env::default().default_filter_or("info")
    };
    let _ = env_logger::Builder::from_env(filter)
        .format_timestamp_secs()
        .try_init();

    let cfg = cli.into_config()?;

    // Validate yt-dlp is callable before binding.
    if let Err(e) = config::yt_dlp_check(&cfg.yt_dlp) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    log::info!(
        "web-dl starting: download_dir={} state_file={} addr={} cookies={}",
        cfg.download_dir.display(),
        cfg.state_file.display(),
        cfg.addr,
        cfg.cookies_browser.as_deref().unwrap_or("none"),
    );

    // Load persisted queue (restart requeue: active -> pending).
    let queue = persist::load(&cfg.state_file).await?;
    let pending = queue
        .items
        .iter()
        .filter(|i| i.status == state::ItemStatus::Pending)
        .count();
    if pending > 0 {
        log::info!("{pending} pending item(s) will be re-started");
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
    log::info!("listening on http://{}", cfg.addr);

    // Graceful shutdown: SIGINT / SIGTERM trip the global shutdown token.
    let shutdown_token = state.shutdown.clone();
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

        tokio::select! {
            _ = ctrl_c => {}
            _ = term => {}
        }
        log::info!("shutdown signal received");
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
    log::info!("flushing queue on shutdown");
    let pending = {
        let q = state.queue.lock().await;
        let n = q
            .items
            .iter()
            .filter(|i| i.status == state::ItemStatus::Pending)
            .count();
        n
    };
    state.persist().await;
    log::info!("{pending} item(s) queued for re-start; bye");

    Ok(())
}

//! CLI parsing + resolved `Config`.
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::fs;


/// `web-dl` -- a single-binary web wrapper around `yt-dlp`.
#[derive(Parser, Debug)]
#[command(name = "web-dl", version, about)]
pub struct Cli {
    /// Where yt-dlp writes files (passed as -P). Default: ~/Downloads
    #[arg(short, long, value_name = "DIR")]
    pub download_dir: Option<String>,

    /// Browser to pull cookies from via --cookies-from-browser. Default: firefox.
    /// Use "none" to disable cookies entirely.
    #[arg(short = 'b', long = "cookies-from-browser", value_name = "BROWSER", default_value = "firefox")]
    pub cookies_from_browser: String,

    /// Path to yt-dlp binary. Default: yt-dlp (PATH).
    #[arg(long, value_name = "PATH", default_value = "yt-dlp")]
    pub yt_dlp: String,

    /// Queue persistence directory (one JSON file per item). Default:
    /// ~/.local/share/web-dl/queue/
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<String>,

    /// Bind address (host:port). Default: 127.0.0.1:8080 (loopback). Use
    /// 0.0.0.0:<port> to listen on all interfaces -- DANGEROUS; prints a warning.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8080")]
    pub bind: String,

    /// Verbose server logs (web_dl=debug).
    #[arg(short, long)]
    pub verbose: bool,

    /// Shut the server down after N seconds. Intended for testing so the
    /// binary self-terminates without an external kill; unset by default.
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<u64>,
}

/// Resolved runtime configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub download_dir: PathBuf,
    pub cookies_from_browser: Option<String>,
    pub yt_dlp: String,
    pub state_dir: PathBuf,
    pub addr: SocketAddr,
    pub timeout: Option<u64>,
}

impl Cli {
    /// Resolve CLI args into a `Config`, expanding `~`, creating directories,
    /// and validating the listen address.
    pub fn into_config(self) -> Result<Config> {
        let home = home_dir().context("could not determine home directory")?;

        let download_dir = match self.download_dir {
            Some(d) => expand_tilde(&d, &home),
            None => home.join("Downloads"),
        };
        fs::create_dir_all(&download_dir).with_context(|| {
            format!("failed to create download dir: {}", download_dir.display())
        })?;

        let cookies_from_browser = if self.cookies_from_browser.eq_ignore_ascii_case("none") {
            None
        } else {
            Some(self.cookies_from_browser.clone())
        };

        let state_dir = match self.state_dir {
            Some(s) => expand_tilde(&s, &home),
            None => home.join(".local").join("share").join("web-dl").join("queue"),
        };
        fs::create_dir_all(&state_dir)
            .with_context(|| format!("failed to create state dir: {}", state_dir.display()))?;

        let addr: SocketAddr = self
            .bind
            .parse()
            .with_context(|| format!("invalid bind address: {}", self.bind))?;

        // Loud warning when binding anything other than loopback. Printed to
        // stderr so it's visible even if the tracing subscriber failed to init;
        // also emitted via tracing so it lands in journald with a WARN priority.
        if !addr.ip().is_loopback() {
            eprintln!(
                "WARNING: --bind {bind} listens on a non-loopback interface. Anyone who can \
                 reach this machine can run yt-dlp with your browser cookies, download any \
                 file in {dir}, and delete files. Do not expose to untrusted networks.",
                bind = self.bind,
                dir = download_dir.display(),
            );
            tracing::warn!(
                bind = %self.bind,
                "listening on a non-loopback interface; anyone reachable can run yt-dlp with your cookies, read/delete files in the download dir"
            );
        }

        Ok(Config {
            download_dir,
            cookies_from_browser,
            yt_dlp: self.yt_dlp,
            state_dir,
            addr,
            timeout: self.timeout,
        })
    }
}

/// Resolve a home directory, preferring `$HOME`, falling back to the `dirs` crate.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    dirs::home_dir()
}

/// Expand a leading `~` to `home`.
fn expand_tilde(input: &str, home: &Path) -> PathBuf {
    if input == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(input)
}

use anyhow::{Context, Result, bail};
use clap::Parser;

/// Quick helper used by `main` to validate the yt-dlp binary is callable.
pub fn yt_dlp_check(yt_dlp: &str) -> Result<()> {
    let out = std::process::Command::new(yt_dlp).arg("--version").output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            bail!("`{yt_dlp} --version` exited non-zero: {stderr}");
        }
        Err(e) => {
            bail!(
                "could not run `{yt_dlp} --version`: {e}.\n\
                 Install yt-dlp or pass --yt-dlp <path>."
            );
        }
    }
}

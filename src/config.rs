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

    /// Queue persistence file. Default: ~/.local/share/web-dl/queue.json
    #[arg(long, value_name = "PATH")]
    pub state_file: Option<String>,

    /// Listen address. Default: 127.0.0.1:8080.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8080")]
    pub addr: String,

    /// Bind host/interface. Default: 127.0.0.1 (loopback). Use 0.0.0.0 to listen
    /// on all interfaces -- DANGEROUS; prints a warning. Overrides the host
    /// portion of --addr.
    #[arg(long, value_name = "HOST", default_value = "127.0.0.1")]
    pub bind: String,

    /// Verbose server logs (web_dl=debug).
    #[arg(short, long)]
    pub verbose: bool,
}

/// Resolved runtime configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub download_dir: PathBuf,
    pub cookies_from_browser: Option<String>,
    pub yt_dlp: String,
    pub state_file: PathBuf,
    pub addr: SocketAddr,
    pub bind: String,
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

        let state_file = match self.state_file {
            Some(s) => expand_tilde(&s, &home),
            None => {
                let p = home.join(".local").join("share").join("web-dl").join("queue.json");
                p
            }
        };
        if let Some(parent) = state_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create state dir: {}", parent.display()))?;
        }

        // The listen host comes from --bind (default loopback); the port comes
        // from --addr. So --bind overrides only the host portion of --addr.
        let port = self
            .addr
            .rsplit(':')
            .next()
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or("8080");
        let addr_str = format!("{}:{port}", self.bind);
        let addr: SocketAddr = addr_str
            .parse()
            .with_context(|| format!("invalid listen address: {addr_str}"))?;

        // Loud warning when binding anything other than loopback. Printed to
        // stderr so it's visible even if the tracing subscriber failed to init;
        // also emitted via tracing so it lands in journald with a WARN priority.
        let loopback = matches!(self.bind.as_str(), "127.0.0.1" | "::1" | "localhost");
        if !loopback {
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
            state_file,
            addr,
            bind: self.bind,
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

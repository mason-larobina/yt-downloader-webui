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
    #[arg(short, long, value_name = "BROWSER", default_value = "firefox")]
    pub cookies_browser: String,

    /// Path to yt-dlp binary. Default: yt-dlp (PATH).
    #[arg(long, value_name = "PATH", default_value = "yt-dlp")]
    pub yt_dlp: String,

    /// Queue persistence file. Default: ~/.local/share/web-dl/queue.json
    #[arg(long, value_name = "PATH")]
    pub state_file: Option<String>,

    /// Listen address. Default: 127.0.0.1:8080.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8080")]
    pub addr: String,

    /// Bind 0.0.0.0 instead of loopback. DANGEROUS; prints a warning.
    #[arg(long)]
    pub bind_all: bool,

    /// Verbose server logs (web_dl=debug).
    #[arg(short, long)]
    pub verbose: bool,
}

/// Resolved runtime configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub download_dir: PathBuf,
    pub cookies_browser: Option<String>,
    pub yt_dlp: String,
    pub state_file: PathBuf,
    pub addr: SocketAddr,
    pub bind_all: bool,
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

        let cookies_browser = if self.cookies_browser.eq_ignore_ascii_case("none") {
            None
        } else {
            Some(self.cookies_browser.clone())
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

        let addr_str = if self.bind_all {
            // Replace host portion with 0.0.0.0, preserving port.
            let port = self
                .addr
                .rsplit(':')
                .next()
                .unwrap_or("8080")
                .to_string();
            format!("0.0.0.0:{port}")
        } else {
            self.addr.clone()
        };
        let addr: SocketAddr = addr_str
            .parse()
            .with_context(|| format!("invalid listen address: {addr_str}"))?;
        if self.bind_all {
            // Loud warning, printed to stderr so it's visible even with logging off.
            eprintln!(
                "WARNING: --bind-all binds 0.0.0.0. Anyone who can reach this \
                 machine can run yt-dlp with your Firefox cookies, download any \
                 file in {}, and delete files. Do not expose to untrusted networks.",
                download_dir.display()
            );
        }

        Ok(Config {
            download_dir,
            cookies_browser,
            yt_dlp: self.yt_dlp,
            state_file,
            addr,
            bind_all: self.bind_all,
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

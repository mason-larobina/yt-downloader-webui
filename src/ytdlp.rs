//! Build the `yt-dlp` `Command` for a single queued URL.
use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

/// The command built per DESIGN Sec. 4:
///
/// ```text
/// yt-dlp --cookies-from-browser <b> --newline \
///        --progress-template '%(progress)j' -P <dir> <URL>
/// ```
///
/// URLs are passed as a single `Command::arg`, never concatenated into a
/// shell string -> no command injection.
pub fn build(yt_dlp: &str, browser: Option<&str>, download_dir: &Path, url: &str) -> Command {
    let mut cmd = Command::new(yt_dlp);
    if let Some(b) = browser {
        cmd.arg("--cookies-from-browser").arg(b);
    }
    cmd.arg("--newline");
    cmd.arg("--progress-template").arg("%(progress)j");
    cmd.arg("-P").arg(download_dir);
    cmd.arg(url);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    cmd
}

/// Build the `yt-dlp` command that classifies a submitted URL without
/// downloading anything: `--flat-playlist -j` prints one JSON line per
/// playlist entry (a playlist) or one big video dict (a single video).
///
/// ```text
/// yt-dlp --cookies-from-browser <b> --flat-playlist -j --no-progress <URL>
/// ```
///
/// The worker reads stdout line-by-line: each playlist entry (`_type:"url"`)
/// becomes a pending per-video `Video` item (its `url` is already the full
/// watch URL); a single-video dict triggers a direct download of the
/// original URL. `--flat-playlist` on a non-playlist URL is a no-op (yt-dlp
/// fully extracts the video), so single videos cost a second extraction at
/// download time -- accepted for robustness (no URL-shape guessing).
///
/// No `-P`/`--progress-template`: this process never writes files or emits
/// progress ticks. Cookies are reused so YouTube (which 403s anonymous,
/// cookie-less requests) actually responds on the operator's machine.
pub fn build_probe(yt_dlp: &str, browser: Option<&str>, url: &str) -> Command {
    let mut cmd = Command::new(yt_dlp);
    if let Some(b) = browser {
        cmd.arg("--cookies-from-browser").arg(b);
    }
    cmd.arg("--flat-playlist");
    cmd.arg("-j");
    cmd.arg("--no-progress");
    cmd.arg(url);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    cmd
}

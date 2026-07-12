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

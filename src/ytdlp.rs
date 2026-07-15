//! Build the `yt-dlp` `Command` for a single queued URL.
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

/// The command built per ARCHITECTURE Sec. 4:
///
/// ```text
/// yt-dlp --cookies-from-browser <b> --newline \
///        --progress-template '%(progress)j' \
///        --print-to-file 'after_move:{"status":"after_move","filename":%(filepath)j}' <sidefile> \
///        -P <dir> <URL>
/// ```
///
/// URLs are passed as a single `Command::arg`, never concatenated into a
/// shell string -> no command injection.
///
/// Format selection prefers mp4 video + m4a audio at up to 1080p, falling
/// back to best mp4/m4a pair at any height, then best single-file mp4, then
/// best anything (yt-dlp picks whatever streams the site offers).
/// `--merge-output-format mp4` ensures merged containers are mp4 even when the
/// best available video/audio come as separate non-mp4 streams.
///
/// `--print-to-file after_move:…` writes one JSON line to `sidefile` with the
/// **actual final on-disk path** after all post-processing (merge, remux,
/// move). It fires exactly once, on success, for *every* download shape
/// -- including a fresh merge (where progress-tick `filename` only ever names
/// an intermediate `.fNNN.*` stream) and an already-downloaded file (where
/// yt-dlp emits *zero* progress JSON). The worker reads it after the child
/// exits to set the authoritative filename, superseding the fragile
/// `[Merger] Merging formats into` / `has already been downloaded` log scrapes
/// (see `worker::read_after_move_filename`). Unlike `--print`, `--print-to-file`
/// does **not** imply `--quiet`/`--simulate`, so the live progress JSON and text
/// logs keep flowing to stdout/stderr unchanged.
pub fn build(
    yt_dlp: &str,
    browser: Option<&str>,
    download_dir: &Path,
    sidefile: &Path,
    url: &str,
) -> Command {
    let mut cmd = Command::new(yt_dlp);
    if let Some(b) = browser {
        cmd.arg("--cookies-from-browser").arg(b);
    }
    cmd.arg("--newline");
    cmd.arg("--progress-template").arg("%(progress)j");
    cmd.arg("--print-to-file")
        .arg(r#"after_move:{"status":"after_move","filename":%(filepath)j}"#)
        .arg(sidefile);
    cmd.arg("-P").arg(download_dir);
    cmd.arg("-f").arg(format_selector());
    cmd.arg("--merge-output-format").arg("mp4");
    cmd.arg(url);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    cmd
}

/// yt-dlp `-f` format selector: prefer mp4 video + m4a audio at up to
/// 1080p, then progressively relax constraints so we always fall back to
/// whatever the site offers rather than failing.
///
/// Order of alternatives (first match wins):
/// 1. best mp4 video ≤1080p + best m4a audio
/// 2. best mp4 video (any height) + best m4a audio
/// 3. best single progressive mp4 ≤1080p
/// 4. best single progressive mp4 (any height)
/// 5. best anything (last resort)
fn format_selector() -> &'static str {
    "bv*[height<=1080][ext=mp4]+ba[ext=m4a]\
     /bv*[ext=mp4]+ba[ext=m4a]\
     /b[height<=1080][ext=mp4]\
     /b[ext=mp4]\
     /b"
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

/// The per-item path passed to `--print-to-file` so yt-dlp writes the
/// authoritative `after_move` filename to a sidecar the worker reads after
/// the child exits. Lives in the system temp dir (transient: created by
/// yt-dlp, read + deleted by the worker immediately after the child exits);
/// keyed by `item_id` so concurrent items never collide. The worker removes
/// any stale file at this path *before* spawning yt-dlp, since yt-dlp opens it
/// in append mode and would otherwise accumulate lines across retries of the
/// same item id within a session.
pub fn after_move_sidefile(item_id: u64) -> PathBuf {
    std::env::temp_dir().join(format!("yt-dl-webui-{item_id}-after-move.json"))
}

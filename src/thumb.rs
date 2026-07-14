//! Thumbnail cache: native (high-resolution) frames extracted from a
//! downloaded/imported video file with ffmpeg. Files live in `cfg.cache_dir`
//! (XDG cache home by default) and are keyed by sha1 of the video's basename:
//! `<sha1>.<i>.jpg`. The directory is a pure cache: safe to clear; anything
//! missing is re-generated on the next download completion or import.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha1::{Digest, Sha1};

/// Resolve a bare thumbnail filename to a path inside `cache_dir`, guarding
/// against traversal exactly like `library::resolve_safe`. Returns `None` for
/// unsafe names (any `/`, `..`, leading `.`) or a name that does not resolve
/// inside `cache_dir`.
pub fn resolve(cache_dir: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == ".."
        || name.contains('\0')
        || name.starts_with('.')
    {
        return None;
    }
    let target = cache_dir.join(name);
    // Canonicalize the parent so we don't require the file to exist (it may
    // not yet -- but for /thumb serving it does); if it exists, canonicalize
    // the target and assert it stays under the cache dir.
    let dir_canon = cache_dir.canonicalize().ok()?;
    let target_canon = target.canonicalize().ok()?;
    if target_canon.starts_with(&dir_canon) {
        Some(target_canon)
    } else {
        None
    }
}

/// sha1 hex of `s`.
fn sha1_hex(s: &str) -> String {
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    hex(&h.finalize())
}

/// Lowercase hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Maximum number of frames generated for a single video. `ln(duration)`
/// grows slowly but is uncapped; cap so a 24h recording doesn't spawn dozens
/// of ffmpeg passes.
const MAX_FRAMES: usize = 12;

/// How many native frames to extract for a video of `duration` seconds.
/// `floor(ln(duration)) + 1`, clamped to `[1, MAX_FRAMES]`. A short clip (<3s,
/// `ln < 1`) yields 1 frame; a 10-min video yields 7; a 1-hour video yields 9.
/// Returns 1 for unknown / non-positive durations.
fn frame_count(duration: Option<f64>) -> usize {
    let d = duration.filter(|d| *d > 0.0).unwrap_or(0.0);
    if d <= 0.0 {
        return 1;
    }
    let n = (d.ln().floor() as i64 + 1).max(1) as usize;
    n.clamp(1, MAX_FRAMES)
}

/// Generate native (high-resolution) thumbnails from `video_path` with ffmpeg
/// and return the cache filenames (`<sha1(basename)>.<i>.jpg`). The number of
/// frames is `floor(ln(duration)) + 1` (clamped), evenly spaced at
/// `t = i / N * duration` for `i in 0..N` -- a logarithmic count so longer
/// videos get proportionally (but slowly) more frames without spamming ffmpeg.
///
/// Frames are extracted at native resolution (no downscale) with good jpeg
/// quality (`-q:v 2`); each frame is a separate ffmpeg pass with an input
/// `-ss` seek (fast, keyframe-accurate enough for thumbnails). A frame already
/// present in the cache is reused (so a partial run resumes without re-extracting).
///
/// Best-effort: errors abort the run and return whatever frames landed so far
/// (the caller treats a partial set as better than none).
pub async fn generate_native(
    ffmpeg: &str,
    cache_dir: &Path,
    video_path: &Path,
    duration: Option<f64>,
) -> Result<Vec<String>> {
    let base = video_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video");
    let stem = sha1_hex(base);
    let n = frame_count(duration);
    let d = duration.filter(|d| *d > 0.0).unwrap_or(0.0);

    let mut landed: Vec<String> = Vec::with_capacity(n);
    for i in 0..n {
        let name = format!("{stem}.{i}.jpg");
        // Resume: reuse a frame already in the cache (across re-runs / partial).
        if cache_dir.join(&name).is_file() {
            landed.push(name);
            continue;
        }
        // Evenly spaced: t = i / N * duration. Clamp the first frame to a tiny
        // offset so a black intro frame at t=0 isn't the primary, while still
        // honouring the even-spacing formula.
        let t = if n == 1 {
            // Single frame: seek 1s in (skip black intro) capped to duration/2.
            (d / 2.0).min(1.0).max(0.1)
        } else {
            let raw = i as f64 * d / n as f64;
            if i == 0 {
                raw.max(0.1)
            } else {
                raw
            }
        };
        match extract_frame(ffmpeg, cache_dir, video_path, &name, t).await {
            Ok(()) => landed.push(name),
            Err(e) => {
                tracing::debug!(
                    "ffmpeg frame {i} (t={t:.1}s) failed for {}: {e:#}",
                    video_path.display()
                );
                // Continue to the next frame rather than aborting the whole set;
                // a partial gallery is better than none.
            }
        }
    }
    if landed.is_empty() {
        anyhow::bail!(
            "no thumbnail frames extracted for {}",
            video_path.display()
        );
    }
    Ok(landed)
}

/// Extract one frame at timestamp `t` (seconds) from `video_path` into
/// `cache_dir/<out_name>` as a high-quality jpeg at native resolution.
async fn extract_frame(
    ffmpeg: &str,
    cache_dir: &Path,
    video_path: &Path,
    out_name: &str,
    t: f64,
) -> Result<()> {
    let out_path = cache_dir.join(out_name);
    // -y overwrite, -ss before -i (fast keyframe seek), -frames:v 1 single
    // frame, -q:v 2 high jpeg quality, no scale (native resolution).
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-y")
        .arg("-ss")
        .arg(format!("{t:.3}"))
        .arg("-i")
        .arg(video_path)
        .arg("-frames:v")
        .arg("1")
        .arg("-q:v")
        .arg("2")
        .arg(&out_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = cmd
        .output()
        .await
        .with_context(|| format!("spawning ffmpeg for {}", video_path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "ffmpeg exited ({}) for {} at t={t:.1}s: {}",
            output.status,
            video_path.display(),
            stderr.trim()
        );
    }
    if !out_path.is_file() {
        anyhow::bail!(
            "ffmpeg reported success but wrote no frame for {} at t={t:.1}s",
            video_path.display()
        );
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_hex_matches_reference() {
        // sha1("hello") = aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d
        assert_eq!(
            sha1_hex("hello"),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        );
    }

    #[test]
    fn sha1_hex_is_lowercase_hex_40() {
        let h = sha1_hex("https://i.ytimg.com/vi/abc/hqdefault.jpg");
        assert_eq!(h.len(), 40);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(h, h.to_ascii_lowercase());
    }

    #[test]
    fn frame_count_scales_with_duration() {
        // Unknown / non-positive duration -> 1 frame.
        assert_eq!(frame_count(None), 1);
        assert_eq!(frame_count(Some(0.0)), 1);
        assert_eq!(frame_count(Some(-1.0)), 1);
        // ln(2)≈0.69 -> floor 0 +1 = 1.
        assert_eq!(frame_count(Some(2.0)), 1);
        // ln(60)≈4.09 -> 5.
        assert_eq!(frame_count(Some(60.0)), 5);
        // ln(600)≈6.40 -> 7.
        assert_eq!(frame_count(Some(600.0)), 7);
        // Capped at MAX_FRAMES for very long durations.
        assert_eq!(frame_count(Some(86400.0)), MAX_FRAMES);
    }

    #[test]
    fn resolve_rejects_traversal() {
        let dir = tempfile_dir();
        // a real file inside the cache dir resolves
        std::fs::write(dir.join("ok.jpg"), b"x").unwrap();
        assert!(resolve(&dir, "ok.jpg").is_some());
        // unsafe names never resolve
        assert!(resolve(&dir, "../etc/passwd").is_none());
        assert!(resolve(&dir, "..\\x").is_none());
        assert!(resolve(&dir, ".hidden").is_none());
        assert!(resolve(&dir, "a/b").is_none());
        assert!(resolve(&dir, "").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fresh temp dir for filesystem helper tests.
    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "yt-downloader-webui-thumb-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// End-to-end: ffmpeg synthesises a 2s colour clip, then
    /// `thumb::generate_native` extracts `frame_count(2.0)` frames (1 frame for
    /// a 2s clip) into the cache. Ignored by default (needs ffmpeg on PATH);
    /// run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn generate_extracts_frame_from_synthetic_video() {
        let cache = tempfile_dir();
        let video = cache.join("clip.mp4");
        // Generate a 2s red clip. ln(2)≈0.69 -> floor=0 -> 1 frame.
        let out = tokio::process::Command::new("ffmpeg")
            .arg("-y")
            .args(["-f", "lavfi", "-i", "color=c=red:s=320x240:d=2"])
            .arg(&video)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .expect("ffmpeg synth");
        assert!(out.status.success(), "ffmpeg synth failed: {out:?}");
        assert!(video.is_file());

        let names = generate_native("ffmpeg", &cache, &video, Some(2.0))
            .await
            .expect("generate_native");
        assert_eq!(names.len(), 1, "2s clip -> 1 frame");
        assert_eq!(names[0], format!("{}.0.jpg", sha1_hex("clip.mp4")));
        let thumb = cache.join(&names[0]);
        assert!(
            thumb.is_file(),
            "thumbnail not written at {}",
            thumb.display()
        );
        assert!(thumb.metadata().unwrap().len() > 0);
        // A second call reuses the cached frame (no ffmpeg re-run).
        let names2 = generate_native("ffmpeg", &cache, &video, Some(2.0))
            .await
            .expect("generate_native2");
        assert_eq!(names, names2);

        std::fs::remove_dir_all(&cache).ok();
    }
}

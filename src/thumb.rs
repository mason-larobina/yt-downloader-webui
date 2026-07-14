//! Thumbnail cache: remote thumbnails fetched during the yt-dlp probe are the
//! *primary* thumbnail (highest quality); when none is available a native
//! (high-resolution) frame extracted from the downloaded file by ffmpeg is
//! used as a fallback. The native frames also form the item-page gallery.
//!
//! Files live in `cfg.cache_dir` (XDG cache home by default) and are keyed by
//! sha1 of their source: the thumbnail URL for fetched thumbs (`<sha1>.<ext>`),
//! the video basename for ffmpeg-generated frames (`<sha1>.<i>.jpg`). The
//! directory is a pure cache: safe to clear; fetched thumbs are re-fetched on
//! the next probe and native frames re-generated on the next download /
//! import.
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

/// Pick a file extension for a fetched thumbnail: prefer the response
/// Content-Type, falling back to the URL's path extension, then `.jpg`.
fn ext_for(content_type: Option<&str>, url: &str) -> &'static str {
    if let Some(ct) = content_type {
        let ct = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match ct.as_str() {
            "image/jpeg" | "image/jpg" => return "jpg",
            "image/png" => return "png",
            "image/webp" => return "webp",
            "image/gif" => return "gif",
            _ => {}
        }
    }
    // Fall back to the URL's extension.
    let path = url.split('?').next().unwrap_or(url);
    if let Some(ext) = path.rsplit('.').next() {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => return "jpg",
            "png" => return "png",
            "webp" => return "webp",
            "gif" => return "gif",
            _ => {}
        }
    }
    "jpg"
}

/// Fetch a thumbnail `url` into `cache_dir` and return the cache filename
/// (`<sha1(url)>.<ext>`). A cache hit (file already present) skips the network.
/// Best-effort: errors are returned to the caller, which treats them as
/// non-fatal (a native frame extracted by ffmpeg after the download acts as
/// the fallback primary instead).
pub async fn fetch(client: &reqwest::Client, cache_dir: &Path, url: &str) -> Result<String> {
    let stem = sha1_hex(url);
    // Cache hit: reuse the existing file for this URL (across known image
    // extensions) so a repeat probe skips the network.
    if let Some(hit) = cache_hit(cache_dir, &stem) {
        return Ok(hit);
    }

    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching thumbnail {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("thumbnail {url} returned HTTP {}", resp.status());
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let ext = ext_for(content_type.as_deref(), url);
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading thumbnail body {url}"))?;
    let fname = format!("{}.{}", stem, ext);
    write_atomic(&cache_dir.join(&fname), &bytes).await?;
    Ok(fname)
}

/// Return the existing cache filename for `stem` if any known image extension
/// is present, so a repeat probe skips the network.
fn cache_hit(cache_dir: &Path, stem: &str) -> Option<String> {
    for ext in ["jpg", "png", "webp", "gif"] {
        let name = format!("{stem}.{ext}");
        if cache_dir.join(&name).is_file() {
            return Some(name);
        }
    }
    None
}

/// Atomically write `bytes` to `path` (`.tmp` + fsync + rename).
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = sibling_tmp(path);
    {
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("creating thumbnail temp {}", tmp.display()))?;
        use tokio::io::AsyncWriteExt;
        f.write_all(bytes).await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming thumbnail into {}", path.display()))?;
    Ok(())
}

/// `<dir>/<name>.ext` -> `<dir>/<name>.ext.tmp` (sibling for atomic rename).
fn sibling_tmp(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("thumb.bin"));
    name.push(".tmp");
    path.with_file_name(name)
}

/// How many native frames to extract for a video of `duration` seconds.
///
/// Ported from the `screens` crate's frame-count formula: two anchors in
/// log2 space — 10s → 2, 3600s (1h) → 16 — i.e. `2 + slope * log2(d/10)`
/// with slope `14 / log2(360) ≈ 1.648`. Using log2 (rather than natural log)
/// keeps the count well-behaved across the durations that actually occur:
/// log grows slowly enough on its own that no upper cap is needed, while log2
/// spreads frames more sensibly than the previous `1.5*ln` curve — a 1-hour
/// video now yields 16 frames (was 13), a 10-min clip 6 (was 10), a day ~24.
///
/// `duration` unknown or non-positive yields 1 (a single fallback frame at
/// t=0 — the gallery offsets need a real duration to space interior points);
/// any positive duration is floored at 2, mirroring `screens`: a one-frame
/// gallery is never useful (the opening frame alone rarely represents the
/// content, so we always sample at least two interior points).
fn frame_count(duration: Option<f64>) -> usize {
    let d = duration.filter(|d| *d > 0.0).unwrap_or(0.0);
    if d <= 0.0 {
        return 1;
    }
    let slope = 14.0 / 360.0f64.log2();
    let raw = 2.0 + slope * (d / 10.0).log2();
    raw.max(2.0).floor() as usize
}

/// Generate native (high-resolution) thumbnails from `video_path` with ffmpeg
/// and return the cache filenames (`<sha1(basename)>.<i>.jpg`). The number of
/// frames is the `screens`-style log2-anchored count (`frame_count`: 10s → 2,
/// 1h → 16), evenly spaced at `t = (i + 1) / (N + 1) * duration` for `i in
/// 0..N` -- a logarithmic count so longer videos get proportionally (but
/// slowly) more frames without spamming ffmpeg, and interior spacing that
/// drops the very start (t=0, often a black intro) and the very end
/// (t=duration, often credits/fade) while keeping the remaining frames at
/// equal intervals.
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
        // Evenly spaced interior points: t = (i + 1) / (N + 1) * duration.
        // This drops the very start (t=0, often a black intro) and the very
        // end (t=duration, often credits/fade) while keeping equal intervals
        // across the remaining frames. For N=1 this naturally lands at the
        // midpoint (d/2).
        let t = (i as f64 + 1.0) * d / (n as f64 + 1.0);
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
        // Unknown / non-positive duration -> 1 frame (single fallback).
        assert_eq!(frame_count(None), 1);
        assert_eq!(frame_count(Some(0.0)), 1);
        assert_eq!(frame_count(Some(-1.0)), 1);
        // Ported from `screens`: `2 + slope*log2(d/10)`, slope=14/log2(360).
        // 10s -> 2 (anchor); anything shorter floors at 2.
        assert_eq!(frame_count(Some(2.0)), 2);
        assert_eq!(frame_count(Some(10.0)), 2);
        // log2(60/10)=log2(6)≈2.585 -> 2 + 1.648*2.585≈6.26 -> 6.
        assert_eq!(frame_count(Some(60.0)), 6);
        // log2(60)≈5.907 -> 2 + 1.648*5.907≈11.74 -> 11.
        assert_eq!(frame_count(Some(600.0)), 11);
        // 1h is the upper anchor -> exactly 16.
        assert_eq!(frame_count(Some(3600.0)), 16);
        // log2(8640)≈13.077 -> 2 + 1.648*13.077≈23.55 -> 23.
        assert_eq!(frame_count(Some(86400.0)), 23);
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
    /// `thumb::generate_native` extracts `frame_count(2.0)` frames (2 frames
    /// for a 2s clip) into the cache. Ignored by default (needs ffmpeg on PATH);
    /// run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn generate_extracts_frame_from_synthetic_video() {
        let cache = tempfile_dir();
        let video = cache.join("clip.mp4");
        // Generate a 2s red clip. ln(2)≈0.69 -> 1.5*0.69≈1.04 -> 2 frames.
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
        assert_eq!(names.len(), 2, "2s clip -> 2 frames");
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

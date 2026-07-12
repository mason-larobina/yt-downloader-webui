//! Thumbnail cache: fetch a thumbnail URL during the yt-dlp probe, or generate
//! one from a downloaded file with ffmpeg. Files live in `cfg.cache_dir` (XDG
//! cache home by default) and are keyed by sha1 of their source -- the
//! thumbnail URL for fetched thumbs, the video filename for ffmpeg-generated
//! ones. The directory is a pure cache: safe to clear; anything missing is
//! re-fetched on the next probe or re-generated on the next download.
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
        let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
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
/// non-fatal (the queue simply renders without a thumbnail, and ffmpeg may
/// generate one after the download instead).
pub async fn fetch(
    client: &reqwest::Client,
    cache_dir: &Path,
    url: &str,
) -> Result<String> {
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
        anyhow::bail!(
            "thumbnail {url} returned HTTP {}",
            resp.status()
        );
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

/// Generate a thumbnail from `video_path` with ffmpeg and return the cache
/// filename (`<sha1(basename)>.jpg`). Seeks 1s in (skipping any black intro),
/// extracts one frame, scales to fit within ~320px wide. Best-effort: errors
/// are returned to the caller and treated as non-fatal.
pub async fn generate(
    ffmpeg: &str,
    cache_dir: &Path,
    video_path: &Path,
) -> Result<String> {
    let base = video_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video");
    let stem = sha1_hex(base);
    // Reuse an existing generation if present.
    if let Some(hit) = cache_hit(cache_dir, &stem) {
        return Ok(hit);
    }
    let out_name = format!("{stem}.jpg");
    let out_path = cache_dir.join(&out_name);

    // -y overwrite, -ss 1 seek 1s (fast, before -i), -frames:v 1 single frame,
    // -q:v 3 good jpeg quality, scale to 320px wide keeping aspect.
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-y")
        .arg("-ss").arg("1")
        .arg("-i").arg(video_path)
        .arg("-frames:v").arg("1")
        .arg("-q:v").arg("3")
        .arg("-vf").arg("scale=320:-2")
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
        anyhow::bail!("ffmpeg exited ({}) for {}: {}", output.status, video_path.display(), stderr.trim());
    }
    if !out_path.is_file() {
        anyhow::bail!(
            "ffmpeg reported success but wrote no thumbnail for {}",
            video_path.display()
        );
    }
    Ok(out_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_hex_matches_reference() {
        // sha1("hello") = aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d
        assert_eq!(sha1_hex("hello"), "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
    }

    #[test]
    fn sha1_hex_is_lowercase_hex_40() {
        let h = sha1_hex("https://i.ytimg.com/vi/abc/hqdefault.jpg");
        assert_eq!(h.len(), 40);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(h, h.to_ascii_lowercase());
    }

    #[test]
    fn ext_for_prefers_content_type() {
        assert_eq!(ext_for(Some("image/jpeg"), "https://x/y?v=1"), "jpg");
        assert_eq!(ext_for(Some("image/png"), "https://x/y"), "png");
        assert_eq!(ext_for(Some("image/webp"), ""), "webp");
        assert_eq!(ext_for(Some("image/gif"), ""), "gif");
        // content-type with params
        assert_eq!(ext_for(Some("image/jpeg; charset=binary"), ""), "jpg");
        // unknown content-type falls back to URL extension
        assert_eq!(ext_for(Some("application/octet-stream"), "https://x/thumb.PNG"), "png");
    }

    #[test]
    fn ext_for_falls_back_to_url_then_jpg() {
        assert_eq!(ext_for(None, "https://x/path/pic.webp?sqp=x"), "webp");
        assert_eq!(ext_for(None, "https://x/path/noext"), "jpg");
        assert_eq!(ext_for(None, ""), "jpg");
    }

    #[test]
    fn cache_hit_finds_existing_extension() {
        let dir = tempfile_dir();
        let stem = "deadbeef";
        std::fs::write(dir.join(format!("{stem}.png"),), b"x").unwrap();
        assert_eq!(cache_hit(&dir, stem), Some(format!("{stem}.png")));
        assert!(cache_hit(&dir, "missing").is_none());
        std::fs::remove_dir_all(&dir).ok();
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
            "web-dl-thumb-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// End-to-end: ffmpeg synthesises a 2s colour clip, then `thumb::generate`
    /// extracts a 1s-in frame into the cache. Ignored by default (needs
    /// ffmpeg on PATH); run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn generate_extracts_frame_from_synthetic_video() {
        let cache = tempfile_dir();
        let video = cache.join("clip.mp4");
        // Generate a 2s red clip so the 1s seek lands on a real frame.
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

        let name = generate("ffmpeg", &cache, &video)
            .await
            .expect("generate");
        assert_eq!(name, format!("{}.jpg", sha1_hex("clip.mp4")));
        let thumb = cache.join(&name);
        assert!(thumb.is_file(), "thumbnail not written at {}", thumb.display());
        assert!(thumb.metadata().unwrap().len() > 0);
        // Cache hit on a second call (no ffmpeg re-run needed).
        let name2 = generate("ffmpeg", &cache, &video).await.expect("generate2");
        assert_eq!(name, name2);

        std::fs::remove_dir_all(&cache).ok();
    }

    /// End-to-end fetch against the real YouTube thumbnail URL captured in the
    /// probe fixture. Ignored by default (needs network); run with
    /// `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn fetch_caches_real_thumbnail() {
        let cache = tempfile_dir();
        let client = reqwest::Client::builder()
            .user_agent("web-dl-test")
            .build()
            .unwrap();
        let url = "https://i.ytimg.com/vi/p8eM3MEd_A4/hqdefault.jpg";
        let name = fetch(&client, &cache, url).await.expect("fetch");
        assert_eq!(name, format!("{}.jpg", sha1_hex(url)));
        let path = cache.join(&name);
        assert!(path.is_file());
        assert!(path.metadata().unwrap().len() > 0);
        // Cache hit on a second call (no network).
        let name2 = fetch(&client, &cache, url).await.expect("fetch2");
        assert_eq!(name, name2);
        std::fs::remove_dir_all(&cache).ok();
    }
}

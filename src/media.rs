//! ffprobe-backed media metadata: identifies a file as a video (it has a
//! video stream) and extracts the display fields (duration, resolution,
//! codecs, bitrate) persisted on each `QueueItem`.
//!
//! Probing is the single source of truth for "is this a video?" during import
//! (an unreferenced file in the download dir is imported only if ffprobe
//! reports a video stream), and the result is stored on the item so we never
//! re-probe the same file. ffprobe is fast (no decoding -- container parsing
//! only), but spawning it per file on every startup would still be wasteful,
//! hence the persisted cache.
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// ffprobe-extracted media metadata for a downloaded/imported file. Stored
/// on the `QueueItem` so the item details page can show codec / resolution /
/// bitrate / duration without re-probing, and so import can decide whether a
/// file is a video (`has_video`) once and reuse the answer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MediaInfo {
    /// Duration in seconds (from `format.duration`). `None` if unknown.
    #[serde(default)]
    pub duration: Option<f64>,
    /// Width of the primary video stream in pixels.
    #[serde(default)]
    pub width: Option<u32>,
    /// Height of the primary video stream in pixels.
    #[serde(default)]
    pub height: Option<u32>,
    /// Video codec name of the primary video stream (e.g. `h264`, `vp9`).
    #[serde(default)]
    pub video_codec: Option<String>,
    /// Audio codec name of the first audio stream (e.g. `aac`, `opus`).
    #[serde(default)]
    pub audio_codec: Option<String>,
    /// Overall bit rate in bits/sec (from `format.bit_rate`).
    #[serde(default)]
    pub bit_rate: Option<u64>,
    /// Frame rate of the primary video stream (e.g. 29.97). `None` if unknown.
    #[serde(default)]
    pub fps: Option<f64>,
    /// Container/format name (e.g. `mov,mp4,m4a,3gp,3g2,mj2`).
    #[serde(default)]
    pub format: Option<String>,
}

impl MediaInfo {
    /// True if this metadata describes a video (a video stream was found).
    pub fn has_video(&self) -> bool {
        self.video_codec.is_some()
    }

    /// `"1920x1080"` if both dimensions are known.
    pub fn resolution_str(&self) -> Option<String> {
        match (self.width, self.height) {
            (Some(w), Some(h)) => Some(format!("{w}x{h}")),
            _ => None,
        }
    }

    /// Human-friendly bitrate, e.g. `2.8 Mb/s`. `None` if unknown.
    pub fn bitrate_str(&self) -> Option<String> {
        let bps = self.bit_rate?;
        let mbps = bps as f64 / 1_000_000.0;
        if mbps >= 1.0 {
            Some(format!("{mbps:.2} Mb/s"))
        } else {
            let kbps = bps / 1000;
            Some(format!("{kbps} kb/s"))
        }
    }
}

/// The slice of ffprobe's JSON output we parse (`-show_streams -show_format`).
/// Fields we don't carry are ignored; everything is `Option`/defaulted.
#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    #[serde(default)]
    format: FfprobeFormat,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeStream {
    #[serde(default)]
    codec_type: Option<String>,
    #[serde(default)]
    codec_name: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    /// avg_frame_rate as a fraction string like "30000/1001".
    #[serde(default)]
    avg_frame_rate: Option<String>,
    /// r_frame_rate as a fraction string (fallback).
    #[serde(default)]
    r_frame_rate: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeFormat {
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    bit_rate: Option<String>,
    #[serde(default)]
    format_name: Option<String>,
}

/// Run `ffprobe` on `path` and return parsed [`MediaInfo`]. Returns an error
/// if the binary can't run or exits non-zero / produces unparseable JSON. A
/// file with no video stream still returns `Ok` (with `video_codec: None`);
/// callers check [`MediaInfo::has_video`] to decide whether to import it.
pub async fn probe(ffprobe: &str, path: &Path) -> Result<MediaInfo> {
    let output = tokio::process::Command::new(ffprobe)
        .arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_streams")
        .arg("-show_format")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawning ffprobe for {}", path.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "ffprobe exited ({}) for {}: {}",
            output.status,
            path.display(),
            stderr.trim()
        );
    }

    let parsed: FfprobeOutput = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parsing ffprobe JSON for {}", path.display()))?;

    Ok(parse(parsed))
}

/// Parse the ffprobe JSON into a [`MediaInfo`]: the first `video` stream is
/// the primary (its codec/width/height/fps); the first `audio` stream gives
/// the audio codec. Duration/bitrate come from the format container.
fn parse(out: FfprobeOutput) -> MediaInfo {
    let video = out
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"));
    let audio = out
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));

    MediaInfo {
        duration: out.format.duration.as_deref().and_then(parse_f64),
        width: video.and_then(|v| v.width),
        height: video.and_then(|v| v.height),
        video_codec: video.and_then(|v| v.codec_name.clone()),
        audio_codec: audio.and_then(|a| a.codec_name.clone()),
        bit_rate: out
            .format
            .bit_rate
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok()),
        fps: video.and_then(|v| {
            parse_fraction(&v.avg_frame_rate).or_else(|| parse_fraction(&v.r_frame_rate))
        }),
        format: out.format.format_name,
    }
}

/// Parse a float that may be `"12.345"` or already numeric-ish.
fn parse_f64(s: &str) -> Option<f64> {
    s.trim()
        .parse::<f64>()
        .ok()
        .filter(|f| f.is_finite() && *f > 0.0)
}

/// Parse a fraction string like `"30000/1001"` -> 29.97. `None` for `0/0`
/// (ffprobe's "unknown" sentinel) or unparseable input.
fn parse_fraction(s: &Option<String>) -> Option<f64> {
    let s = s.as_ref()?;
    if s == "0/0" {
        return None;
    }
    if let Some((num, den)) = s.split_once('/') {
        let n: f64 = num.parse().ok()?;
        let d: f64 = den.parse().ok()?;
        if d == 0.0 {
            return None;
        }
        let v = n / d;
        if v.is_finite() && v > 0.0 {
            return Some(v);
        }
        return None;
    }
    s.parse::<f64>().ok().filter(|f| f.is_finite() && *f > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_picks_video_and_audio_streams() {
        let out = FfprobeOutput {
            streams: vec![
                FfprobeStream {
                    codec_type: Some("audio".into()),
                    codec_name: Some("aac".into()),
                    width: None,
                    height: None,
                    avg_frame_rate: None,
                    r_frame_rate: None,
                },
                FfprobeStream {
                    codec_type: Some("video".into()),
                    codec_name: Some("h264".into()),
                    width: Some(1920),
                    height: Some(1080),
                    avg_frame_rate: Some("30000/1001".into()),
                    r_frame_rate: Some("30000/1001".into()),
                },
            ],
            format: FfprobeFormat {
                duration: Some("600.500".into()),
                bit_rate: Some("2800000".into()),
                format_name: Some("mov,mp4,m4a,3gp,3g2,mj2".into()),
            },
        };
        let m = parse(out);
        assert!(m.has_video());
        assert_eq!(m.video_codec.as_deref(), Some("h264"));
        assert_eq!(m.audio_codec.as_deref(), Some("aac"));
        assert_eq!(m.width, Some(1920));
        assert_eq!(m.height, Some(1080));
        assert_eq!(m.resolution_str().as_deref(), Some("1920x1080"));
        assert!((m.duration.unwrap() - 600.5).abs() < 1e-6);
        assert!((m.fps.unwrap() - 29.97).abs() < 1e-2);
        assert_eq!(m.bit_rate, Some(2_800_000));
        assert_eq!(m.bitrate_str().as_deref(), Some("2.80 Mb/s"));
    }

    #[test]
    fn parse_audio_only_has_no_video() {
        let out = FfprobeOutput {
            streams: vec![FfprobeStream {
                codec_type: Some("audio".into()),
                codec_name: Some("opus".into()),
                width: None,
                height: None,
                avg_frame_rate: None,
                r_frame_rate: None,
            }],
            format: FfprobeFormat::default(),
        };
        let m = parse(out);
        assert!(!m.has_video());
        assert!(m.video_codec.is_none());
    }

    #[test]
    fn parse_fraction_handles_zero_zero_and_garbage() {
        assert!(parse_fraction(&Some("0/0".into())).is_none());
        assert!(parse_fraction(&Some("garbage".into())).is_none());
        assert!(parse_fraction(&None).is_none());
        assert!((parse_fraction(&Some("60/1".into())).unwrap() - 60.0).abs() < 1e-6);
    }

    #[test]
    fn bitrate_str_uses_kb_under_one_mbps() {
        let mut m = MediaInfo::default();
        m.bit_rate = Some(128_000);
        assert_eq!(m.bitrate_str().as_deref(), Some("128 kb/s"));
        m.bit_rate = Some(1_500_000);
        assert_eq!(m.bitrate_str().as_deref(), Some("1.50 Mb/s"));
        m.bit_rate = None;
        assert!(m.bitrate_str().is_none());
    }

    /// End-to-end probe against a synthetic 2s clip. Ignored by default (needs
    /// ffprobe on PATH); run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn probe_synthetic_clip() {
        let dir = std::env::temp_dir().join(format!("media-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        let out = tokio::process::Command::new("ffmpeg")
            .arg("-y")
            .args(["-f", "lavfi", "-i", "color=c=blue:s=640x360:d=2:r=30"])
            .arg(&clip)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .expect("ffmpeg synth");
        assert!(out.status.success(), "ffmpeg synth failed: {out:?}");

        let m = probe("ffprobe", &clip).await.expect("probe");
        assert!(m.has_video());
        assert_eq!(m.width, Some(640));
        assert_eq!(m.height, Some(360));
        assert!((m.duration.unwrap() - 2.0).abs() < 0.1);
        assert!((m.fps.unwrap() - 30.0).abs() < 0.1);

        std::fs::remove_dir_all(&dir).ok();
    }
}

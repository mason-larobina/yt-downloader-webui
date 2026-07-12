//! Classify a single line of yt-dlp stdout/stderr into a structured event.
//!
//! Two line shapes are parsed:
//!   * `--progress-template '%(progress)j'` ticks during a *download*
//!     (one JSON object with a `status` field) -> [`ParsedLine::Progress`].
//!   * `--flat-playlist -j` entry lines during a *probe* (one JSON object per
//!     playlist entry, or one big video dict for a single video) ->
//!     [`ParsedLine::FlatEntry`].
//!
//! Anything else (yt-dlp status lines, `WARNING:`, `ERROR:`) is a log line.
use serde_json::Value;

use crate::state::Progress;

/// Result of classifying one yt-dlp output line.
#[derive(Debug)]
pub enum ParsedLine {
    /// A progress tick parsed from `%(progress)j` JSON (download phase).
    Progress(Progress),
    /// A flat-playlist entry / single-video dict parsed from a `-j` line
    /// (probe phase). Present whenever the line is a JSON object, even if it
    /// is a single full video extraction rather than a playlist entry -- the
    /// caller distinguishes via [`FlatEntry::is_playlist_entry`].
    FlatEntry(FlatEntry),
    /// Anything else (yt-dlp status lines, `WARNING:`, `ERROR:`, etc.).
    Log(String),
}

/// A parsed `--flat-playlist -j` line. Captures only the fields the queue
/// cares about; everything else is dropped.
///
/// For a playlist entry, `_type == "url"` and `url` is already the full
/// per-video watch URL (`https://www.youtube.com/watch?v=<id>`), so the worker
/// can download it directly without reconstruction. For a single video the
/// line is a full video dict (`_type == "video"` or absent) and `url`, if
/// present, is the *media* URL -- the caller must keep the original submitted
/// URL for download, only borrowing `title`/`duration`.
#[derive(Debug, Clone, Default)]
pub struct FlatEntry {
    pub _type: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub id: Option<String>,
    pub duration: Option<f64>,
    pub ie_key: Option<String>,
    pub playlist_index: Option<u64>,
    pub playlist_count: Option<u64>,
    pub playlist_title: Option<String>,
    /// Best-resolution thumbnail URL harvested from the entry's `thumbnails`
    /// array (max width*height), or the top-level `thumbnail` string for a
    /// single-video dict. `None` if the probe JSON carried none.
    pub thumbnail: Option<String>,
}

impl FlatEntry {
    /// True for a playlist entry (`_type:"url"`) carrying a downloadable URL.
    /// These become per-video `Video` queue items.
    pub fn is_playlist_entry(&self) -> bool {
        self._type.as_deref() == Some("url") && self.url.is_some()
    }
}

/// Classify one line of yt-dlp output.
///
/// Per DESIGN Sec. 5: a line that parses as a JSON object containing a
/// `status` field is a progress event; everything else is a log line. The
/// probe phase additionally treats any JSON object (with or without
/// `status`) as a [`FlatEntry`] -- but the download-phase progress JSON also
/// parses as a `FlatEntry`, so this single function returns `FlatEntry` only
/// for objects that are NOT progress (no `status`), keeping `Progress`
/// priority for the download path.
///
/// The probe path calls [`parse_flat_line`] directly (it never sees progress
/// JSON); this `parse_line` stays the download-phase classifier.
pub fn parse_line(line: &str) -> ParsedLine {
    let trimmed = line.trim();
    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(obj) = value.as_object() {
                if obj.contains_key("status") {
                    return ParsedLine::Progress(parse_progress(obj));
                }
                // JSON object without `status`: a flat-playlist line in the
                // probe phase. (The download path never emits such lines.)
                return ParsedLine::FlatEntry(parse_flat_entry(obj));
            }
        }
    }
    ParsedLine::Log(line.to_string())
}

/// Parse a `--flat-playlist -j` line into a [`FlatEntry`]. Returns `None` for
/// non-JSON / non-object lines (the caller treats those as log lines).
pub fn parse_flat_line(line: &str) -> Option<FlatEntry> {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value = serde_json::from_str::<Value>(trimmed).ok()?;
    let obj = value.as_object()?;
    Some(parse_flat_entry(obj))
}

/// Extract a `Progress` from the parsed progress-JSON object.
fn parse_progress(obj: &serde_json::Map<String, Value>) -> Progress {
    let get_str = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let get_num = |k: &str| {
        obj.get(k).and_then(|v| {
            v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
        })
    };
    let get_uint = |k: &str| {
        obj.get(k).and_then(|v| {
            v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))
        })
    };

    Progress {
        status: get_str("status"),
        filename: get_str("filename"),
        tmpfilename: get_str("tmpfilename"),
        downloaded_bytes: get_num("downloaded_bytes"),
        total_bytes: get_num("total_bytes"),
        total_bytes_estimate: get_num("total_bytes_estimate"),
        speed: get_num("speed"),
        eta: get_num("eta"),
        elapsed: get_num("elapsed"),
        fragment_index: get_uint("fragment_index"),
        fragment_count: get_uint("fragment_count"),
        percent: get_num("_percent"),
        percent_str: get_str("_percent_str"),
        speed_str: get_str("_speed_str"),
        eta_str: get_str("_eta_str"),
    }
}

/// Extract a [`FlatEntry`] from a parsed `--flat-playlist -j` JSON object.
fn parse_flat_entry(obj: &serde_json::Map<String, Value>) -> FlatEntry {
    let get_str = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let get_num = |k: &str| {
        obj.get(k).and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_i64().map(|i| i as f64))
                .or_else(|| v.as_u64().map(|u| u as f64))
                .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
        })
    };
    let get_uint = |k: &str| {
        obj.get(k).and_then(|v| {
            v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))
        })
    };

    FlatEntry {
        _type: get_str("_type"),
        url: get_str("url"),
        title: get_str("title"),
        id: get_str("id"),
        duration: get_num("duration"),
        ie_key: get_str("ie_key"),
        playlist_index: get_uint("playlist_index"),
        playlist_count: get_uint("playlist_count"),
        playlist_title: get_str("playlist_title"),
        thumbnail: best_thumbnail(obj),
    }
}

/// Pick the highest-resolution thumbnail URL from a `thumbnails` array, falling
/// back to the top-level `thumbnail` string. yt-dlp's flat-playlist entries carry
/// a small `thumbnails` list; single-video dicts carry a rich one plus a
/// top-level `thumbnail` URL. We maximise `width * height` (treating missing
/// dims as 0) and ignore entries without a `url`.
fn best_thumbnail(obj: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(Value::Array(arr)) = obj.get("thumbnails") {
        let mut best: Option<(f64, String)> = None;
        for t in arr {
            let Some(url) = t.get("url").and_then(|v| v.as_str()) else {
                continue;
            };
            let w = t.get("width").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let h = t.get("height").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let area = w * h;
            if best.as_ref().map_or(true, |(a, _)| area > *a) {
                best = Some((area, url.to_string()));
            }
        }
        if best.is_some() {
            return best.map(|(_, u)| u);
        }
    }
    obj.get("thumbnail").and_then(|v| v.as_str()).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real `downloading` progress tick captured from yt-dlp 2026.07.04 with
    // `--newline --progress-template '%(progress)j'`. One JSON object per line.
    const DOWNLOADING_LINE: &str = r#"{"status": "downloading", "downloaded_bytes": 2096128, "total_bytes": 61878609, "tmpfilename": "/tmp/x.webm.part", "filename": "/tmp/x.webm", "eta": 35, "speed": 1686945.1497892805, "elapsed": 2.2881102561950684, "ctx_id": null, "_eta_str": "00:35", "_speed_str": "   1.61MiB/s", "_percent": 3.3874840334565373, "_percent_str": "  3.4%", "_total_bytes_str": "  59.01MiB", "_downloaded_bytes_str": "   2.00MiB", "_elapsed_str": "00:00:02"}"#;

    const FINISHED_LINE: &str = r#"{"downloaded_bytes": 61878609, "total_bytes": 61878609, "filename": "/tmp/x.webm", "status": "finished", "elapsed": 10.32468557357788, "ctx_id": null, "speed": 5993268.129961734, "_speed_str": "5.72MiB/s", "_percent": 100.0, "_percent_str": "100.0%"}"#;

    #[test]
    fn classifies_progress_line_as_progress() {
        match parse_line(DOWNLOADING_LINE) {
            ParsedLine::Progress(p) => {
                assert_eq!(p.status.as_deref(), Some("downloading"));
                assert_eq!(p.filename.as_deref(), Some("/tmp/x.webm"));
                assert_eq!(p.tmpfilename.as_deref(), Some("/tmp/x.webm.part"));
                assert_eq!(p.downloaded_bytes, Some(2096128.0));
                assert_eq!(p.total_bytes, Some(61878609.0));
                assert_eq!(p.speed, Some(1686945.1497892805));
                assert_eq!(p.eta, Some(35.0));
                // Numeric `_percent` preferred over the string form.
                assert_eq!(p.percent, Some(3.3874840334565373));
                assert!((p.percent().unwrap() - 3.387_484).abs() < 0.01);
            }
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    #[test]
    fn classifies_finished_line_as_progress() {
        match parse_line(FINISHED_LINE) {
            ParsedLine::Progress(p) => {
                assert_eq!(p.status.as_deref(), Some("finished"));
                assert!((p.percent().unwrap() - 100.0).abs() < f64::EPSILON);
            }
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    #[test]
    fn classifies_info_line_as_log() {
        let line = "[download] Destination: /tmp/x.webm";
        match parse_line(line) {
            ParsedLine::Log(s) => assert_eq!(s, line),
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn classifies_error_line_as_log() {
        // yt-dlp prints `ERROR: ...` to stderr (no progress JSON with
        // status=="error" for extraction failures).
        let line = "ERROR: [youtube] BaW_jenozKc: Video unavailable";
        match parse_line(line) {
            ParsedLine::Log(s) => assert_eq!(s, line),
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn json_without_status_field_is_flat_entry() {
        // A JSON object without a `status` key is a `--flat-playlist -j` line
        // in the probe phase (an entry or a single-video dict), not a log
        // line and not a progress tick.
        let line = r#"{"filename": "/tmp/x.webm", "_default_template": "100%"}"#;
        match parse_line(line) {
            ParsedLine::FlatEntry(e) => {
                assert!(!e.is_playlist_entry()); // no _type:"url" -> not an entry
            }
            other => panic!("expected FlatEntry, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_log() {
        let line = "{not actually json";
        assert!(matches!(parse_line(line), ParsedLine::Log(_)));
    }

    #[test]
    fn percent_falls_back_to_bytes_when_no_percent_field() {
        // percent=None, percent_str=None -> derive from downloaded/total.
        let p = Progress {
            downloaded_bytes: Some(50.0),
            total_bytes: Some(200.0),
            ..Default::default()
        };
        assert!((p.percent().unwrap() - 25.0).abs() < f64::EPSILON);
    }

    // A real `--flat-playlist -j` line captured from the user's YouTube
    // playlist (70 entries) via tests/probe-flat-playlist.sh, with Firefox
    // cookies. `entry.url` is already the full watch URL; `_type=="url"` marks
    // it as a playlist entry; playlist_count/playlist_title arrive on line 1.
    const FLAT_ENTRY_LINE: &str = r#"{"_type":"url","ie_key":"Youtube","id":"p8eM3MEd_A4","url":"https://www.youtube.com/watch?v=p8eM3MEd_A4","title":"YAMATOMAYA #6 Progressive House Melodic Techno mix OCT '24(4K) River","description":null,"duration":3623,"channel":"YAMATOMAYA","thumbnails":[{"url":"https://i.ytimg.com/vi/p8eM3MEd_A4/hqdefault.jpg","height":94,"width":168}],"availability":null,"view_count":50000,"webpage_url":"https://www.youtube.com/watch?v=p8eM3MEd_A4","original_url":"https://www.youtube.com/watch?v=p8eM3MEd_A4","extractor":"youtube","extractor_key":"Youtube","playlist_count":70,"playlist":"Chill","playlist_id":"PLEueSxy2K1ZYInz4AIBbSugDZe377cILP","playlist_title":"Chill","n_entries":70,"playlist_index":1,"duration_string":"1:00:23"}"#;

    #[test]
    fn parses_real_flat_playlist_entry() {
        let e = parse_flat_line(FLAT_ENTRY_LINE).expect("entry line should parse");
        assert!(e.is_playlist_entry());
        assert_eq!(e._type.as_deref(), Some("url"));
        // url is already the full, directly-downloadable watch URL.
        assert_eq!(
            e.url.as_deref(),
            Some("https://www.youtube.com/watch?v=p8eM3MEd_A4")
        );
        assert_eq!(e.id.as_deref(), Some("p8eM3MEd_A4"));
        assert_eq!(
            e.title.as_deref(),
            Some("YAMATOMAYA #6 Progressive House Melodic Techno mix OCT '24(4K) River")
        );
        assert_eq!(e.duration, Some(3623.0));
        assert_eq!(e.ie_key.as_deref(), Some("Youtube"));
        assert_eq!(e.playlist_index, Some(1));
        assert_eq!(e.playlist_count, Some(70));
        assert_eq!(e.playlist_title.as_deref(), Some("Chill"));
        assert_eq!(
            e.thumbnail.as_deref(),
            Some("https://i.ytimg.com/vi/p8eM3MEd_A4/hqdefault.jpg")
        );
    }

    #[test]
    fn parse_line_classifies_flat_entry() {
        // The download-phase `parse_line` also recognises flat-entry lines
        // (JSON objects without `status`) as `FlatEntry`, not `Log`.
        match parse_line(FLAT_ENTRY_LINE) {
            ParsedLine::FlatEntry(e) => assert!(e.is_playlist_entry()),
            other => panic!("expected FlatEntry, got {other:?}"),
        }
    }

    #[test]
    fn single_video_dict_is_not_a_playlist_entry() {
        // A `--flat-playlist -j` line for a single (non-playlist) URL is a
        // full video dict: `_type` is "video" (or absent), and `url` is the
        // *media* URL, not a watch URL. The caller must NOT enqueue it as a
        // per-video item (it would re-download a media stream URL); it keeps
        // the original submitted URL and borrows only title/duration.
        let line = r#"{"_type":"video","id":"p8eM3MEd_A4","title":"Some Video","duration":3623,"formats":[{"format_id":"137"}],"webpage_url":"https://www.youtube.com/watch?v=p8eM3MEd_A4"}"#;
        let e = parse_flat_line(line).expect("video line should parse");
        assert!(!e.is_playlist_entry());
        assert_eq!(e.title.as_deref(), Some("Some Video"));
        assert_eq!(e.duration, Some(3623.0));
    }

    #[test]
    fn non_json_line_is_not_a_flat_entry() {
        // `parse_flat_line` returns None for non-JSON (WARNING:/ERROR:/status
        // lines); the probe treats those as log lines.
        assert!(parse_flat_line("ERROR: [youtube] x: Video unavailable").is_none());
        assert!(parse_flat_line("[download] Destination: /tmp/x.webm").is_none());
        assert!(parse_flat_line("not json").is_none());
    }

    #[test]
    fn flat_entry_without_url_is_not_downloadable() {
        // A playlist entry missing its `url` (some extractors omit it in flat
        // mode) cannot be enqueued as a per-video item.
        let line = r#"{"_type":"url","id":"abc","title":"No URL here"}"#;
        let e = parse_flat_line(line).expect("should parse");
        assert!(!e.is_playlist_entry()); // url is None
    }

}

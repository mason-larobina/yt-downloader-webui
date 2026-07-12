//! Classify a single line of yt-dlp stdout/stderr into a structured event.
use serde_json::Value;

use crate::state::Progress;

/// Result of classifying one yt-dlp output line.
#[derive(Debug)]
pub enum ParsedLine {
    /// A progress tick parsed from `%(progress)j` JSON.
    Progress(Progress),
    /// Anything else (yt-dlp status lines, `WARNING:`, `ERROR:`, etc.).
    Log(String),
}

/// Classify one line of yt-dlp output.
///
/// Per DESIGN Sec. 5: a line that parses as a JSON object containing a
/// `status` field is a progress event; everything else is a log line.
pub fn parse_line(line: &str) -> ParsedLine {
    let trimmed = line.trim();
    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(obj) = value.as_object() {
                if obj.contains_key("status") {
                    return ParsedLine::Progress(parse_progress(obj));
                }
            }
        }
    }
    ParsedLine::Log(line.to_string())
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
    fn json_without_status_field_is_log() {
        // yt-dlp's `finished`-style summary line lacks status in some versions;
        // a JSON object without a `status` key must NOT be treated as progress.
        let line = r#"{"filename": "/tmp/x.webm", "_default_template": "100%"}"#;
        match parse_line(line) {
            ParsedLine::Log(s) => assert_eq!(s, line),
            other => panic!("expected Log, got {other:?}"),
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

}

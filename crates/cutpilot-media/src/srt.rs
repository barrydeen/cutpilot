//! SRT/VTT import and SRT export for the caption track.

use cutpilot_core::{Caption, Ticks, TICKS_PER_SEC};
use uuid::Uuid;

/// Parse SRT or WebVTT text into captions, with an optional tick offset.
pub fn parse_srt(text: &str, offset_ticks: Ticks) -> Result<Vec<Caption>, String> {
    let mut captions = Vec::new();
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.lines().peekable();
    // Skip a WebVTT header and NOTE blocks.
    while let Some(line) = lines.peek() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("WEBVTT") || t.starts_with("NOTE") {
            lines.next();
        } else {
            break;
        }
    }
    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Optional numeric index line.
        let time_line = if line.chars().all(|c| c.is_ascii_digit()) && !line.contains("--") {
            match lines.next() {
                Some(next) => next.trim().to_string(),
                None => break,
            }
        } else {
            line.to_string()
        };
        if !time_line.contains("-->") {
            return Err(
                "invalid SRT/VTT format: expected a timecode line (`HH:MM:SS,mmm --> HH:MM:SS,mmm`)"
                    .into(),
            );
        }
        let mut parts = time_line.split("-->");
        let start = parts
            .next()
            .map(parse_timestamp)
            .transpose()?
            .ok_or("missing start time")?;
        let end_raw = parts.next().ok_or("missing end time")?;
        // Strip VTT settings after the timecode.
        let end_str = end_raw.split_whitespace().next().unwrap_or("");
        let end = parse_timestamp(end_str)?;
        if end <= start {
            return Err(format!("caption end {end_str} is not after start"));
        }
        let mut body: Vec<&str> = Vec::new();
        for text_line in lines.by_ref() {
            if text_line.trim().is_empty() {
                break;
            }
            body.push(text_line);
        }
        let text = body.join("\n").trim().to_string();
        if text.is_empty() {
            return Err("caption has empty text".into());
        }
        captions.push(Caption {
            id: Uuid::new_v4(),
            start: start + offset_ticks,
            end: end + offset_ticks,
            text,
        });
    }
    Ok(captions)
}

fn parse_timestamp(s: &str) -> Result<Ticks, String> {
    let s = s.trim();
    // Accept HH:MM:SS,mmm and H:MM:SS.mmm and MM:SS.mmm.
    let bad = || format!("bad timestamp `{s}`");
    let (hms, ms) = match s.rsplit_once(['.', ',']) {
        Some((hms, frac)) => (hms, frac),
        None => (s, "0"),
    };
    let ms: i64 = if ms.len() == 3 {
        ms.parse().map_err(|_| bad())?
    } else if ms.len() == 2 {
        ms.parse::<i64>().map_err(|_| bad())? * 10
    } else if ms.is_empty() {
        0
    } else {
        return Err(bad());
    };
    let segs: Vec<i64> = hms
        .split(':')
        .map(|p| p.parse::<i64>().map_err(|_| bad()))
        .collect::<Result<_, _>>()?;
    let (h, m, sec) = match segs.as_slice() {
        [h, m, s] => (*h, *m, *s),
        [m, s] => (0, *m, *s),
        [s] => (0, 0, *s),
        _ => return Err(bad()),
    };
    if !(0..=999).contains(&h) || !(0..=59).contains(&m) || !(0..=59).contains(&sec) || !(0..=999).contains(&ms) {
        return Err(bad());
    }
    let total_ms = h
        .checked_mul(60)
        .and_then(|v| v.checked_add(m))
        .and_then(|v| v.checked_mul(60))
        .and_then(|v| v.checked_add(sec))
        .and_then(|v| v.checked_mul(1_000))
        .and_then(|v| v.checked_add(ms))
        .ok_or_else(bad)?;
    total_ms
        .checked_mul(TICKS_PER_SEC / 1_000)
        .ok_or_else(bad)
}

fn srt_timestamp(ticks: Ticks) -> String {
    let total_ms = cutpilot_core::ticks::ms_from_ticks(ticks.max(0));
    let ms = total_ms % 1_000;
    let total_s = total_ms / 1_000;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

/// Serialize captions to SRT, sorted by start time.
pub fn to_srt(mut captions: Vec<Caption>) -> String {
    captions.sort_by_key(|c| c.start);
    let mut out = String::new();
    for (i, caption) in captions.iter().enumerate() {
        out.push_str(&format!("{}\n", i + 1));
        out.push_str(&format!(
            "{} --> {}\n",
            srt_timestamp(caption.start),
            srt_timestamp(caption.end)
        ));
        out.push_str(&format!("{}\n\n", caption.text.trim_end_matches('\n')));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_srt() {
        let srt = "1\n00:00:01,500 --> 00:00:03,000\nHello\n\n2\n00:00:05.250 --> 00:00:06,750\nSecond {tag}\n";
        let caps = parse_srt(srt, 0).unwrap();
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[0].text, "Hello");
        assert_eq!(caps[0].start, TICKS_PER_SEC + TICKS_PER_SEC / 2);
        assert_eq!(caps[1].text, "Second {tag}");
        let back = to_srt(caps);
        assert!(back.contains("00:00:01,500 --> 00:00:03,000"));
        assert!(back.contains("Second {tag}"));
    }

    #[test]
    fn vtt_header_and_offset() {
        let vtt = "WEBVTT\n\n00:00:00.500 --> 00:00:01.000\nHi\n";
        let caps = parse_srt(vtt, TICKS_PER_SEC).unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].start, TICKS_PER_SEC + TICKS_PER_SEC / 2);
        assert_eq!(caps[0].end, TICKS_PER_SEC * 2);
    }

    #[test]
    fn parse_srt_does_not_leak_non_srt_file_contents_and_rejects_overflow() {
        let err = parse_srt("SECRET_API_KEY=super-secret-value\n", 0).unwrap_err();
        assert!(
            !err.contains("SECRET_API_KEY") && !err.contains("super-secret-value"),
            "error must not echo arbitrary file contents: {err}"
        );
        assert!(parse_srt("1\n999999999999:00:00,000 --> 999999999999:00:01,000\nhi\n", 0).is_err());
    }
}

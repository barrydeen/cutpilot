//! ASS (Advanced SubStation Alpha) generation for burned-in captions.
//! Rendered by libass at export time and approximated by the UI preview.

use openprem_core::{CaptionStyle, Ticks, TICKS_PER_SEC};

/// Convert `#RRGGBB` to ASS `&HAABBGGRR` (alpha 00 = opaque).
pub fn hex_to_ass_color(hex: &str) -> Result<String, String> {
    let ok = hex.len() == 7 && hex.starts_with('#') && hex[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if !ok {
        return Err(format!("invalid color `{hex}`: expected #RRGGBB"));
    }
    let r = &hex[1..3];
    let g = &hex[3..5];
    let b = &hex[5..7];
    Ok(format!("&H00{b}{g}{r}"))
}

/// ASS timestamp: `H:MM:SS.CC`.
fn ass_time(ticks: Ticks) -> String {
    let cs = ((ticks.max(0) as f64) / (TICKS_PER_SEC as f64 / 100.0)).round() as i64;
    let cs_part = cs % 100;
    let total_s = cs / 100;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    format!("{h}:{m:02}:{s:02}.{cs_part:02}")
}

/// Escape caption text for ASS: no override tags, newlines become `\N`.
fn escape_ass_text(text: &str) -> String {
    text.replace('{', "(")
        .replace('}', ")")
        .replace('\r', "")
        .replace('\n', "\\N")
}

/// Full ASS document for the project's captions, in timeline time.
pub fn generate_ass(
    style: &CaptionStyle,
    captions: &[openprem_core::Caption],
    width: u32,
    height: u32,
) -> Result<String, String> {
    let primary = hex_to_ass_color(&style.primary_color)?;
    let outline = hex_to_ass_color(&style.outline_color)?;
    let mut s = String::new();
    s.push_str("[Script Info]\n");
    s.push_str("ScriptType: v4.00+\n");
    s.push_str(&format!("PlayResX: {width}\n"));
    s.push_str(&format!("PlayResY: {height}\n"));
    s.push_str("WrapStyle: 0\n");
    s.push_str("ScaledBorderAndShadow: yes\n\n");
    s.push_str("[V4+ Styles]\n");
    s.push_str("Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n");
    s.push_str(&format!(
        "Style: OpenPrem,{font},{size},{primary},&H000000FF,{outline},&H80000000,0,0,0,0,100,100,0,0,1,{outline_w},0,{align},80,80,{margin},1\n\n",
        font = style.font,
        size = style.font_size,
        primary = primary,
        outline = outline,
        outline_w = if style.outline { 2 } else { 0 },
        align = style.alignment,
        margin = style.margin_v,
    ));
    s.push_str("[Events]\n");
    s.push_str("Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
    for caption in captions {
        s.push_str(&format!(
            "Dialogue: 0,{},{},OpenPrem,,0,0,0,,{}\n",
            ass_time(caption.start),
            ass_time(caption.end),
            escape_ass_text(&caption.text)
        ));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openprem_core::Caption;

    #[test]
    fn colors_convert_bgr() {
        assert_eq!(hex_to_ass_color("#FF8040").unwrap(), "&H004080FF");
        assert!(hex_to_ass_color("#GGGGGG").is_err());
    }

    #[test]
    fn captions_render_as_dialogue() {
        let style = CaptionStyle::default();
        let captions = vec![Caption {
            id: uuid::Uuid::new_v4(),
            start: TICKS_PER_SEC,
            end: TICKS_PER_SEC * 3,
            text: "Hello\nworld".into(),
        }];
        let ass = generate_ass(&style, &captions, 3840, 2160).unwrap();
        assert!(ass.contains("PlayResX: 3840"));
        assert!(ass.contains("Dialogue: 0,0:00:01.00,0:00:03.00,OpenPrem,,0,0,0,,Hello\\Nworld"));
        assert!(ass.contains("Style: OpenPrem,Sans,64,&H00FFFFFF"));
    }
}

//! Frame-exact timeline time.
//!
//! All edit decisions are stored as integer ticks at 1/3,600,000 second.
//! This timebase divides evenly for every frame rate CutPilot supports,
//! including NTSC drop-frame material (23.976, 29.97, 59.94):
//!
//! - 24 fps    -> 150,000 ticks per frame
//! - 23.976    -> 150,150 ticks per frame (exact)
//! - 29.97     -> 120,120 ticks per frame (exact)
//! - 59.94     -> 60,060  ticks per frame (exact)

/// Ticks per second. 3,600,000 is divisible by 24, 25, 30, 48, 50, 60, 120
/// and by the 1001-based NTSC rates after multiplication.
pub const TICKS_PER_SEC: i64 = 3_600_000;

/// A position or duration on the timeline, in [`TICKS_PER_SEC`] units.
pub type Ticks = i64;

/// Convert seconds (may be fractional) to ticks, rounding to the nearest tick.
pub fn ticks_from_secs(secs: f64) -> Ticks {
    (secs * TICKS_PER_SEC as f64).round() as Ticks
}

/// Convert ticks to fractional seconds.
pub fn secs_from_ticks(ticks: Ticks) -> f64 {
    ticks as f64 / TICKS_PER_SEC as f64
}

/// Convert milliseconds to ticks.
pub fn ticks_from_ms(ms: i64) -> Ticks {
    ms * (TICKS_PER_SEC / 1_000)
}

/// Convert ticks to whole milliseconds (rounded).
pub fn ms_from_ticks(ticks: Ticks) -> i64 {
    const HALF: i64 = TICKS_PER_SEC / 2_000;
    (ticks + HALF) / (TICKS_PER_SEC / 1_000)
}

/// `HH:MM:SS.mmm` display used by the UI and tool summaries.
pub fn format_seconds(ticks: Ticks) -> String {
    let total_ms = ms_from_ticks(ticks.max(0));
    let ms = total_ms % 1_000;
    let total_s = total_ms / 1_000;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntsc_frame_durations_are_exact() {
        // 30000/1001 fps
        let ntsc30 = crate::model::Fps::ntsc(30);
        let frame = ntsc30.frame_duration_ticks();
        assert_eq!(frame * 30_000, TICKS_PER_SEC * 1_001);
        // 24000/1001 fps
        let ntsc24 = crate::model::Fps::ntsc(24);
        let frame24 = ntsc24.frame_duration_ticks();
        assert_eq!(frame24 * 24_000, TICKS_PER_SEC * 1_001);
        // integer fps
        assert_eq!(crate::model::Fps::from_int(30).frame_duration_ticks(), 120_000);
    }

    #[test]
    fn seconds_roundtrip_within_one_tick() {
        for &secs in &[0.0, 1.5, 12.033366, 3600.0] {
            let back = secs_from_ticks(ticks_from_secs(secs));
            assert!(
                (back - secs).abs() <= 1.0 / TICKS_PER_SEC as f64,
                "{secs} -> {back}"
            );
        }
    }
}

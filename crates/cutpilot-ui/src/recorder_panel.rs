//! Native Voiceover Studio panel & live VU meter widgets for `cutpilot-ui`.
//!
//! Designed to be effortless out of the box: click Record and get warm, crisp,
//! broadcast-leveled audio automatically via Studio Voice (real-time rumble
//! filter + auto-leveling + anti-clipping limiter + FFmpeg mastering on stop),
//! with a single Mic Level slider if your microphone is extra hot or quiet.
//!
//! All start/stop/discard recording mutations are returned as [`RecorderUiAction`]
//! and executed through `Engine::execute` (no UI-only actions).

use cutpilot_core::{Project, TrackKind};
use cutpilot_engine::LiveRecordingSnapshot;
use cutpilot_media::record::{
    AudioInputDevice, DEFAULT_CHANNELS, DEFAULT_INPUT_GAIN, DEFAULT_SAMPLE_RATE,
};
use eframe::egui;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Actions emitted by the Voiceover Studio panel or toolbar buttons.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecorderUiAction {
    None,
    /// Begin recording immediately (or after the configured countdown).
    RequestStart,
    /// Countdown reached zero — start the actual engine capture now.
    StartNow,
    /// Stop recording, finalize/polish WAV, import, and insert onto the timeline.
    StopAndInsert,
    /// Cancel countdown or discard the active recording take.
    Discard,
    /// User dragged the Mic Level slider while recording; update live DSP gain.
    UpdateLiveGain(f32),
}

pub struct RecorderPanelState {
    pub open: bool,
    pub devices: Vec<AudioInputDevice>,
    devices_rx: Option<mpsc::Receiver<Vec<AudioInputDevice>>>,
    pub selected_device: String,
    pub take_name: String,
    pub channels: u16,
    pub sample_rate: u32,
    /// Linear microphone gain (`0.10..=2.0`, default `0.65` for clean headroom).
    pub input_gain: f32,
    /// Studio Voice broadcast processing (ON by default).
    pub studio_voice: bool,
    pub target_track: Option<Uuid>,
    pub countdown_secs: u8,
    pub play_video_while_recording: bool,
    pub mute_timeline_while_recording: bool,
    pub countdown_until: Option<Instant>,
    pub saved_playback_muted: Option<bool>,
}

impl Default for RecorderPanelState {
    fn default() -> Self {
        Self {
            open: false,
            devices: vec![AudioInputDevice {
                id: "default".into(),
                name: "System Default Microphone".into(),
                backend: "auto".into(),
                is_default: true,
                is_monitor: false,
            }],
            devices_rx: None,
            selected_device: "default".into(),
            take_name: String::new(),
            channels: DEFAULT_CHANNELS,
            sample_rate: DEFAULT_SAMPLE_RATE,
            input_gain: DEFAULT_INPUT_GAIN,
            studio_voice: true,
            target_track: None,
            countdown_secs: 0,
            play_video_while_recording: true,
            mute_timeline_while_recording: true,
            countdown_until: None,
            saved_playback_muted: None,
        }
    }
}

impl RecorderPanelState {
    /// Refresh available microphones asynchronously so the UI never blocks.
    pub fn refresh_devices(&mut self, ctx: &egui::Context) {
        if self.devices_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let devs = cutpilot_media::record::list_input_devices();
            let _ = tx.send(devs);
            repaint.request_repaint();
        });
        self.devices_rx = Some(rx);
    }

    /// Poll background device discovery and countdown timer.
    pub fn poll(&mut self, ctx: &egui::Context) -> RecorderUiAction {
        if let Some(rx) = &self.devices_rx {
            match rx.try_recv() {
                Ok(devs) => {
                    if !devs.is_empty() {
                        self.devices = devs;
                        if !self.devices.iter().any(|d| d.id == self.selected_device) {
                            self.selected_device = self.devices[0].id.clone();
                        }
                    }
                    self.devices_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.devices_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        if let Some(deadline) = self.countdown_until {
            if Instant::now() >= deadline {
                self.countdown_until = None;
                return RecorderUiAction::StartNow;
            }
            ctx.request_repaint_after(Duration::from_millis(40));
        }

        RecorderUiAction::None
    }

    pub fn is_counting_down(&self) -> bool {
        self.countdown_until.is_some()
    }

    /// Remaining whole seconds in the countdown (`3`, `2`, `1`), if active.
    pub fn countdown_remaining_secs(&self) -> Option<u64> {
        let deadline = self.countdown_until?;
        let rem = deadline.saturating_duration_since(Instant::now());
        Some(rem.as_secs_f64().ceil().max(1.0) as u64)
    }

    pub fn begin_countdown(&mut self) {
        if self.countdown_secs > 0 {
            self.countdown_until =
                Some(Instant::now() + Duration::from_secs(self.countdown_secs as u64));
        }
    }

    pub fn cancel_countdown(&mut self) {
        self.countdown_until = None;
    }

    pub fn selected_device_label(&self) -> &str {
        self.devices
            .iter()
            .find(|d| d.id == self.selected_device)
            .map(|d| d.name.as_str())
            .unwrap_or("System Default Microphone")
    }

    /// Compact toolbar widget in the top bar: Record/Stop button, live mini VU
    /// meter when recording, discard button, and Voiceover Recorder toggle.
    pub fn top_bar_controls(
        &mut self,
        ui: &mut egui::Ui,
        live: &LiveRecordingSnapshot,
    ) -> RecorderUiAction {
        let mut action = RecorderUiAction::None;

        if let Some(rem) = self.countdown_remaining_secs() {
            let btn = egui::Button::new(
                egui::RichText::new(format!("⏳ Starting in {rem}… (Cancel)"))
                    .strong()
                    .color(egui::Color32::WHITE),
            )
            .fill(egui::Color32::from_rgb(200, 120, 35));
            if ui
                .add(btn)
                .on_hover_text("Click or press Esc to cancel countdown")
                .clicked()
            {
                action = RecorderUiAction::Discard;
            }
        } else if live.recording {
            let pulse = ((ui.input(|i| i.time) * 3.2).sin() * 0.5 + 0.5) as f32;
            let red = egui::Color32::from_rgb(
                (195.0 + 40.0 * pulse) as u8,
                (45.0 + 18.0 * pulse) as u8,
                (48.0 + 16.0 * pulse) as u8,
            );
            let btn = egui::Button::new(
                egui::RichText::new(format!(
                    "⏹ Stop VO  {}",
                    format_mmss_tenths(live.elapsed_secs)
                ))
                .strong()
                .color(egui::Color32::WHITE),
            )
            .fill(red);
            if ui
                .add(btn)
                .on_hover_text("Stop recording, polish voice & add to timeline (R or Space)")
                .clicked()
            {
                action = RecorderUiAction::StopAndInsert;
            }
            draw_vu_meter(ui, egui::vec2(72.0, 14.0), live.peak, live.rms, live.clipping);
            if ui
                .button(
                    egui::RichText::new("✕")
                        .strong()
                        .color(egui::Color32::from_rgb(245, 130, 130)),
                )
                .on_hover_text("Discard current voiceover take (Esc)")
                .clicked()
            {
                action = RecorderUiAction::Discard;
            }
        } else {
            let btn = egui::Button::new(
                egui::RichText::new("🎙 Record VO")
                    .strong()
                    .color(egui::Color32::from_rgb(255, 225, 225)),
            )
            .fill(egui::Color32::from_rgb(138, 38, 44));
            if ui
                .add(btn)
                .on_hover_text("Record a crisp voiceover at the playhead (R)")
                .clicked()
            {
                action = RecorderUiAction::RequestStart;
            }
        }

        let studio_btn = egui::Button::new("🎙⚙").selected(self.open);
        if ui
            .add(studio_btn)
            .on_hover_text("Voiceover Recorder settings (Mic Level, Studio Voice, Microphone)")
            .clicked()
        {
            self.open = !self.open;
            if self.open {
                self.refresh_devices(ui.ctx());
            }
        }

        action
    }

    /// Clean, simple Voiceover Recorder window.
    /// Keeps the default view focused on 3 essentials (Record, Mic Level,
    /// Studio Voice) with technical details tucked under Advanced.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        project: Option<&Project>,
        live: &LiveRecordingSnapshot,
    ) -> RecorderUiAction {
        if !self.open {
            return RecorderUiAction::None;
        }
        let mut action = RecorderUiAction::None;
        let mut open = self.open;

        egui::Window::new("🎙 Voiceover Recorder")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(370.0)
            .show(ctx, |ui| {
                ui.set_width(354.0);

                // ---- Live Meter + Waveform Card ----
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(16, 18, 24))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .stroke(egui::Stroke::new(
                        1.2,
                        if live.recording {
                            egui::Color32::from_rgb(225, 68, 68)
                        } else {
                            egui::Color32::from_gray(46)
                        },
                    ))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if live.recording {
                                ui.label(
                                    egui::RichText::new("● REC")
                                        .strong()
                                        .color(egui::Color32::from_rgb(255, 85, 85)),
                                );
                                ui.label(
                                    egui::RichText::new(format_mmss_tenths(live.elapsed_secs))
                                        .monospace()
                                        .strong()
                                        .size(16.0)
                                        .color(egui::Color32::WHITE),
                                );
                            } else if let Some(rem) = self.countdown_remaining_secs() {
                                ui.label(
                                    egui::RichText::new(format!("⏳ Starting in {rem}…"))
                                        .strong()
                                        .size(15.0)
                                        .color(egui::Color32::from_rgb(250, 195, 85)),
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new("Ready")
                                        .strong()
                                        .color(egui::Color32::from_gray(220)),
                                );
                            }

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let (hint, col) = level_coaching(live);
                                    ui.label(egui::RichText::new(hint).small().strong().color(col));
                                },
                            );
                        });

                        ui.add_space(7.0);
                        let w = ui.available_width();
                        draw_vu_meter(
                            ui,
                            egui::vec2(w, 16.0),
                            if live.recording { live.peak } else { 0.0 },
                            if live.recording { live.rms } else { 0.0 },
                            live.recording && live.clipping,
                        );

                        ui.add_space(6.0);
                        draw_live_waveform(
                            ui,
                            egui::vec2(w, 46.0),
                            &live.waveform_peaks,
                            live.recording,
                        );

                        if let Some(err) = &live.error {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(format!("Mic warning: {err}"))
                                    .small()
                                    .color(egui::Color32::from_rgb(245, 115, 115)),
                            );
                        }
                    });

                ui.add_space(10.0);

                // ---- Primary Record / Stop Button ----
                ui.horizontal(|ui| {
                    if live.recording {
                        let stop_btn = egui::Button::new(
                            egui::RichText::new("⏹ Stop & Add to Timeline")
                                .strong()
                                .size(13.5)
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(215, 55, 58))
                        .min_size(egui::vec2(246.0, 34.0));
                        if ui.add(stop_btn).clicked() {
                            action = RecorderUiAction::StopAndInsert;
                        }
                        if ui
                            .add(
                                egui::Button::new("🗑 Discard")
                                    .min_size(egui::vec2(100.0, 34.0)),
                            )
                            .clicked()
                        {
                            action = RecorderUiAction::Discard;
                        }
                    } else if self.is_counting_down() {
                        let cancel_btn = egui::Button::new(
                            egui::RichText::new("Cancel Countdown (Esc)")
                                .strong()
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(185, 110, 35))
                        .min_size(egui::vec2(354.0, 34.0));
                        if ui.add(cancel_btn).clicked() {
                            action = RecorderUiAction::Discard;
                        }
                    } else {
                        let rec_btn = egui::Button::new(
                            egui::RichText::new("⏺ Record Voiceover  (R)")
                                .strong()
                                .size(13.5)
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(195, 48, 52))
                        .min_size(egui::vec2(354.0, 34.0));
                        if ui.add(rec_btn).clicked() {
                            action = RecorderUiAction::RequestStart;
                        }
                    }
                });

                ui.add_space(10.0);

                // ---- Essential Control 1: Mic Level (adjustable even while recording!) ----
                let mut pct = (self.input_gain * 100.0).round();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Mic Level").strong());
                    ui.spacing_mut().slider_width = 185.0;
                    let changed = ui
                        .add(
                            egui::Slider::new(&mut pct, 10.0..=150.0)
                                .suffix("%")
                                .fixed_decimals(0),
                        )
                        .on_hover_text(
                            "Lower this if your mic is loud. Works live even while recording.",
                        )
                        .changed();
                    if changed {
                        self.input_gain = (pct / 100.0).clamp(0.10, 1.50);
                        if live.recording {
                            action = RecorderUiAction::UpdateLiveGain(self.input_gain);
                        }
                    }
                    if ui
                        .small_button("Reset")
                        .on_hover_text("Reset Mic Level to 65% (clean headroom)")
                        .clicked()
                    {
                        self.input_gain = DEFAULT_INPUT_GAIN;
                        if live.recording {
                            action = RecorderUiAction::UpdateLiveGain(self.input_gain);
                        }
                    }
                });

                ui.add_space(4.0);

                // ---- Essential Control 2 & 3: Studio Voice + Quick Toggles ----
                let idle = !live.recording && !self.is_counting_down();
                ui.add_enabled_ui(idle, |ui| {
                    ui.checkbox(
                        &mut self.studio_voice,
                        egui::RichText::new("✨ Studio Voice (recommended)").strong(),
                    )
                    .on_hover_text(
                        "Automatically tames loud mics, prevents red clipping, removes room noise & rumble, and masters your voice to crisp broadcast loudness (-16 LUFS).",
                    );

                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        let mut use_countdown = self.countdown_secs > 0;
                        if ui.checkbox(&mut use_countdown, "3s countdown").changed() {
                            self.countdown_secs = if use_countdown { 3 } else { 0 };
                        }
                        ui.add_space(10.0);
                        ui.checkbox(&mut self.play_video_while_recording, "Watch video");
                    });

                    ui.add_space(6.0);
                    // ---- Collapsible Advanced Settings ----
                    egui::CollapsingHeader::new(
                        egui::RichText::new("Advanced (Microphone, Track, Format)").small(),
                    )
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Microphone");
                            let sel_label = self.selected_device_label().to_string();
                            egui::ComboBox::from_id_salt("vo_input_device")
                                .width(205.0)
                                .selected_text(sel_label)
                                .show_ui(ui, |ui| {
                                    for dev in &self.devices {
                                        let label = if dev.is_default && dev.id != "default" {
                                            format!("★ {}", dev.name)
                                        } else {
                                            dev.name.clone()
                                        };
                                        ui.selectable_value(
                                            &mut self.selected_device,
                                            dev.id.clone(),
                                            label,
                                        );
                                    }
                                });
                            if ui
                                .small_button("↻")
                                .on_hover_text("Refresh microphone list")
                                .clicked()
                            {
                                self.refresh_devices(ctx);
                            }
                        });

                        ui.horizontal(|ui| {
                            ui.label("Take Name");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.take_name)
                                    .hint_text("auto (voiceover-001, …)")
                                    .desired_width(ui.available_width()),
                            );
                        });

                        if let Some(proj) = project {
                            let audio_tracks: Vec<(Uuid, String)> = proj
                                .tracks
                                .iter()
                                .filter(|t| t.kind == TrackKind::Audio && !t.locked)
                                .map(|t| (t.id, t.name.clone()))
                                .collect();
                            if let Some(tid) = self.target_track {
                                if !audio_tracks.iter().any(|(id, _)| *id == tid) {
                                    self.target_track = None;
                                }
                            }
                            let cur_track_label = self
                                .target_track
                                .and_then(|tid| {
                                    audio_tracks
                                        .iter()
                                        .find(|(id, _)| *id == tid)
                                        .map(|(_, n)| n.clone())
                                })
                                .unwrap_or_else(|| "Auto (Voiceover track)".into());
                            ui.horizontal(|ui| {
                                ui.label("Track");
                                egui::ComboBox::from_id_salt("vo_target_track")
                                    .width(210.0)
                                    .selected_text(cur_track_label)
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut self.target_track,
                                            None,
                                            "Auto (Voiceover track)",
                                        );
                                        for (tid, tname) in &audio_tracks {
                                            ui.selectable_value(
                                                &mut self.target_track,
                                                Some(*tid),
                                                tname,
                                            );
                                        }
                                    });
                            });
                        }

                        ui.horizontal(|ui| {
                            ui.label("Format");
                            egui::ComboBox::from_id_salt("vo_channels")
                                .width(115.0)
                                .selected_text(if self.channels == 1 {
                                    "Mono (Voice)"
                                } else {
                                    "Stereo (2 ch)"
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.channels, 1, "Mono (Voice)");
                                    ui.selectable_value(&mut self.channels, 2, "Stereo (2 ch)");
                                });
                            egui::ComboBox::from_id_salt("vo_sample_rate")
                                .width(105.0)
                                .selected_text(format!("{} kHz WAV", self.sample_rate / 1000))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.sample_rate, 48_000, "48 kHz WAV");
                                    ui.selectable_value(
                                        &mut self.sample_rate,
                                        44_100,
                                        "44.1 kHz WAV",
                                    );
                                });
                        });

                        ui.add_enabled(
                            self.play_video_while_recording,
                            egui::Checkbox::new(
                                &mut self.mute_timeline_while_recording,
                                "Mute timeline audio while recording",
                            ),
                        );
                    });
                });
            });

        self.open = open;
        action
    }
}

fn level_coaching(live: &LiveRecordingSnapshot) -> (String, egui::Color32) {
    if !live.recording {
        return (
            "✨ Studio Voice Ready".into(),
            egui::Color32::from_rgb(120, 210, 160),
        );
    }
    if live.clipping {
        return (
            "Too loud — lower Mic Level".into(),
            egui::Color32::from_rgb(255, 85, 85),
        );
    }
    if live.input_hot && live.studio_voice {
        return (
            format!("Auto-leveled · {:.0} dB", live.db),
            egui::Color32::from_rgb(245, 198, 72),
        );
    }
    if live.peak < 0.03 {
        return ("Quiet".into(), egui::Color32::from_gray(145));
    }
    (
        format!("Good level · {:.0} dB", live.db),
        egui::Color32::from_rgb(95, 225, 145),
    )
}

/// Draw a horizontal segmented VU level meter (green sweet spot -> gold -> red).
pub fn draw_vu_meter(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    peak: f32,
    rms: f32,
    clipping: bool,
) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(18, 21, 27));
    painter.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(
            1.0,
            if clipping {
                egui::Color32::from_rgb(255, 65, 65)
            } else {
                egui::Color32::from_gray(52)
            },
        ),
        egui::StrokeKind::Inside,
    );

    let inner = rect.shrink(2.0);
    let norm = |v: f32| -> f32 {
        if v <= 1e-4 {
            0.0
        } else {
            let db = (20.0 * v.log10()).clamp(-60.0, 0.0);
            ((db + 60.0) / 60.0).clamp(0.0, 1.0)
        }
    };
    let rms_frac = norm(rms);
    let peak_frac = norm(peak);

    let segments = 20usize;
    let gap = 1.5;
    let seg_w = ((inner.width() - gap * (segments as f32 - 1.0)) / segments as f32).max(1.0);
    for i in 0..segments {
        let frac = (i as f32 + 0.5) / segments as f32;
        let x0 = inner.left() + i as f32 * (seg_w + gap);
        let seg_rect = egui::Rect::from_min_size(
            egui::pos2(x0, inner.top()),
            egui::vec2(seg_w, inner.height()),
        );
        let base_col = if frac > 0.90 {
            egui::Color32::from_rgb(245, 68, 68)
        } else if frac > 0.76 {
            egui::Color32::from_rgb(245, 195, 62)
        } else {
            egui::Color32::from_rgb(76, 214, 130)
        };
        let col = if frac <= rms_frac {
            base_col
        } else if frac <= peak_frac {
            base_col.linear_multiply(0.75)
        } else {
            base_col.linear_multiply(0.14)
        };
        painter.rect_filled(seg_rect, 1.5, col);
    }
}

fn draw_live_waveform(ui: &mut egui::Ui, size: egui::Vec2, peaks: &[f32], recording: bool) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(12, 14, 19));
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(42)),
        egui::StrokeKind::Inside,
    );

    let mid_y = rect.center().y;
    painter.line_segment(
        [
            egui::pos2(rect.left() + 4.0, mid_y),
            egui::pos2(rect.right() - 4.0, mid_y),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 24)),
    );

    if peaks.is_empty() {
        let hint = if recording {
            "Listening to microphone…"
        } else {
            "Click Record and speak normally"
        };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            hint,
            egui::FontId::proportional(11.0),
            egui::Color32::from_gray(115),
        );
        return;
    }

    let inner = rect.shrink(4.0);
    let step = 3.0_f32;
    let max_bars = (inner.width() / step).floor().max(1.0) as usize;
    let start = peaks.len().saturating_sub(max_bars);
    let slice = &peaks[start..];
    let half_h = inner.height() * 0.46;

    for (idx, &p) in slice.iter().enumerate() {
        let x = inner.right() - (slice.len() - idx) as f32 * step + 1.5;
        let h = (p.clamp(0.0, 1.0).powf(0.65) * half_h).max(1.5);
        let col = if p >= 0.90 {
            egui::Color32::from_rgb(255, 85, 85)
        } else if p >= 0.68 {
            egui::Color32::from_rgb(245, 200, 75)
        } else {
            egui::Color32::from_rgb(95, 222, 155)
        };
        painter.line_segment(
            [egui::pos2(x, mid_y - h), egui::pos2(x, mid_y + h)],
            egui::Stroke::new(2.0, col),
        );
    }
}

pub fn format_mmss_tenths(secs: f64) -> String {
    let clamped = secs.max(0.0);
    let total_tenths = (clamped * 10.0).floor() as u64;
    let tenths = total_tenths % 10;
    let total_secs = total_tenths / 10;
    let s = total_secs % 60;
    let m = total_secs / 60;
    format!("{m:02}:{s:02}.{tenths}")
}

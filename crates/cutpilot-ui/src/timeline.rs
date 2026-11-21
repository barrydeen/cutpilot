//! Interactive NLE timeline widget:
//! - Select tool (V): select, drag-to-move with ghost & snapping, drag left/right
//!   edges to trim clips directly on the timeline
//! - Razor/Blade tool (C): hover preview line + click anywhere on a clip to split
//! - Timeline toolbar: Select, Razor, Split at Playhead, In/Out range marks,
//!   Ripple Cut Range, Delete, Ripple Delete, Snap toggle, Zoom/Fit, +Tracks
//! - Track header controls: Mute, Hide, Lock
//! - Audio waveforms drawn inside audio clips
//! - Captions lane + transition markers + right-click clip context menu
//!
//! Pure painting + hit-testing: every mutation is returned as a [`TimelineAction`]
//! and executed through `Engine::execute` (no UI-only state).

use crate::media_cache::{FilmstripTileRequest, FilmstripView};
use eframe::egui;
use cutpilot_core::{AssetKind, Clip, Project, Selection, Ticks, TrackKind, TICKS_PER_SEC};
use cutpilot_engine::LiveRecordingSnapshot;
use cutpilot_media::waveform::Peak;
use std::collections::HashMap;
use uuid::Uuid;

pub const RULER_HEIGHT: f32 = 26.0;
pub const CAPTION_LANE_HEIGHT: f32 = 22.0;
pub const TRACK_HEIGHT: f32 = 66.0;
pub const TRACK_GAP: f32 = 4.0;
pub const HEADER_WIDTH: f32 = 136.0;
pub const MIN_ZOOM: f32 = 0.0001;
pub const MAX_ZOOM: f32 = 5_000.0;
const EDGE_HIT_PX: f32 = 7.0;
const SNAP_PX: f32 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimelineTool {
    #[default]
    Select,
    Razor,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DragMode {
    Scrub,
    MoveClip {
        clip_id: Uuid,
        grab_offset_ticks: Ticks,
    },
    TrimLeft {
        clip_id: Uuid,
    },
    TrimRight {
        clip_id: Uuid,
    },
    AdjustVolume {
        clip_id: Uuid,
        top_y: f32,
        bottom_y: f32,
    },
}

/// Persistent UI interaction state for the timeline.
pub struct TimelineState {
    pub tool: TimelineTool,
    pub snap: bool,
    pub in_point: Option<Ticks>,
    pub out_point: Option<Ticks>,
    pub drag: Option<DragMode>,
    pub drag_current_ticks: Option<Ticks>,
    pub drag_target_track: Option<Uuid>,
    pub drag_volume: Option<f32>,
    pub snap_guide_ticks: Option<Ticks>,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            tool: TimelineTool::Select,
            snap: true,
            in_point: None,
            out_point: None,
            drag: None,
            drag_current_ticks: None,
            drag_target_track: None,
            drag_volume: None,
            snap_guide_ticks: None,
        }
    }
}

impl TimelineState {
    pub fn is_scrubbing(&self) -> bool {
        matches!(self.drag, Some(DragMode::Scrub))
    }

    /// Ordered (start, end) range when both In and Out are set and distinct.
    pub fn active_range(&self) -> Option<(Ticks, Ticks)> {
        let (a, b) = (self.in_point?, self.out_point?);
        if a == b {
            None
        } else if a < b {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }
}

/// Trim operation emitted when dragging a clip's left or right edge.
pub struct TrimRequest {
    pub clip_id: Uuid,
    pub timeline_start: Option<Ticks>,
    pub source_in: Option<Ticks>,
    pub source_out: Option<Ticks>,
}

/// Actions emitted by one frame of timeline interaction.
#[derive(Default)]
pub struct TimelineAction {
    pub seek: Option<Ticks>,
    pub select_clip: Option<Uuid>,
    pub select_caption: Option<Uuid>,
    /// (clip_id, new_timeline_start, optional_new_track_id)
    pub move_clip: Option<(Uuid, Ticks, Option<Uuid>)>,
    pub trim_clip: Option<TrimRequest>,
    /// (clip_id, split_at_ticks)
    pub split_clip: Option<(Uuid, Ticks)>,
    pub split_at_playhead: bool,
    pub remove_clip: Option<Uuid>,
    pub ripple_delete_clip: Option<Uuid>,
    pub duplicate_clip: Option<Uuid>,
    pub unlink_clip: Option<Uuid>,
    pub add_marker_at: Option<Ticks>,
    pub remove_marker: Option<Uuid>,
    pub ripple_cut_range: Option<(Ticks, Ticks)>,
    pub add_fade_in: Option<Ticks>,
    pub add_fade_out: Option<Ticks>,
    /// (clip_id, fade_in_ticks) for audio clip fade-in
    pub set_audio_fade_in: Option<(Uuid, Ticks)>,
    /// (clip_id, fade_out_ticks) for audio clip fade-out
    pub set_audio_fade_out: Option<(Uuid, Ticks)>,
    /// (clip_id, linear_volume 0.0..=4.0) from dragging an audio clip's volume line or menu
    pub set_clip_volume: Option<(Uuid, f32)>,
    /// (track_id, linear_volume 0.0..=4.0) from an audio track header slider
    pub set_track_volume: Option<(Uuid, f32)>,
    /// Dragged asset from the Media Bin dropped onto a timeline track:
    /// (asset_id, track_id, timeline_start)
    pub drop_asset: Option<(Uuid, Uuid, Ticks)>,
    pub cut_silence_asset: Option<Uuid>,
    pub transcribe_asset: Option<Uuid>,
    pub set_zoom: Option<f32>,
    pub fit_timeline: bool,
    pub add_track: Option<TrackKind>,
    /// Reorder a track to a new 0-based index in `project.tracks`: (track_id, to_index).
    pub move_track: Option<(Uuid, u32)>,
    pub toggle_mute: Option<(Uuid, bool)>,
    pub toggle_hidden: Option<(Uuid, bool)>,
    pub toggle_locked: Option<(Uuid, bool)>,
    pub filmstrip_requests: Vec<FilmstripTileRequest>,
}

pub struct TimelineLayout {
    pub content_width: f32,
    pub zoom: f32,
    pub track_y: HashMap<Uuid, f32>,
    pub tracks_height: f32,
    pub tracks_top_offset: f32,
}

pub fn compute_layout(
    project: &Project,
    available_width: f32,
    live_recording: Option<&LiveRecordingSnapshot>,
) -> TimelineLayout {
    let zoom = project.editor.zoom_pixels_per_second.clamp(MIN_ZOOM, MAX_ZOOM);
    let rec_end = live_recording
        .filter(|r| r.recording)
        .map(|r| r.insert_at_ticks + r.elapsed_ticks + TICKS_PER_SEC * 2)
        .unwrap_or(0);
    let duration_secs = cutpilot_core::secs_from_ticks(
        project.timeline_end().max(rec_end).max(TICKS_PER_SEC * 5),
    );
    let content_width = (duration_secs * zoom as f64) as f32 + 64.0;
    let content_width = content_width.max((available_width - HEADER_WIDTH - 16.0).max(200.0));
    let mut track_y = HashMap::new();
    let tracks_top_offset = RULER_HEIGHT + CAPTION_LANE_HEIGHT + 2.0;
    let mut y = 0.0;
    // Video tracks top-to-bottom (topmost overlay first, then V1), then audio tracks.
    for track in ordered_tracks(project) {
        track_y.insert(track.id, y);
        y += TRACK_HEIGHT + TRACK_GAP;
    }
    TimelineLayout {
        content_width,
        zoom,
        track_y,
        tracks_height: y.max(TRACK_HEIGHT),
        tracks_top_offset,
    }
}

/// Display order: Video tracks top-to-bottom (highest overlay first, V1 bottom),
/// followed by Audio tracks (A1, A2, ...).
fn ordered_tracks(project: &Project) -> Vec<&cutpilot_core::Track> {
    let mut videos: Vec<_> = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .collect();
    videos.reverse();
    let audios: Vec<_> = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio)
        .collect();
    videos.extend(audios);
    videos
}

pub fn draw_timeline(
    ui: &mut egui::Ui,
    project: &Project,
    state: &mut TimelineState,
    waveforms: &HashMap<Uuid, Vec<Peak>>,
    filmstrips: &FilmstripView<'_>,
    dragging_asset: Option<Uuid>,
    live_recording: Option<&LiveRecordingSnapshot>,
) -> TimelineAction {
    let mut action = TimelineAction::default();
    let selected_clip_id = project.editor.selection.iter().find_map(|s| match s {
        Selection::Clip(id) => Some(*id),
        _ => None,
    });

    // ---- Timeline Toolbar ----
    draw_toolbar(ui, project, state, selected_clip_id, &mut action);
    ui.separator();

    let available_width = ui.available_width();
    let layout = compute_layout(project, available_width, live_recording);
    let tracks_ordered = ordered_tracks(project);

    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;

            // ---- Track Headers Column ----
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(24, 26, 32))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(HEADER_WIDTH);
                        // Zero vertical item spacing so header cells match
                        // `layout.tracks_top_offset` and `TRACK_HEIGHT + TRACK_GAP` pixel-for-pixel.
                        ui.spacing_mut().item_spacing.y = 0.0;
                        // Ruler header cell.
                        ui.allocate_ui_with_layout(
                            egui::vec2(HEADER_WIDTH, RULER_HEIGHT),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.add_space(6.0);
                                ui.label(
                                    egui::RichText::new(format_timecode(
                                        project.editor.playhead_ticks,
                                        project.settings.fps,
                                    ))
                                    .monospace()
                                    .strong()
                                    .color(egui::Color32::from_rgb(245, 115, 115)),
                                );
                            },
                        );
                        // Captions lane header.
                        ui.allocate_ui_with_layout(
                            egui::vec2(HEADER_WIDTH, CAPTION_LANE_HEIGHT + 2.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.add_space(6.0);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "CC ({})",
                                        project.captions.len()
                                    ))
                                    .small()
                                    .color(egui::Color32::from_rgb(210, 195, 130)),
                                );
                            },
                        );
                        // Track rows.
                        let video_indices: Vec<(Uuid, usize)> = project
                            .tracks
                            .iter()
                            .enumerate()
                            .filter(|(_, t)| t.kind == TrackKind::Video)
                            .map(|(i, t)| (t.id, i))
                            .collect();
                        let audio_indices: Vec<(Uuid, usize)> = project
                            .tracks
                            .iter()
                            .enumerate()
                            .filter(|(_, t)| t.kind == TrackKind::Audio)
                            .map(|(i, t)| (t.id, i))
                            .collect();

                        for track in &tracks_ordered {
                            let bg = row_color(track.kind);
                            // Determine move-up (closer to front for video / higher row for audio)
                            // and move-down targets in `project.tracks`.
                            let (up_target, down_target) = if track.kind == TrackKind::Video {
                                // Video tracks display reversed: top row = highest index (front).
                                let pos = video_indices.iter().position(|(id, _)| *id == track.id);
                                let up = pos
                                    .and_then(|p| video_indices.get(p + 1))
                                    .map(|(_, idx)| *idx as u32);
                                let down = pos
                                    .and_then(|p| p.checked_sub(1).and_then(|q| video_indices.get(q)))
                                    .map(|(_, idx)| *idx as u32);
                                (up, down)
                            } else {
                                let pos = audio_indices.iter().position(|(id, _)| *id == track.id);
                                let up = pos
                                    .and_then(|p| p.checked_sub(1).and_then(|q| audio_indices.get(q)))
                                    .map(|(_, idx)| *idx as u32);
                                let down = pos
                                    .and_then(|p| audio_indices.get(p + 1))
                                    .map(|(_, idx)| *idx as u32);
                                (up, down)
                            };

                            egui::Frame::new()
                                .fill(bg)
                                .corner_radius(3.0)
                                .inner_margin(egui::Margin::symmetric(6, 4))
                                .show(ui, |ui| {
                                    ui.set_width(HEADER_WIDTH - 12.0);
                                    ui.set_height(TRACK_HEIGHT - 8.0);
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 4.0;
                                        if track.kind == TrackKind::Video {
                                            ui.add_space(16.0);
                                        } else {
                                            ui.add_space(4.0);
                                        }
                                        ui.horizontal(|ui| {
                                            let kind_badge = if track.kind == TrackKind::Video {
                                                egui::Color32::from_rgb(90, 155, 250)
                                            } else {
                                                egui::Color32::from_rgb(80, 200, 135)
                                            };
                                            let hover_role = if track.kind == TrackKind::Video {
                                                if Some(track.id)
                                                    == video_indices.first().map(|(id, _)| *id)
                                                {
                                                    "Base video track (back-most layer)"
                                                } else {
                                                    "Overlay video track (composited in front of tracks below)"
                                                }
                                            } else {
                                                "Audio track (mixed with all unmuted audio tracks)"
                                            };
                                            ui.label(
                                                egui::RichText::new(&track.name)
                                                    .strong()
                                                    .color(kind_badge),
                                            )
                                            .on_hover_text(hover_role);
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.spacing_mut().item_spacing.x = 2.0;
                                                    let lock_col = if track.locked {
                                                        egui::Color32::from_rgb(245, 175, 70)
                                                    } else {
                                                        egui::Color32::from_gray(120)
                                                    };
                                                    if ui
                                                        .add(
                                                            egui::Button::new(
                                                                egui::RichText::new("L")
                                                                    .small()
                                                                    .color(lock_col),
                                                            )
                                                            .min_size(egui::vec2(16.0, 18.0)),
                                                        )
                                                        .on_hover_text("Lock / unlock track")
                                                        .clicked()
                                                    {
                                                        action.toggle_locked =
                                                            Some((track.id, !track.locked));
                                                    }
                                                    if track.kind == TrackKind::Audio {
                                                        let mute_col = if track.muted {
                                                            egui::Color32::from_rgb(245, 95, 95)
                                                        } else {
                                                            egui::Color32::from_gray(145)
                                                        };
                                                        if ui
                                                            .add(
                                                                egui::Button::new(
                                                                    egui::RichText::new("M")
                                                                        .small()
                                                                        .color(mute_col),
                                                                )
                                                                .min_size(egui::vec2(16.0, 18.0)),
                                                            )
                                                            .on_hover_text("Mute / unmute audio track")
                                                            .clicked()
                                                        {
                                                            action.toggle_mute =
                                                                Some((track.id, !track.muted));
                                                        }
                                                    } else {
                                                        let hide_col = if track.hidden {
                                                            egui::Color32::from_rgb(245, 95, 95)
                                                        } else {
                                                            egui::Color32::from_gray(145)
                                                        };
                                                        if ui
                                                            .add(
                                                                egui::Button::new(
                                                                    egui::RichText::new("H")
                                                                        .small()
                                                                        .color(hide_col),
                                                                )
                                                                .min_size(egui::vec2(16.0, 18.0)),
                                                            )
                                                            .on_hover_text("Hide / show video track")
                                                            .clicked()
                                                        {
                                                            action.toggle_hidden =
                                                                Some((track.id, !track.hidden));
                                                        }
                                                    }
                                                    if up_target.is_some() || down_target.is_some() {
                                                        let down_tip =
                                                            if track.kind == TrackKind::Video {
                                                                "Move track down (farther back)"
                                                            } else {
                                                                "Move track down"
                                                            };
                                                        if ui
                                                            .add_enabled(
                                                                down_target.is_some(),
                                                                egui::Button::new(
                                                                    egui::RichText::new("▼")
                                                                        .small()
                                                                        .color(
                                                                            egui::Color32::from_gray(
                                                                                165,
                                                                            ),
                                                                        ),
                                                                )
                                                                .min_size(egui::vec2(15.0, 18.0)),
                                                            )
                                                            .on_hover_text(down_tip)
                                                            .clicked()
                                                        {
                                                            if let Some(to) = down_target {
                                                                action.move_track =
                                                                    Some((track.id, to));
                                                            }
                                                        }
                                                        let up_tip =
                                                            if track.kind == TrackKind::Video {
                                                                "Move track up (closer to front)"
                                                            } else {
                                                                "Move track up"
                                                            };
                                                        if ui
                                                            .add_enabled(
                                                                up_target.is_some(),
                                                                egui::Button::new(
                                                                    egui::RichText::new("▲")
                                                                        .small()
                                                                        .color(
                                                                            egui::Color32::from_gray(
                                                                                165,
                                                                            ),
                                                                        ),
                                                                )
                                                                .min_size(egui::vec2(15.0, 18.0)),
                                                            )
                                                            .on_hover_text(up_tip)
                                                            .clicked()
                                                        {
                                                            if let Some(to) = up_target {
                                                                action.move_track =
                                                                    Some((track.id, to));
                                                            }
                                                        }
                                                    }
                                                },
                                            );
                                        });

                                        // Direct per-track volume slider for every audio track!
                                        if track.kind == TrackKind::Audio {
                                            ui.horizontal(|ui| {
                                                ui.spacing_mut().item_spacing.x = 3.0;
                                                let mut tvol = track.volume;
                                                let icon = if track.muted || tvol <= 0.001 {
                                                    "🔇"
                                                } else {
                                                    "🔊"
                                                };
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new(icon).small(),
                                                        )
                                                        .frame(false),
                                                    )
                                                    .on_hover_text("Click to reset track volume to 100%")
                                                    .clicked()
                                                {
                                                    action.set_track_volume = Some((track.id, 1.0));
                                                }
                                                ui.spacing_mut().slider_width = 72.0;
                                                let resp = ui
                                                    .add_enabled(
                                                        !track.locked,
                                                        egui::Slider::new(&mut tvol, 0.0..=2.0)
                                                            .show_value(false),
                                                    )
                                                    .on_hover_text(format!(
                                                        "{} Track Volume: {} ({})",
                                                        track.name,
                                                        format_volume_pct(track.volume),
                                                        format_volume_db(track.volume)
                                                    ));
                                                if resp.changed() {
                                                    action.set_track_volume =
                                                        Some((track.id, tvol));
                                                }
                                                ui.label(
                                                    egui::RichText::new(format_volume_short(
                                                        track.volume,
                                                    ))
                                                    .monospace()
                                                    .size(9.5)
                                                    .color(ui.visuals().weak_text_color()),
                                                );
                                            });
                                        }
                                    });
                                });
                            ui.add_space(TRACK_GAP);
                        }
                    });
                });

            // ---- Scrollable Track Canvas ----
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                let total = egui::vec2(
                    layout.content_width,
                    layout.tracks_top_offset + layout.tracks_height,
                );
                let (rect, response) =
                    ui.allocate_exact_size(total, egui::Sense::click_and_drag());
                let painter = ui.painter_at(rect);

                let tick_to_x = |t: Ticks| -> f32 {
                    cutpilot_core::secs_from_ticks(t) as f32 * layout.zoom
                };
                let x_to_ticks = |x: f32| -> Ticks {
                    let secs = (x / layout.zoom) as f64;
                    cutpilot_core::ticks_from_secs(secs.max(0.0))
                };

                // Base background.
                painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(20, 22, 27));

                // ---- Ruler ----
                let ruler_rect = egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(layout.content_width, RULER_HEIGHT),
                );
                painter.rect_filled(ruler_rect, 0.0, egui::Color32::from_rgb(28, 31, 38));
                painter.line_segment(
                    [ruler_rect.left_bottom(), ruler_rect.right_bottom()],
                    egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
                );

                let step_secs = ruler_step(layout.zoom);
                let minor_step = step_secs / 5.0;
                let clip_vis = ui.clip_rect();
                let vis_x0 = (clip_vis.left() - rect.left() - 80.0).max(0.0);
                let vis_x1 = (clip_vis.right() - rect.left() + 80.0).min(layout.content_width);
                let start_sec = (vis_x0 / layout.zoom) as f64;
                let mut idx_tick: i64 = (start_sec / minor_step).floor().max(0.0) as i64;
                loop {
                    let sec = idx_tick as f64 * minor_step;
                    let x = (sec * layout.zoom as f64) as f32;
                    if x > vis_x1 {
                        break;
                    }
                    let is_major = idx_tick % 5 == 0;
                    let h = if is_major { 10.0 } else { 5.0 };
                    painter.line_segment(
                        [
                            egui::pos2(rect.left() + x, rect.top() + RULER_HEIGHT - h),
                            egui::pos2(rect.left() + x, rect.top() + RULER_HEIGHT),
                        ],
                        egui::Stroke::new(
                            1.0,
                            if is_major {
                                egui::Color32::from_gray(135)
                            } else {
                                egui::Color32::from_gray(68)
                            },
                        ),
                    );
                    if is_major {
                        let small = egui::FontId::monospace(10.5);
                        painter.text(
                            egui::pos2(rect.left() + x + 3.0, rect.top() + 3.0),
                            egui::Align2::LEFT_TOP,
                            format_seconds_short(sec),
                            small,
                            egui::Color32::from_gray(185),
                        );
                    }
                    idx_tick += 1;
                }

                // ---- Captions Lane ----
                let cap_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.left(), rect.top() + RULER_HEIGHT + 1.0),
                    egui::vec2(layout.content_width, CAPTION_LANE_HEIGHT),
                );
                painter.rect_filled(cap_rect, 2.0, egui::Color32::from_rgb(28, 28, 34));
                let mut caption_rects: Vec<(Uuid, egui::Rect)> = Vec::new();
                for caption in &project.captions {
                    let cx0 = rect.left() + tick_to_x(caption.start);
                    let cx1 = (rect.left() + tick_to_x(caption.end)).max(cx0 + 8.0);
                    let r = egui::Rect::from_min_max(
                        egui::pos2(cx0, cap_rect.top() + 2.0),
                        egui::pos2(cx1, cap_rect.bottom() - 2.0),
                    );
                    let is_sel = project
                        .editor
                        .selection
                        .iter()
                        .any(|s| matches!(s, Selection::Caption(id) if *id == caption.id));
                    painter.rect_filled(
                        r,
                        3.0,
                        if is_sel {
                            egui::Color32::from_rgb(185, 145, 50)
                        } else {
                            egui::Color32::from_rgb(120, 96, 40)
                        },
                    );
                    if r.width() > 26.0 {
                        let clipped = ui.painter_at(r);
                        clipped.text(
                            r.left_center() + egui::vec2(4.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            &caption.text,
                            egui::FontId::proportional(10.5),
                            egui::Color32::WHITE,
                        );
                    }
                    caption_rects.push((caption.id, r));
                }

                // ---- Track Rows ----
                let mut track_rects: Vec<(Uuid, TrackKind, egui::Rect)> = Vec::new();
                for track in &tracks_ordered {
                    let y = layout.track_y[&track.id];
                    let tr = egui::Rect::from_min_size(
                        egui::pos2(rect.left(), rect.top() + layout.tracks_top_offset + y),
                        egui::vec2(layout.content_width, TRACK_HEIGHT),
                    );
                    let mut fill = row_color(track.kind);
                    if track.muted || track.hidden {
                        fill = fill.linear_multiply(0.55);
                    }
                    painter.rect_filled(tr, 3.0, fill);
                    track_rects.push((track.id, track.kind, tr));
                }

                // ---- In / Out Range Highlight ----
                if let Some((rs, re)) = state.active_range() {
                    let rx0 = rect.left() + tick_to_x(rs);
                    let rx1 = rect.left() + tick_to_x(re);
                    let range_rect = egui::Rect::from_min_max(
                        egui::pos2(rx0, rect.top()),
                        egui::pos2(rx1, rect.bottom()),
                    );
                    painter.rect_filled(
                        range_rect,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(245, 180, 60, 34),
                    );
                    painter.line_segment(
                        [range_rect.left_top(), range_rect.left_bottom()],
                        egui::Stroke::new(1.5, egui::Color32::from_rgb(245, 190, 70)),
                    );
                    painter.line_segment(
                        [range_rect.right_top(), range_rect.right_bottom()],
                        egui::Stroke::new(1.5, egui::Color32::from_rgb(245, 190, 70)),
                    );
                }

                // ---- Clips ----
                let selected_clips: Vec<Uuid> = project
                    .editor
                    .selection
                    .iter()
                    .filter_map(|s| match s {
                        Selection::Clip(id) => Some(*id),
                        _ => None,
                    })
                    .collect();

                let hover_pos = response.hover_pos();
                let mut clip_rects: Vec<(Uuid, egui::Rect)> = Vec::new();
                // (clip_id, top_y, bottom_y, line_y, x0, x1) for audio clip volume lines
                let mut audio_vol_handles: Vec<(Uuid, f32, f32, f32, f32, f32)> = Vec::new();
                let mut hovered_edge_cursor = false;
                let mut hovered_volume_cursor = false;

                for clip in &project.clips {
                    let Some(track) = project.track(&clip.track_id) else {
                        continue;
                    };
                    let y = layout.track_y[&track.id];
                    let x0 = rect.left() + tick_to_x(clip.timeline_start);
                    let x1 = (rect.left() + tick_to_x(clip.timeline_end())).max(x0 + 4.0);
                    let clip_rect = egui::Rect::from_min_max(
                        egui::pos2(x0, rect.top() + layout.tracks_top_offset + y + 2.0),
                        egui::pos2(x1, rect.top() + layout.tracks_top_offset + y + TRACK_HEIGHT - 2.0),
                    );
                    let is_sel = selected_clips.contains(&clip.id);
                    let is_gen = project
                        .asset(&clip.asset_id)
                        .map(|a| a.generated.is_some())
                        .unwrap_or(false);

                    let mut fill = clip_fill(track.kind, is_gen);
                    if is_sel {
                        fill = brighten(fill, 28);
                    }
                    if track.muted || track.hidden {
                        fill = fill.linear_multiply(0.5);
                    }
                    painter.rect_filled(clip_rect, 4.0, fill);

                    // Top header accent strip on each clip.
                    let strip_rect = egui::Rect::from_min_max(
                        clip_rect.left_top(),
                        egui::pos2(clip_rect.right(), (clip_rect.top() + 16.0).min(clip_rect.bottom())),
                    );
                    painter.rect_filled(
                        strip_rect,
                        egui::CornerRadius {
                            nw: 4,
                            ne: 4,
                            sw: 0,
                            se: 0,
                        },
                        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 105),
                    );

                    // Video filmstrip preview inside video clips.
                    if track.kind == TrackKind::Video {
                        if let Some(asset) = project.asset(&clip.asset_id) {
                            draw_clip_filmstrip(
                                ui,
                                clip_rect,
                                clip_vis,
                                clip,
                                asset,
                                layout.zoom,
                                track.hidden,
                                filmstrips,
                                &mut action.filmstrip_requests,
                            );
                        }
                    }

                    // Detailed audio waveform + draggable horizontal volume line inside audio clips.
                    let active_drag_vol = match state.drag {
                        Some(DragMode::AdjustVolume { clip_id, .. }) if clip_id == clip.id => {
                            state.drag_volume
                        }
                        _ => None,
                    };
                    let effective_clip_vol = active_drag_vol.unwrap_or(clip.volume);
                    if track.kind == TrackKind::Audio {
                        if let (Some(peaks), Some(asset)) =
                            (waveforms.get(&clip.asset_id), project.asset(&clip.asset_id))
                        {
                            let mut preview_clip = clip.clone();
                            preview_clip.volume = (effective_clip_vol * track.volume).clamp(0.0, 4.0);
                            draw_clip_waveform(
                                &painter,
                                clip_rect,
                                clip_vis,
                                &preview_clip,
                                asset.editable_duration(),
                                peaks,
                            );
                        }

                        // Draggable horizontal volume rubber-band line (0% at bottom, 100% mid, 200% top).
                        let wf_top = clip_rect.top() + 18.0;
                        let wf_bot = clip_rect.bottom() - 4.0;
                        if wf_bot > wf_top + 8.0 && clip_rect.width() > 14.0 {
                            let line_y = volume_to_y(effective_clip_vol, wf_top, wf_bot);
                            let is_dragging_this = active_drag_vol.is_some();
                            let is_hovering_line = state.tool == TimelineTool::Select
                                && !track.locked
                                && hover_pos
                                    .map(|hp| {
                                        hp.x >= clip_rect.left() + EDGE_HIT_PX
                                            && hp.x <= clip_rect.right() - EDGE_HIT_PX
                                            && (hp.y - line_y).abs() <= 5.5
                                    })
                                    .unwrap_or(false);
                            if is_hovering_line {
                                hovered_volume_cursor = true;
                            }
                            let line_col = if is_dragging_this || is_hovering_line {
                                egui::Color32::from_rgb(255, 245, 165)
                            } else if effective_clip_vol <= 0.001 {
                                egui::Color32::from_rgba_unmultiplied(245, 95, 95, 210)
                            } else {
                                egui::Color32::from_rgba_unmultiplied(242, 222, 130, 175)
                            };
                            let stroke_w = if is_dragging_this || is_hovering_line {
                                2.4
                            } else {
                                1.3
                            };
                            painter.line_segment(
                                [
                                    egui::pos2(clip_rect.left() + 2.0, line_y),
                                    egui::pos2(clip_rect.right() - 2.0, line_y),
                                ],
                                egui::Stroke::new(stroke_w, line_col),
                            );
                            if is_dragging_this || is_hovering_line {
                                let pill_txt = format!(
                                    "🔊 {} ({})",
                                    format_volume_pct(effective_clip_vol),
                                    format_volume_db(effective_clip_vol)
                                );
                                let raw_x = hover_pos
                                    .map(|p| p.x)
                                    .unwrap_or(clip_rect.center().x);
                                let min_x = clip_rect.left() + 42.0;
                                let max_x = clip_rect.right() - 42.0;
                                let anchor_x = if min_x < max_x {
                                    raw_x.clamp(min_x, max_x)
                                } else {
                                    clip_rect.center().x
                                };
                                let pill = egui::Rect::from_center_size(
                                    egui::pos2(anchor_x, (line_y - 13.0).max(rect.top() + 12.0)),
                                    egui::vec2(120.0, 17.0),
                                );
                                painter.rect_filled(
                                    pill,
                                    4.0,
                                    egui::Color32::from_rgba_unmultiplied(16, 20, 28, 235),
                                );
                                painter.rect_stroke(
                                    pill,
                                    4.0,
                                    egui::Stroke::new(1.0, line_col),
                                    egui::StrokeKind::Inside,
                                );
                                painter.text(
                                    pill.center(),
                                    egui::Align2::CENTER_CENTER,
                                    pill_txt,
                                    egui::FontId::monospace(10.0),
                                    egui::Color32::WHITE,
                                );
                            }
                            if !track.locked {
                                audio_vol_handles.push((
                                    clip.id,
                                    wf_top,
                                    wf_bot,
                                    line_y,
                                    clip_rect.left() + EDGE_HIT_PX,
                                    clip_rect.right() - EDGE_HIT_PX,
                                ));
                            }
                        }
                    }

                    // Fades.
                    if clip.fade_in_ticks > 0 {
                        let w = tick_to_x(clip.fade_in_ticks).min(clip_rect.width());
                        painter.line_segment(
                            [
                                clip_rect.left_bottom() - egui::vec2(0.0, 2.0),
                                egui::pos2(clip_rect.left() + w, clip_rect.top() + 16.0),
                            ],
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(245, 225, 110)),
                        );
                    }
                    if clip.fade_out_ticks > 0 {
                        let w = tick_to_x(clip.fade_out_ticks).min(clip_rect.width());
                        painter.line_segment(
                            [
                                egui::pos2(clip_rect.right() - w, clip_rect.top() + 16.0),
                                clip_rect.right_bottom() - egui::vec2(0.0, 2.0),
                            ],
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(245, 225, 110)),
                        );
                    }

                    // Label: asset name + duration + link indicator.
                    if clip_rect.width() > 30.0 {
                        let clipped = ui.painter_at(clip_rect);
                        let name = project
                            .asset(&clip.asset_id)
                            .map(|a| a.name.as_str())
                            .unwrap_or("clip");
                        let mut title = if clip.link_group.is_some() {
                            format!("🔗 {name}")
                        } else {
                            name.to_string()
                        };
                        if clip.motion.is_some() {
                            title.push_str(" 🔍");
                        }
                        if !clip.color.is_neutral() {
                            title.push_str(" 🎨");
                        }
                        if track.kind == TrackKind::Audio && (effective_clip_vol - 1.0).abs() > 0.02 {
                            if effective_clip_vol <= 0.001 {
                                title.push_str(" · 🔇");
                            } else {
                                title.push_str(&format!(
                                    " · 🔊 {}",
                                    format_volume_db(effective_clip_vol)
                                ));
                            }
                        }
                        clipped.text(
                            clip_rect.left_top() + egui::vec2(5.0, 1.5),
                            egui::Align2::LEFT_TOP,
                            title,
                            egui::FontId::proportional(11.0),
                            egui::Color32::WHITE,
                        );
                        if clip_rect.width() > 54.0 {
                            let speed_tag = if (clip.effective_speed() - 1.0).abs() > 0.01 {
                                format!("{:.2}× · ", clip.effective_speed())
                            } else {
                                String::new()
                            };
                            let dur_text = format!(
                                "{speed_tag}{:.2}s",
                                cutpilot_core::secs_from_ticks(clip.timeline_duration())
                            );
                            let badge_w = (dur_text.len() as f32 * 6.2 + 8.0).min(clip_rect.width() - 6.0);
                            let badge_rect = egui::Rect::from_min_max(
                                egui::pos2(clip_rect.right() - badge_w - 3.0, clip_rect.bottom() - 15.0),
                                egui::pos2(clip_rect.right() - 3.0, clip_rect.bottom() - 2.0),
                            );
                            clipped.rect_filled(
                                badge_rect,
                                3.0,
                                egui::Color32::from_rgba_unmultiplied(12, 14, 18, 175),
                            );
                            clipped.text(
                                clip_rect.right_bottom() - egui::vec2(6.0, 2.5),
                                egui::Align2::RIGHT_BOTTOM,
                                dur_text,
                                egui::FontId::monospace(10.0),
                                egui::Color32::from_rgba_unmultiplied(240, 244, 255, 235),
                            );
                        }
                    }

                    // Border & hover trim handles.
                    let stroke = if is_sel {
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 255, 255))
                    } else {
                        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 45))
                    };
                    painter.rect_stroke(clip_rect, 4.0, stroke, egui::StrokeKind::Inside);

                    if state.tool == TimelineTool::Select {
                        if let Some(hp) = hover_pos {
                            if clip_rect.contains(hp) && clip_rect.width() > 14.0 {
                                let near_left = (hp.x - clip_rect.left()).abs() <= EDGE_HIT_PX;
                                let near_right = (hp.x - clip_rect.right()).abs() <= EDGE_HIT_PX;
                                if near_left {
                                    hovered_edge_cursor = true;
                                    let h = egui::Rect::from_min_max(
                                        clip_rect.left_top(),
                                        egui::pos2(clip_rect.left() + 5.0, clip_rect.bottom()),
                                    );
                                    painter.rect_filled(h, 2.0, egui::Color32::from_rgb(255, 220, 90));
                                } else if near_right {
                                    hovered_edge_cursor = true;
                                    let h = egui::Rect::from_min_max(
                                        egui::pos2(clip_rect.right() - 5.0, clip_rect.top()),
                                        clip_rect.right_bottom(),
                                    );
                                    painter.rect_filled(h, 2.0, egui::Color32::from_rgb(255, 220, 90));
                                }
                            }
                        }
                    }

                    clip_rects.push((clip.id, clip_rect));
                }

                // ---- Live Voiceover Recording Clip Overlay ----
                if let Some(rec) = live_recording.filter(|r| r.recording) {
                    let target_track_id = rec
                        .target_track_id
                        .filter(|id| layout.track_y.contains_key(id))
                        .or_else(|| {
                            tracks_ordered
                                .iter()
                                .rev()
                                .find(|t| t.kind == TrackKind::Audio)
                                .map(|t| t.id)
                        });
                    if let Some(tid) = target_track_id {
                        if let Some(&y) = layout.track_y.get(&tid) {
                            let rx0 = rect.left() + tick_to_x(rec.insert_at_ticks);
                            let rx1 = (rect.left()
                                + tick_to_x(rec.insert_at_ticks + rec.elapsed_ticks))
                            .max(rx0 + 18.0);
                            let rec_rect = egui::Rect::from_min_max(
                                egui::pos2(rx0, rect.top() + layout.tracks_top_offset + y + 2.0),
                                egui::pos2(
                                    rx1,
                                    rect.top() + layout.tracks_top_offset + y + TRACK_HEIGHT - 2.0,
                                ),
                            );
                            painter.rect_filled(
                                rec_rect,
                                4.0,
                                egui::Color32::from_rgba_unmultiplied(175, 36, 44, 215),
                            );
                            // Draw live recorded waveform inside the growing clip!
                            if !rec.waveform_peaks.is_empty() && rec_rect.width() > 8.0 {
                                let wave_top = rec_rect.top() + 17.0;
                                let wave_bot = rec_rect.bottom() - 4.0;
                                let mid_y = (wave_top + wave_bot) * 0.5;
                                let half_h = ((wave_bot - wave_top) * 0.48).max(2.0);
                                let bars = ((rec_rect.width() - 4.0) / 2.0).floor().max(1.0) as usize;
                                let peaks = &rec.waveform_peaks;
                                for b in 0..bars {
                                    let i0 = (b * peaks.len()) / bars;
                                    let i1 = (((b + 1) * peaks.len()) / bars)
                                        .max(i0 + 1)
                                        .min(peaks.len());
                                    let mut amp = 0.0_f32;
                                    for &p in &peaks[i0..i1] {
                                        amp = amp.max(p);
                                    }
                                    let h = (amp.clamp(0.0, 1.0).powf(0.65) * half_h).max(1.0);
                                    let x = rec_rect.left() + 2.0 + b as f32 * 2.0 + 0.5;
                                    painter.line_segment(
                                        [egui::pos2(x, mid_y - h), egui::pos2(x, mid_y + h)],
                                        egui::Stroke::new(
                                            1.4,
                                            egui::Color32::from_rgb(255, 195, 195),
                                        ),
                                    );
                                }
                            }
                            painter.rect_stroke(
                                rec_rect,
                                4.0,
                                egui::Stroke::new(1.8, egui::Color32::from_rgb(255, 110, 110)),
                                egui::StrokeKind::Inside,
                            );
                            let clipped = ui.painter_at(rec_rect);
                            clipped.text(
                                rec_rect.left_top() + egui::vec2(5.0, 2.0),
                                egui::Align2::LEFT_TOP,
                                format!("● REC {} ({:.1}s)", rec.name, rec.elapsed_secs),
                                egui::FontId::proportional(11.0),
                                egui::Color32::WHITE,
                            );
                        }
                    }
                }

                // ---- Live Ghost Preview During Move / Trim ----
                draw_drag_ghost(
                    &painter,
                    rect,
                    &layout,
                    project,
                    state,
                    &tick_to_x,
                );

                // ---- Live Drop Preview When Dragging an Asset from the Media Bin ----
                if let Some(asset_id) = dragging_asset {
                    let pointer_pos = ui.input(|i| i.pointer.hover_pos().or(i.pointer.latest_pos()));
                    if let (Some(p), Some(asset)) = (pointer_pos, project.asset(&asset_id)) {
                        if rect.contains(p) {
                            if let Some((tid, tkind, tr_rect)) = track_rects
                                .iter()
                                .find(|(_, _, r)| p.y >= r.top() && p.y <= r.bottom())
                                .copied()
                            {
                                let compatible = asset_compatible_with_track(asset, tkind);
                                let track_locked = project
                                    .track(&tid)
                                    .map(|t| t.locked)
                                    .unwrap_or(true);
                                let raw_t = x_to_ticks((p.x - rect.left()).max(0.0));
                                let (drop_start, guide) =
                                    snap_ticks(raw_t, project, layout.zoom, state.snap, None);
                                let dur = asset.editable_duration().max(TICKS_PER_SEC / 10);
                                let drop_end = drop_start + dur;
                                let overlaps = project.clips.iter().any(|c| {
                                    c.track_id == tid
                                        && drop_start < c.timeline_end()
                                        && c.timeline_start < drop_end
                                });
                                let valid = compatible && !track_locked && !overlaps;

                                if let Some(gt) = guide {
                                    let gx = rect.left() + tick_to_x(gt);
                                    painter.line_segment(
                                        [egui::pos2(gx, rect.top()), egui::pos2(gx, rect.bottom())],
                                        egui::Stroke::new(
                                            1.5,
                                            egui::Color32::from_rgb(90, 225, 255),
                                        ),
                                    );
                                }

                                let gx0 = rect.left() + tick_to_x(drop_start);
                                let gx1 = (rect.left() + tick_to_x(drop_end)).max(gx0 + 8.0);
                                let g_rect = egui::Rect::from_min_max(
                                    egui::pos2(gx0, tr_rect.top() + 2.0),
                                    egui::pos2(gx1, tr_rect.bottom() - 2.0),
                                );
                                let (fill, stroke_col) = if !valid {
                                    (
                                        egui::Color32::from_rgba_unmultiplied(235, 75, 75, 70),
                                        egui::Color32::from_rgb(245, 95, 95),
                                    )
                                } else if tkind == TrackKind::Audio {
                                    (
                                        egui::Color32::from_rgba_unmultiplied(70, 210, 140, 85),
                                        egui::Color32::from_rgb(100, 235, 165),
                                    )
                                } else {
                                    (
                                        egui::Color32::from_rgba_unmultiplied(95, 185, 255, 85),
                                        egui::Color32::from_rgb(120, 215, 255),
                                    )
                                };
                                painter.rect_filled(g_rect, 4.0, fill);
                                painter.rect_stroke(
                                    g_rect,
                                    4.0,
                                    egui::Stroke::new(2.0, stroke_col),
                                    egui::StrokeKind::Inside,
                                );
                                let hint = if !compatible {
                                    if asset.kind == AssetKind::Audio {
                                        "Drop on an Audio track (A1, A2…)".to_string()
                                    } else {
                                        "Incompatible track".to_string()
                                    }
                                } else if track_locked {
                                    "Track is locked".to_string()
                                } else if overlaps {
                                    "Overlaps existing clip".to_string()
                                } else {
                                    format!(
                                        "+ {} @ {}",
                                        asset.name,
                                        format_timecode(drop_start, project.settings.fps)
                                    )
                                };
                                painter.text(
                                    g_rect.left_top() + egui::vec2(4.0, -14.0),
                                    egui::Align2::LEFT_TOP,
                                    hint,
                                    egui::FontId::monospace(10.5),
                                    stroke_col,
                                );

                                if ui.input(|i| i.pointer.any_released()) && valid {
                                    action.drop_asset = Some((asset_id, tid, drop_start));
                                }
                            }
                        }
                    }
                }

                // ---- Snap Guide Line ----
                if let Some(snap_t) = state.snap_guide_ticks {
                    let sx = rect.left() + tick_to_x(snap_t);
                    painter.line_segment(
                        [egui::pos2(sx, rect.top()), egui::pos2(sx, rect.bottom())],
                        egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 225, 255)),
                    );
                }

                // ---- Razor Hover Cut Line ----
                if state.tool == TimelineTool::Razor {
                    if let Some(hp) = hover_pos {
                        if rect.contains(hp) && hp.y >= rect.top() + layout.tracks_top_offset {
                            let raw_t = x_to_ticks(hp.x - rect.left());
                            let cut_t = snap_ticks(raw_t, project, layout.zoom, state.snap, None).0;
                            let cx = rect.left() + tick_to_x(cut_t);
                            painter.line_segment(
                                [
                                    egui::pos2(cx, rect.top() + layout.tracks_top_offset),
                                    egui::pos2(cx, rect.bottom()),
                                ],
                                egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 95, 95)),
                            );
                            painter.text(
                                egui::pos2(cx + 5.0, hp.y - 16.0),
                                egui::Align2::LEFT_TOP,
                                format!("✂ {}", format_timecode(cut_t, project.settings.fps)),
                                egui::FontId::monospace(10.5),
                                egui::Color32::from_rgb(255, 160, 160),
                            );
                        }
                    }
                }

                // ---- Transitions Markers ----
                for tr in &project.transitions {
                    let x = rect.left() + tick_to_x(tr.at);
                    let half = (tick_to_x(tr.duration) * 0.5).clamp(6.0, 20.0);
                    let cy = rect.top() + layout.tracks_top_offset + 6.0;
                    let pts = [
                        egui::pos2(x - half, cy - 5.0),
                        egui::pos2(x + half, cy - 5.0),
                        egui::pos2(x, cy + 7.0),
                    ];
                    painter.add(egui::Shape::convex_polygon(
                        pts.to_vec(),
                        egui::Color32::from_rgb(255, 175, 65),
                        egui::Stroke::new(1.0, egui::Color32::BLACK),
                    ));
                }

                // ---- Timeline Markers (Ruler Flags) ----
                let mut marker_rects: Vec<(Uuid, Ticks, String, egui::Rect)> = Vec::new();
                for m in &project.markers {
                    let mx = rect.left() + tick_to_x(m.at);
                    let col = parse_hex_color(&m.color);
                    painter.line_segment(
                        [
                            egui::pos2(mx, rect.top() + RULER_HEIGHT),
                            egui::pos2(mx, rect.bottom()),
                        ],
                        egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 85),
                        ),
                    );
                    let flag_w = ((m.label.len() as f32 * 6.0) + 12.0).clamp(18.0, 96.0);
                    let flag_rect = egui::Rect::from_min_max(
                        egui::pos2(mx, rect.top() + 2.0),
                        egui::pos2(mx + flag_w, rect.top() + 17.0),
                    );
                    painter.rect_filled(flag_rect, 3.0, col);
                    let clipped = ui.painter_at(flag_rect);
                    clipped.text(
                        flag_rect.left_center() + egui::vec2(4.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        &m.label,
                        egui::FontId::proportional(10.0),
                        egui::Color32::from_rgb(15, 16, 20),
                    );
                    marker_rects.push((m.id, m.at, m.label.clone(), flag_rect));
                }

                // ---- Playhead ----
                let playhead_x = rect.left() + tick_to_x(project.editor.playhead_ticks);
                painter.line_segment(
                    [egui::pos2(playhead_x, rect.top()), egui::pos2(playhead_x, rect.bottom())],
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 78, 78)),
                );
                let head_pts = vec![
                    egui::pos2(playhead_x - 6.0, rect.top()),
                    egui::pos2(playhead_x + 6.0, rect.top()),
                    egui::pos2(playhead_x + 6.0, rect.top() + 10.0),
                    egui::pos2(playhead_x, rect.top() + 17.0),
                    egui::pos2(playhead_x - 6.0, rect.top() + 10.0),
                ];
                painter.add(egui::Shape::convex_polygon(
                    head_pts,
                    egui::Color32::from_rgb(255, 78, 78),
                    egui::Stroke::NONE,
                ));

                // ---- Cursor feedback ----
                if state.tool == TimelineTool::Razor && response.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                } else if hovered_volume_cursor
                    || matches!(state.drag, Some(DragMode::AdjustVolume { .. }))
                {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                } else if hovered_edge_cursor
                    || matches!(state.drag, Some(DragMode::TrimLeft { .. } | DragMode::TrimRight { .. }))
                {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                } else if matches!(state.drag, Some(DragMode::MoveClip { .. })) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                }

                // ---- Pointer Interaction ----
                let x_of = |p: egui::Pos2| p.x - rect.left();
                let in_ruler = |p: egui::Pos2| p.y - rect.top() < RULER_HEIGHT;

                if response.drag_started_by(egui::PointerButton::Primary) {
                    if let Some(p) = hover_pos {
                        if in_ruler(p) || state.tool == TimelineTool::Razor {
                            state.drag = Some(DragMode::Scrub);
                            action.seek = Some(x_to_ticks(x_of(p)));
                        } else {
                            // Hit-test audio clip volume rubber-band lines first, then clip edges/body.
                            let mut hit = None;
                            for &(cid, top_y, bot_y, line_y, x0, x1) in &audio_vol_handles {
                                if p.x >= x0 && p.x <= x1 && (p.y - line_y).abs() <= 5.5 {
                                    hit = Some(DragMode::AdjustVolume {
                                        clip_id: cid,
                                        top_y,
                                        bottom_y: bot_y,
                                    });
                                    state.drag_volume =
                                        Some(y_to_volume(p.y, top_y, bot_y));
                                    action.select_clip = Some(cid);
                                    break;
                                }
                            }
                            if hit.is_none() {
                                for (id, r) in &clip_rects {
                                    if r.contains(p) {
                                        let near_left =
                                            r.width() > 14.0 && (p.x - r.left()).abs() <= EDGE_HIT_PX;
                                        let near_right =
                                            r.width() > 14.0 && (p.x - r.right()).abs() <= EDGE_HIT_PX;
                                        if near_left {
                                            hit = Some(DragMode::TrimLeft { clip_id: *id });
                                        } else if near_right {
                                            hit = Some(DragMode::TrimRight { clip_id: *id });
                                        } else {
                                            let grab = x_to_ticks(x_of(p))
                                                - project
                                                    .clip(id)
                                                    .map(|c: &Clip| c.timeline_start)
                                                    .unwrap_or(0);
                                            hit = Some(DragMode::MoveClip {
                                                clip_id: *id,
                                                grab_offset_ticks: grab,
                                            });
                                        }
                                        action.select_clip = Some(*id);
                                        break;
                                    }
                                }
                            }
                            if let Some(mode) = hit {
                                state.drag = Some(mode);
                                state.drag_current_ticks = Some(x_to_ticks(x_of(p)));
                                state.drag_target_track = None;
                            } else {
                                state.drag = Some(DragMode::Scrub);
                                action.seek = Some(x_to_ticks(x_of(p)));
                            }
                        }
                    }
                }

                if response.dragged_by(egui::PointerButton::Primary) {
                    if let Some(p) = hover_pos {
                        let raw_t = x_to_ticks(x_of(p));
                        match state.drag {
                            Some(DragMode::Scrub) => {
                                action.seek = Some(raw_t);
                                state.snap_guide_ticks = None;
                            }
                            Some(DragMode::MoveClip {
                                clip_id,
                                grab_offset_ticks,
                            }) => {
                                let desired = (raw_t - grab_offset_ticks).max(0);
                                let (snapped, guide) =
                                    snap_ticks(desired, project, layout.zoom, state.snap, Some(clip_id));
                                state.drag_current_ticks = Some(snapped);
                                state.snap_guide_ticks = guide;
                                // Check vertical track targeting for compatible tracks.
                                if let Some(clip) = project.clip(&clip_id) {
                                    let cur_kind = project
                                        .track(&clip.track_id)
                                        .map(|t| t.kind);
                                    state.drag_target_track = track_rects
                                        .iter()
                                        .find(|(_, k, r)| {
                                            Some(*k) == cur_kind && p.y >= r.top() && p.y <= r.bottom()
                                        })
                                        .map(|(tid, _, _)| *tid);
                                }
                            }
                            Some(DragMode::TrimLeft { clip_id })
                            | Some(DragMode::TrimRight { clip_id }) => {
                                let (snapped, guide) =
                                    snap_ticks(raw_t, project, layout.zoom, state.snap, Some(clip_id));
                                state.drag_current_ticks = Some(snapped);
                                state.snap_guide_ticks = guide;
                            }
                            Some(DragMode::AdjustVolume { top_y, bottom_y, .. }) => {
                                state.drag_volume = Some(y_to_volume(p.y, top_y, bottom_y));
                                state.snap_guide_ticks = None;
                            }
                            None => {}
                        }
                    }
                }

                if response.drag_stopped_by(egui::PointerButton::Primary) {
                    if let (Some(mode), Some(target_t)) =
                        (state.drag.take(), state.drag_current_ticks.take())
                    {
                        let target_track = state.drag_target_track.take();
                        let frame = project.settings.fps.frame_duration_ticks().max(1);
                        match mode {
                            DragMode::Scrub => {}
                            DragMode::MoveClip { clip_id, .. } => {
                                if let Some(clip) = project.clip(&clip_id) {
                                    let new_start = target_t.max(0);
                                    let changed_track =
                                        target_track.filter(|t| *t != clip.track_id);
                                    if new_start != clip.timeline_start || changed_track.is_some() {
                                        action.move_clip = Some((clip_id, new_start, changed_track));
                                    }
                                }
                            }
                            DragMode::TrimLeft { clip_id } => {
                                if let Some(clip) = project.clip(&clip_id) {
                                    let max_start = clip.timeline_end() - frame;
                                    let max_left_tl = ((clip.source_in as f64)
                                        / clip.effective_speed() as f64)
                                        .floor() as Ticks;
                                    let min_start = (clip.timeline_start - max_left_tl).max(0);
                                    let clamped = target_t.clamp(min_start, max_start);
                                    let tl_delta = clamped - clip.timeline_start;
                                    let src_delta = ((tl_delta as f64) * clip.effective_speed() as f64).round() as Ticks;
                                    if tl_delta != 0 {
                                        action.trim_clip = Some(TrimRequest {
                                            clip_id,
                                            timeline_start: Some(clamped),
                                            source_in: Some((clip.source_in + src_delta).clamp(0, clip.source_out - 1)),
                                            source_out: None,
                                        });
                                    }
                                }
                            }
                            DragMode::TrimRight { clip_id } => {
                                if let Some(clip) = project.clip(&clip_id) {
                                    let asset_max = project
                                        .asset(&clip.asset_id)
                                        .map(|a| a.editable_duration())
                                        .unwrap_or(clip.source_out);
                                    let min_end = clip.timeline_start + frame;
                                    let max_tl_dur = (((asset_max - clip.source_in) as f64)
                                        / clip.effective_speed() as f64)
                                        .round() as Ticks;
                                    let max_end = clip.timeline_start + max_tl_dur.max(frame);
                                    let clamped_end = target_t.clamp(min_end, max_end);
                                    let new_src_dur =
                                        clip.source_offset_for_timeline_delta(clamped_end - clip.timeline_start);
                                    let new_out = (clip.source_in + new_src_dur).clamp(clip.source_in + 1, asset_max);
                                    if new_out != clip.source_out {
                                        action.trim_clip = Some(TrimRequest {
                                            clip_id,
                                            timeline_start: None,
                                            source_in: None,
                                            source_out: Some(new_out),
                                        });
                                    }
                                }
                            }
                            DragMode::AdjustVolume { clip_id, .. } => {
                                if let Some(vol) = state.drag_volume.take() {
                                    action.set_clip_volume = Some((clip_id, vol));
                                }
                            }
                        }
                    } else {
                        state.drag = None;
                        state.drag_current_ticks = None;
                        state.drag_target_track = None;
                        state.drag_volume = None;
                    }
                    state.snap_guide_ticks = None;
                }

                if response.clicked_by(egui::PointerButton::Primary) {
                    if let Some(p) = hover_pos {
                        if let Some((_, m_at, _, _)) =
                            marker_rects.iter().find(|(_, _, _, r)| r.contains(p))
                        {
                            action.seek = Some(*m_at);
                        } else if in_ruler(p) {
                            action.seek = Some(x_to_ticks(x_of(p)));
                        } else if state.tool == TimelineTool::Razor {
                            let raw_t = x_to_ticks(x_of(p));
                            let cut_t =
                                snap_ticks(raw_t, project, layout.zoom, state.snap, None).0;
                            for (id, r) in &clip_rects {
                                if r.contains(p) {
                                    action.split_clip = Some((*id, cut_t));
                                    break;
                                }
                            }
                        } else {
                            // Check captions first, then clips, then empty space seek.
                            let mut handled = false;
                            for (cid, cr) in &caption_rects {
                                if cr.contains(p) {
                                    action.select_caption = Some(*cid);
                                    handled = true;
                                    break;
                                }
                            }
                            if !handled {
                                for (id, r) in &clip_rects {
                                    if r.contains(p) {
                                        action.select_clip = Some(*id);
                                        handled = true;
                                        break;
                                    }
                                }
                            }
                            if !handled {
                                action.seek = Some(x_to_ticks(x_of(p)));
                            }
                        }
                    }
                    state.drag = None;
                }

                // ---- Right-click Context Menu on Clips / Markers ----
                let ctx_marker = hover_pos.and_then(|p| {
                    marker_rects
                        .iter()
                        .find(|(_, _, _, r)| r.contains(p))
                        .map(|(id, _, label, _)| (*id, label.clone()))
                });
                let ctx_clip_id = hover_pos.and_then(|p| {
                    clip_rects
                        .iter()
                        .find(|(_, r)| r.contains(p))
                        .map(|(id, _)| *id)
                });
                response.context_menu(|ui| {
                    if let Some((mid, mlabel)) = &ctx_marker {
                        if ui.button(format!("✕ Remove Marker “{mlabel}”")).clicked() {
                            action.remove_marker = Some(*mid);
                            ui.close();
                        }
                        ui.separator();
                    }
                    let target_id = ctx_clip_id.or(selected_clip_id);
                    if let Some(cid) = target_id {
                        if let Some(clip) = project.clip(&cid) {
                            let playhead = project.editor.playhead_ticks;
                            let can_split =
                                playhead > clip.timeline_start && playhead < clip.timeline_end();
                            if ui
                                .add_enabled(can_split, egui::Button::new("✂ Split at Playhead (S)"))
                                .clicked()
                            {
                                action.split_clip = Some((cid, playhead));
                                ui.close();
                            }
                            if ui.button("⧉ Duplicate Clip (Ctrl+D)").clicked() {
                                action.duplicate_clip = Some(cid);
                                ui.close();
                            }
                            if clip.link_group.is_some()
                                && ui.button("⛓ Unlink Audio / Video").clicked()
                            {
                                action.unlink_clip = Some(cid);
                                ui.close();
                            }
                            if ui.button("🗑 Delete Clip (Del)").clicked() {
                                action.remove_clip = Some(cid);
                                ui.close();
                            }
                            if ui.button("⏪ Ripple Delete Clip (Shift+Del)").clicked() {
                                action.ripple_delete_clip = Some(cid);
                                ui.close();
                            }
                            ui.separator();
                            let is_audio_clip = project
                                .track(&clip.track_id)
                                .map(|t| t.kind == TrackKind::Audio)
                                .unwrap_or(false);
                            if is_audio_clip {
                                ui.menu_button("🔊 Clip Volume", |ui| {
                                    for (lbl, vol) in [
                                        ("Mute (0%)", 0.0_f32),
                                        ("Background (−18 dB · 13%)", 0.126),
                                        ("Quiet (−12 dB · 25%)", 0.251),
                                        ("Lower (−6 dB · 50%)", 0.501),
                                        ("Normal (0 dB · 100%)", 1.0),
                                        ("Boost (+3 dB · 141%)", 1.413),
                                        ("Loud (+6 dB · 200%)", 2.0),
                                    ] {
                                        if ui.button(lbl).clicked() {
                                            action.set_clip_volume = Some((cid, vol));
                                            ui.close();
                                        }
                                    }
                                });
                                let fade_dur = (TICKS_PER_SEC / 2).min(clip.timeline_duration());
                                if ui.button("◐ Audio Fade In (0.5s)").clicked() {
                                    action.set_audio_fade_in = Some((cid, fade_dur));
                                    ui.close();
                                }
                                if ui.button("◑ Audio Fade Out (0.5s)").clicked() {
                                    action.set_audio_fade_out = Some((cid, fade_dur));
                                    ui.close();
                                }
                            } else {
                                if ui.button("◐ Add Fade In at Start (0.5s)").clicked() {
                                    action.add_fade_in = Some(clip.timeline_start);
                                    ui.close();
                                }
                                if ui.button("◑ Add Fade Out at End (0.5s)").clicked() {
                                    action.add_fade_out = Some(clip.timeline_end());
                                    ui.close();
                                }
                            }
                            ui.separator();
                            if ui.button("🔇 Cut Dead Space (Silence)").clicked() {
                                action.cut_silence_asset = Some(clip.asset_id);
                                ui.close();
                            }
                            if ui.button("🎙 Transcribe Audio (Whisper)").clicked() {
                                action.transcribe_asset = Some(clip.asset_id);
                                ui.close();
                            }
                        }
                    } else if ctx_marker.is_none()
                        && ui.button("🚩 Add Marker at Playhead (M)").clicked()
                    {
                        action.add_marker_at = Some(project.editor.playhead_ticks);
                        ui.close();
                    }
                });
            });
        });
    });

    action
}

fn parse_hex_color(hex: &str) -> egui::Color32 {
    let s = hex.trim().trim_start_matches('#');
    if s.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&s[0..2], 16),
            u8::from_str_radix(&s[2..4], 16),
            u8::from_str_radix(&s[4..6], 16),
        ) {
            return egui::Color32::from_rgb(r, g, b);
        }
    }
    egui::Color32::from_rgb(245, 158, 11)
}

fn draw_toolbar(
    ui: &mut egui::Ui,
    project: &Project,
    state: &mut TimelineState,
    selected_clip_id: Option<Uuid>,
    action: &mut TimelineAction,
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;

        // Tool mode buttons.
        let sel_btn = egui::Button::new("↖ Select (V)").selected(state.tool == TimelineTool::Select);
        if ui
            .add(sel_btn)
            .on_hover_text("Select, move, or drag clip edges to trim (V)")
            .clicked()
        {
            state.tool = TimelineTool::Select;
        }
        let razor_btn = egui::Button::new("✂ Razor (C)").selected(state.tool == TimelineTool::Razor);
        if ui
            .add(razor_btn)
            .on_hover_text("Click anywhere on a clip to cut/split it (C)")
            .clicked()
        {
            state.tool = TimelineTool::Razor;
        }

        ui.separator();

        // Direct clip actions.
        if ui
            .button("✂ Split (S)")
            .on_hover_text("Split clip under playhead (S)")
            .clicked()
        {
            action.split_at_playhead = true;
        }
        if ui
            .add_enabled(selected_clip_id.is_some(), egui::Button::new("🗑 Delete"))
            .on_hover_text("Remove selected clip (Del)")
            .clicked()
        {
            action.remove_clip = selected_clip_id;
        }
        if ui
            .add_enabled(
                selected_clip_id.is_some(),
                egui::Button::new("⏪ Ripple Del"),
            )
            .on_hover_text("Remove selected clip and close the gap (Shift+Del)")
            .clicked()
        {
            action.ripple_delete_clip = selected_clip_id;
        }

        if ui
            .add_enabled(
                selected_clip_id.is_some(),
                egui::Button::new("⧉ Dup"),
            )
            .on_hover_text("Duplicate selected clip (Ctrl+D)")
            .clicked()
        {
            action.duplicate_clip = selected_clip_id;
        }
        if ui
            .button("🚩 Marker (M)")
            .on_hover_text("Add a timeline marker / chapter at the playhead (M)")
            .clicked()
        {
            action.add_marker_at = Some(project.editor.playhead_ticks);
        }

        ui.separator();

        // In / Out range selection & Ripple Cut.
        if ui
            .button("[ In (I)")
            .on_hover_text("Mark range start at playhead (I)")
            .clicked()
        {
            state.in_point = Some(project.editor.playhead_ticks);
        }
        if ui
            .button("Out (O) ]")
            .on_hover_text("Mark range end at playhead (O)")
            .clicked()
        {
            state.out_point = Some(project.editor.playhead_ticks);
        }
        let range = state.active_range();
        if ui
            .add_enabled(range.is_some(), egui::Button::new("✂ Cut Range (X)"))
            .on_hover_text("Ripple-delete the marked In..Out range across unlocked tracks (X)")
            .clicked()
        {
            if let Some((s, e)) = range {
                action.ripple_cut_range = Some((s, e));
                state.in_point = None;
                state.out_point = None;
            }
        }
        if (state.in_point.is_some() || state.out_point.is_some())
            && ui.small_button("✕").on_hover_text("Clear In/Out marks").clicked()
        {
            state.in_point = None;
            state.out_point = None;
        }

        ui.separator();

        // Snap + Track controls.
        let snap_btn = egui::Button::new("🧲 Snap").selected(state.snap);
        if ui
            .add(snap_btn)
            .on_hover_text("Snap moves, trims, and razor cuts to playhead & clip edges")
            .clicked()
        {
            state.snap = !state.snap;
        }
        if ui.small_button("+ V Track").on_hover_text("Add overlay video track").clicked() {
            action.add_track = Some(TrackKind::Video);
        }
        if ui.small_button("+ A Track").on_hover_text("Add audio track").clicked() {
            action.add_track = Some(TrackKind::Audio);
        }

        // Right-aligned Zoom controls (supports zooming out arbitrarily far).
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Fit (F)").on_hover_text("Fit entire timeline in view (F)").clicked() {
                action.fit_timeline = true;
            }
            if ui.small_button("+").on_hover_text("Zoom in (+)").clicked() {
                action.set_zoom =
                    Some((project.editor.zoom_pixels_per_second * 1.4).clamp(MIN_ZOOM, MAX_ZOOM));
            }
            let mut z = project.editor.zoom_pixels_per_second.clamp(MIN_ZOOM, MAX_ZOOM);
            let slider_min = z.clamp(MIN_ZOOM, 0.01);
            let slider_max = z.clamp(600.0, MAX_ZOOM);
            let visible_hint_secs = 1000.0 / z.max(MIN_ZOOM) as f64;
            if ui
                .add(
                    egui::Slider::new(&mut z, slider_min..=slider_max)
                        .logarithmic(true)
                        .show_value(false),
                )
                .on_hover_text(format!(
                    "Timeline zoom: {:.3} px/s (~{} per 1000px) — Ctrl + Wheel or −/+ to zoom freely",
                    z,
                    format_seconds_short(visible_hint_secs)
                ))
                .changed()
            {
                action.set_zoom = Some(z.clamp(MIN_ZOOM, MAX_ZOOM));
            }
            if ui.small_button("−").on_hover_text("Zoom out (−)").clicked() {
                action.set_zoom =
                    Some((project.editor.zoom_pixels_per_second / 1.4).clamp(MIN_ZOOM, MAX_ZOOM));
            }
        });
    });
}

fn draw_drag_ghost(
    painter: &egui::Painter,
    rect: egui::Rect,
    layout: &TimelineLayout,
    project: &Project,
    state: &TimelineState,
    tick_to_x: &impl Fn(Ticks) -> f32,
) {
    let (Some(mode), Some(cur_t)) = (state.drag, state.drag_current_ticks) else {
        return;
    };
    let frame = project.settings.fps.frame_duration_ticks().max(1);
    let (clip_id, ghost_start, ghost_end, track_id) = match mode {
        DragMode::Scrub | DragMode::AdjustVolume { .. } => return,
        DragMode::MoveClip { clip_id, .. } => {
            let Some(c) = project.clip(&clip_id) else { return };
            let tid = state.drag_target_track.unwrap_or(c.track_id);
            (clip_id, cur_t.max(0), cur_t.max(0) + c.timeline_duration(), tid)
        }
        DragMode::TrimLeft { clip_id } => {
            let Some(c) = project.clip(&clip_id) else { return };
            let min_s = (c.timeline_start - c.source_in).max(0);
            let max_s = c.timeline_end() - frame;
            (clip_id, cur_t.clamp(min_s, max_s), c.timeline_end(), c.track_id)
        }
        DragMode::TrimRight { clip_id } => {
            let Some(c) = project.clip(&clip_id) else { return };
            let asset_max = project
                .asset(&c.asset_id)
                .map(|a| a.editable_duration())
                .unwrap_or(c.source_out);
            let min_e = c.timeline_start + frame;
            let max_e = c.timeline_start + (asset_max - c.source_in);
            (clip_id, c.timeline_start, cur_t.clamp(min_e, max_e), c.track_id)
        }
    };
    let _ = clip_id;
    let Some(&y) = layout.track_y.get(&track_id) else {
        return;
    };
    let gx0 = rect.left() + tick_to_x(ghost_start);
    let gx1 = (rect.left() + tick_to_x(ghost_end)).max(gx0 + 4.0);
    let g_rect = egui::Rect::from_min_max(
        egui::pos2(gx0, rect.top() + layout.tracks_top_offset + y + 2.0),
        egui::pos2(gx1, rect.top() + layout.tracks_top_offset + y + TRACK_HEIGHT - 2.0),
    );
    painter.rect_filled(
        g_rect,
        4.0,
        egui::Color32::from_rgba_unmultiplied(110, 210, 255, 80),
    );
    painter.rect_stroke(
        g_rect,
        4.0,
        egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 230, 255)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        g_rect.left_top() + egui::vec2(4.0, -14.0),
        egui::Align2::LEFT_TOP,
        format!(
            "{}  ({:.2}s)",
            format_timecode(ghost_start, project.settings.fps),
            cutpilot_core::secs_from_ticks(ghost_end - ghost_start)
        ),
        egui::FontId::monospace(10.5),
        egui::Color32::from_rgb(140, 235, 255),
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_clip_filmstrip(
    ui: &egui::Ui,
    clip_rect: egui::Rect,
    clip_vis: egui::Rect,
    clip: &Clip,
    asset: &cutpilot_core::Asset,
    zoom: f32,
    dimmed: bool,
    filmstrips: &FilmstripView<'_>,
    requests: &mut Vec<FilmstripTileRequest>,
) {
    let body_rect = egui::Rect::from_min_max(
        egui::pos2(clip_rect.left() + 1.0, clip_rect.top() + 16.0),
        egui::pos2(clip_rect.right() - 1.0, clip_rect.bottom() - 1.0),
    );
    if body_rect.width() < 8.0 || body_rect.height() < 12.0 {
        return;
    }
    let vis = body_rect.intersect(clip_vis);
    if !vis.is_positive() {
        return;
    }

    let aspect = asset
        .probe
        .as_ref()
        .map(|p| {
            let (w, h) = p.oriented_size();
            if h > 0 {
                w as f32 / h as f32
            } else {
                16.0 / 9.0
            }
        })
        .unwrap_or(16.0 / 9.0)
        .clamp(0.55, 2.2);
    let tile_h_f = body_rect.height();
    let tile_w_f = (tile_h_f * aspect).clamp(28.0, 96.0);
    let tile_w_u = (tile_w_f.round() as u32).clamp(28, 96) & !1;
    let tile_h_u = (tile_h_f.round() as u32).clamp(24, 64) & !1;

    let first_k = (((vis.left() - clip_rect.left()) / tile_w_f).floor().max(0.0)) as usize;
    let last_k = (((vis.right() - clip_rect.left()) / tile_w_f).ceil().max(0.0)) as usize;

    let src_in_secs = cutpilot_core::secs_from_ticks(clip.source_in);
    let src_out_secs = cutpilot_core::secs_from_ticks(clip.source_out);
    let tile_dur_secs = (tile_w_f as f64) / (zoom.max(MIN_ZOOM) as f64);
    let is_image = asset.kind == AssetKind::Image;

    let tint = if dimmed {
        egui::Color32::from_rgba_unmultiplied(150, 150, 150, 180)
    } else {
        egui::Color32::WHITE
    };

    let clip_painter = ui.painter_at(body_rect);
    for k in first_k..=last_k {
        let tx0 = clip_rect.left() + k as f32 * tile_w_f;
        if tx0 >= body_rect.right() {
            break;
        }
        let tx1 = (tx0 + tile_w_f).min(body_rect.right());
        let actual_w = tx1 - tx0;
        if actual_w < 1.0 {
            continue;
        }
        let tile_rect = egui::Rect::from_min_max(
            egui::pos2(tx0, body_rect.top()),
            egui::pos2(tx1, body_rect.bottom()),
        );
        let center_offset_px = (tx0 - clip_rect.left()) + tile_w_f * 0.5;
        let sample_secs = (src_in_secs + center_offset_px as f64 / zoom.max(MIN_ZOOM) as f64)
            .clamp(src_in_secs, (src_out_secs - 0.01).max(src_in_secs));

        if let Some((tex_id, mut uv)) = filmstrips.lookup_or_request(
            clip.asset_id,
            is_image,
            sample_secs,
            tile_dur_secs,
            tile_w_u,
            tile_h_u,
            requests,
        ) {
            // Crop UV horizontally if the final tile is narrower than a full tile.
            if actual_w < tile_w_f - 0.5 {
                let frac = (actual_w / tile_w_f).clamp(0.01, 1.0);
                uv.max.x = uv.min.x + (uv.max.x - uv.min.x) * frac;
            }
            clip_painter.image(tex_id, tile_rect, uv, tint);
            if k > first_k {
                clip_painter.line_segment(
                    [tile_rect.left_top(), tile_rect.left_bottom()],
                    egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 70)),
                );
            }
        }
    }
}

fn draw_clip_waveform(
    painter: &egui::Painter,
    clip_rect: egui::Rect,
    clip_vis: egui::Rect,
    clip: &Clip,
    asset_duration: Ticks,
    peaks: &[Peak],
) {
    if peaks.is_empty() || asset_duration <= 0 || clip_rect.width() < 6.0 {
        return;
    }
    let wf_top = clip_rect.top() + 17.0;
    let wf_bottom = clip_rect.bottom() - 2.0;
    if wf_bottom <= wf_top + 6.0 {
        return;
    }
    let wf_rect = egui::Rect::from_min_max(
        egui::pos2(clip_rect.left() + 1.0, wf_top),
        egui::pos2(clip_rect.right() - 1.0, wf_bottom),
    );
    let vis = wf_rect.intersect(clip_vis);
    if !vis.is_positive() {
        return;
    }

    let mid_y = (wf_top + wf_bottom) * 0.5;
    let half_h = (wf_bottom - wf_top) * 0.48;

    // Subtle dark backing + center zero line + -35 dB silence threshold rails
    // so silent pockets vs speech energy pop out immediately.
    painter.rect_filled(
        vis,
        2.0,
        egui::Color32::from_rgba_unmultiplied(12, 26, 20, 115),
    );
    painter.line_segment(
        [egui::pos2(vis.left(), mid_y), egui::pos2(vis.right(), mid_y)],
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 34)),
    );
    let silence_amp: f32 = 0.018; // ~ -35 dB (matches CutPilot default silence detector)
    let rail_h = perceptual_amp(silence_amp) * half_h;
    for sign in [-1.0_f32, 1.0_f32] {
        let ry = mid_y + sign * rail_h;
        painter.line_segment(
            [egui::pos2(vis.left(), ry), egui::pos2(vis.right(), ry)],
            egui::Stroke::new(0.75, egui::Color32::from_rgba_unmultiplied(130, 215, 175, 28)),
        );
    }

    let start_frac = (clip.source_in as f64 / asset_duration as f64).clamp(0.0, 1.0);
    let end_frac = (clip.source_out as f64 / asset_duration as f64).clamp(start_frac, 1.0);
    let frac_span = end_frac - start_frac;
    let clip_w = clip_rect.width().max(1.0);
    let gain = clip.volume.clamp(0.0, 2.0);
    let peak_len = peaks.len();

    // Render one high-resolution vertical slice every 1.5 px across the visible
    // portion of the clip (no 300-bar cap!).
    let col_step: f32 = 1.5;
    let first_col = ((vis.left() - clip_rect.left()) / col_step).floor().max(0.0) as usize;
    let last_col = ((vis.right() - clip_rect.left()) / col_step).ceil().max(0.0) as usize;

    for col in first_col..=last_col {
        let x0 = clip_rect.left() + col as f32 * col_step;
        let x_mid = x0 + col_step * 0.5;
        if x_mid < vis.left() || x_mid > vis.right() {
            continue;
        }
        let u0 = (((x0 - clip_rect.left()) / clip_w) as f64).clamp(0.0, 1.0);
        let u1 = ((((x0 + col_step) - clip_rect.left()) / clip_w) as f64).clamp(u0, 1.0);
        let f0 = start_frac + u0 * frac_span;
        let f1 = start_frac + u1 * frac_span;

        let i0 = ((f0 * peak_len as f64).floor() as usize).min(peak_len - 1);
        let i1 = ((f1 * peak_len as f64).ceil() as usize).clamp(i0 + 1, peak_len);

        let mut min_v = 0.0_f32;
        let mut max_v = 0.0_f32;
        let mut max_rms = 0.0_f32;
        for p in &peaks[i0..i1] {
            min_v = min_v.min(p.min);
            max_v = max_v.max(p.max);
            let r = if p.rms > 0.0 {
                p.rms
            } else {
                p.max.abs().max(p.min.abs()) * 0.55
            };
            max_rms = max_rms.max(r);
        }

        let pos_amp = (max_v.abs() * gain).min(1.0);
        let neg_amp = (min_v.abs() * gain).min(1.0);
        let peak_amp = pos_amp.max(neg_amp);
        let rms_amp = (max_rms * gain).min(peak_amp);

        if peak_amp < silence_amp {
            // Silent pocket: recessed dark tint + subtle flat baseline so dead
            // space is unmistakably clear.
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, wf_top),
                    egui::pos2((x0 + col_step).min(vis.right()), wf_bottom),
                ),
                0.0,
                egui::Color32::from_rgba_unmultiplied(6, 10, 12, 95),
            );
            painter.line_segment(
                [egui::pos2(x_mid, mid_y - 0.7), egui::pos2(x_mid, mid_y + 0.7)],
                egui::Stroke::new(col_step, egui::Color32::from_rgba_unmultiplied(85, 130, 110, 110)),
            );
            continue;
        }

        // Loudness-coded outer peak envelope.
        let peak_color = if peak_amp >= 0.85 {
            // Very loud / near clipping (> -1.4 dB)
            egui::Color32::from_rgba_unmultiplied(255, 92, 82, 235)
        } else if peak_amp >= 0.50 {
            // Loud emphasis (-6 dB .. -1.4 dB)
            egui::Color32::from_rgba_unmultiplied(250, 202, 72, 225)
        } else {
            // Normal speech body
            egui::Color32::from_rgba_unmultiplied(98, 224, 162, 205)
        };

        let top_h = (perceptual_amp(pos_amp) * half_h).max(1.2);
        let bot_h = (perceptual_amp(neg_amp) * half_h).max(1.2);
        painter.line_segment(
            [egui::pos2(x_mid, mid_y - top_h), egui::pos2(x_mid, mid_y + bot_h)],
            egui::Stroke::new(col_step, peak_color),
        );

        // Brighter inner RMS energy core showing sustained speech loudness.
        if rms_amp >= silence_amp * 0.6 {
            let rms_h = (perceptual_amp(rms_amp) * half_h).min(top_h.min(bot_h));
            let rms_color = if peak_amp >= 0.85 {
                egui::Color32::from_rgba_unmultiplied(255, 190, 150, 235)
            } else if peak_amp >= 0.50 {
                egui::Color32::from_rgba_unmultiplied(255, 235, 145, 225)
            } else {
                egui::Color32::from_rgba_unmultiplied(180, 255, 218, 225)
            };
            painter.line_segment(
                [egui::pos2(x_mid, mid_y - rms_h), egui::pos2(x_mid, mid_y + rms_h)],
                egui::Stroke::new(col_step, rms_color),
            );
        }
    }
}

/// Perceptual compression curve: boosts subtle speech dynamics while keeping
/// near-zero silence visually flat so quiet vs loud spots read effortlessly.
fn perceptual_amp(linear: f32) -> f32 {
    linear.clamp(0.0, 1.0).powf(0.68)
}

/// Snap `raw` to playhead or nearby clip boundaries within `SNAP_PX` screen pixels.
fn snap_ticks(
    raw: Ticks,
    project: &Project,
    zoom: f32,
    snap_enabled: bool,
    ignore_clip: Option<Uuid>,
) -> (Ticks, Option<Ticks>) {
    if !snap_enabled {
        return (raw.max(0), None);
    }
    let tol_ticks = cutpilot_core::ticks_from_secs((SNAP_PX / zoom.max(MIN_ZOOM)) as f64);
    let mut best: Option<(Ticks, Ticks)> = None; // (dist, target)

    let mut consider = |candidate: Ticks| {
        let dist = (raw - candidate).abs();
        if dist <= tol_ticks {
            match best {
                None => best = Some((dist, candidate)),
                Some((bd, _)) if dist < bd => best = Some((dist, candidate)),
                _ => {}
            }
        }
    };

    consider(0);
    consider(project.editor.playhead_ticks);
    for c in &project.clips {
        if Some(c.id) == ignore_clip {
            continue;
        }
        consider(c.timeline_start);
        consider(c.timeline_end());
    }

    if let Some((_, target)) = best {
        (target.max(0), Some(target.max(0)))
    } else {
        (raw.max(0), None)
    }
}

fn ruler_step(zoom: f32) -> f64 {
    let target = 90.0 / zoom.max(MIN_ZOOM) as f64;
    for step in [
        0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0,
        1800.0, 3600.0, 7200.0, 14400.0, 28800.0, 43200.0, 86400.0,
    ] {
        if step >= target {
            return step;
        }
    }
    let mut step = 86_400.0;
    while step < target {
        step *= 2.0;
    }
    step
}

fn format_seconds_short(secs: f64) -> String {
    let total = secs.round() as i64;
    let s = total % 60;
    let m = (total / 60) % 60;
    let h = (total / 3600) % 24;
    let d = total / 86_400;
    if d > 0 {
        format!("{d}d {h:02}:{m:02}")
    } else if total >= 3600 {
        format!("{}:{m:02}:{s:02}", total / 3600)
    } else {
        format!("{m:02}:{s:02}")
    }
}

pub fn format_timecode(ticks: Ticks, fps: cutpilot_core::Fps) -> String {
    let total_secs = cutpilot_core::secs_from_ticks(ticks.max(0));
    let whole = total_secs.floor() as i64;
    let frac = total_secs - whole as f64;
    let frames = (frac * fps.as_f64()).floor() as i64;
    let s = whole % 60;
    let m = (whole / 60) % 60;
    let h = whole / 3600;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}:{frames:02}")
    } else {
        format!("{m:02}:{s:02}:{frames:02}")
    }
}

fn asset_compatible_with_track(asset: &cutpilot_core::Asset, track_kind: TrackKind) -> bool {
    match track_kind {
        TrackKind::Video => matches!(asset.kind, AssetKind::Video | AssetKind::Image),
        TrackKind::Audio => match asset.kind {
            AssetKind::Audio => true,
            AssetKind::Video => asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false),
            AssetKind::Image => false,
        },
    }
}

fn row_color(kind: TrackKind) -> egui::Color32 {
    match kind {
        TrackKind::Video => egui::Color32::from_rgb(30, 35, 46),
        TrackKind::Audio => egui::Color32::from_rgb(28, 39, 34),
    }
}

fn clip_fill(kind: TrackKind, generated: bool) -> egui::Color32 {
    if generated {
        egui::Color32::from_rgb(122, 68, 165)
    } else {
        match kind {
            TrackKind::Video => egui::Color32::from_rgb(48, 94, 168),
            TrackKind::Audio => egui::Color32::from_rgb(42, 124, 86),
        }
    }
}

fn brighten(c: egui::Color32, add: u8) -> egui::Color32 {
    egui::Color32::from_rgb(
        c.r().saturating_add(add),
        c.g().saturating_add(add),
        c.b().saturating_add(add),
    )
}

/// Map linear clip volume (`0.0..=2.0`) to a Y position inside an audio clip's waveform band.
fn volume_to_y(volume: f32, top_y: f32, bottom_y: f32) -> f32 {
    let frac = (volume.clamp(0.0, 2.0) / 2.0).clamp(0.0, 1.0);
    bottom_y - frac * (bottom_y - top_y)
}

/// Map a dragged pointer Y position back to linear clip volume (`0.0..=2.0`),
/// with a gentle magnetic snap to `1.0` (0 dB / 100%).
fn y_to_volume(y: f32, top_y: f32, bottom_y: f32) -> f32 {
    let span = (bottom_y - top_y).max(1.0);
    let frac = ((bottom_y - y) / span).clamp(0.0, 1.0);
    let raw = (frac * 2.0).clamp(0.0, 2.0);
    if (raw - 1.0).abs() < 0.045 {
        1.0
    } else if raw < 0.025 {
        0.0
    } else {
        (raw * 100.0).round() / 100.0
    }
}

pub fn format_volume_pct(vol: f32) -> String {
    format!("{:.0}%", (vol.max(0.0) * 100.0).round())
}

pub fn format_volume_db(vol: f32) -> String {
    if vol <= 0.0005 {
        "−∞ dB".into()
    } else {
        let db = 20.0 * vol.log10();
        if db.abs() < 0.05 {
            "0.0 dB".into()
        } else if db > 0.0 {
            format!("+{db:.1} dB")
        } else {
            format!("{db:.1} dB")
        }
    }
}

fn format_volume_short(vol: f32) -> String {
    if vol <= 0.001 {
        "0%".into()
    } else {
        format!("{:.0}%", (vol * 100.0).round())
    }
}

/// Small helper used by the inspector: seconds <-> ticks DragValue.
pub fn seconds_field(ui: &mut egui::Ui, label: &str, ticks: Ticks) -> Option<Ticks> {
    let mut secs = cutpilot_core::secs_from_ticks(ticks) as f32;
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        changed |= ui
            .add(
                egui::DragValue::new(&mut secs)
                    .speed(0.05)
                    .suffix(" s")
                    .range(0.0..=86_400.0),
            )
            .changed();
    });
    if changed {
        Some(cutpilot_core::ticks_from_secs(secs as f64))
    } else {
        None
    }
}

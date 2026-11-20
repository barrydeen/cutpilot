//! Timeline widget: ruler, tracks, clips, playhead, drag-to-move, scrubbing,
//! and zoom. Pure painting plus hit-testing; all mutations go through the
//! engine command surface.

use eframe::egui;
use openprem_core::{Clip, Project, Ticks, TrackKind, TICKS_PER_SEC};

pub const RULER_HEIGHT: f32 = 22.0;
pub const TRACK_HEIGHT: f32 = 46.0;
pub const HEADER_WIDTH: f32 = 92.0;

/// Result of user interaction with the timeline this frame.
#[derive(Default)]
pub struct TimelineAction {
    pub seek: Option<Ticks>,
    pub select_clip: Option<uuid::Uuid>,
    /// (clip_id, new timeline start)
    pub move_clip: Option<(uuid::Uuid, Ticks)>,
}

pub struct TimelineLayout {
    pub content_width: f32,
    pub zoom: f32,
    /// y offset of each track row, keyed by track id.
    pub track_y: std::collections::HashMap<uuid::Uuid, f32>,
    pub tracks_height: f32,
}

pub fn compute_layout(project: &Project, available_width: f32) -> TimelineLayout {
    let zoom = project.editor.zoom_pixels_per_second.max(1.0);
    let duration_secs = openprem_core::secs_from_ticks(project.timeline_end().max(TICKS_PER_SEC));
    let content_width = ((duration_secs + 4.0) * zoom as f64) as f32;
    let content_width = content_width.max(available_width - HEADER_WIDTH);
    let mut track_y = std::collections::HashMap::new();
    let mut y = 0.0;
    // Video tracks draw top row = topmost (last in Vec).
    for track in project.tracks.iter().rev() {
        track_y.insert(track.id, y);
        y += TRACK_HEIGHT + 4.0;
    }
    TimelineLayout {
        content_width,
        zoom,
        track_y,
        tracks_height: y,
    }
}

pub fn draw_timeline(
    ui: &mut egui::Ui,
    project: &Project,
    drag_state: &mut DragState,
) -> TimelineAction {
    let mut action = TimelineAction::default();
    let available_width = ui.available_width();
    let layout = compute_layout(project, available_width);

    ui.horizontal(|ui| {
        // Track headers (fixed column).
        egui::Frame::new()
            .fill(ui.visuals().panel_fill)
            .show(ui, |ui| {
                ui.set_width(HEADER_WIDTH);
                ui.allocate_space((HEADER_WIDTH, RULER_HEIGHT).into());
                for track in project.tracks.iter().rev() {
                    let row = egui::Frame::new()
                        .fill(row_color(ui, track.kind))
                        .inner_margin(egui::Margin::symmetric(4, 4))
                        .show(ui, |ui| {
                            ui.set_width(HEADER_WIDTH - 8.0);
                            ui.set_height(TRACK_HEIGHT);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    egui::RichText::new(&track.name)
                                        .size(12.0)
                                        .strong()
                                        .color(ui.visuals().text_color()),
                                );
                            });
                        });
                    let _ = row;
                }
            });

        // Scrollable track area with ruler.
        egui::ScrollArea::horizontal()
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                let total = egui::vec2(layout.content_width, RULER_HEIGHT + layout.tracks_height);
                let (rect, response) = ui.allocate_exact_size(total, egui::Sense::click_and_drag());
                let painter = ui.painter_at(rect);

                let tick_to_x = |t: Ticks| -> f32 {
                    openprem_core::secs_from_ticks(t) as f32 * layout.zoom
                };
                let x_to_ticks = |x: f32| -> Ticks {
                    let secs = (x / layout.zoom) as f64;
                    openprem_core::ticks_from_secs(secs.max(0.0))
                };

                // Ruler background + second ticks.
                painter.rect_filled(
                    rect.shrink2(egui::vec2(0.0, 0.0)),
                    2.0,
                    ui.visuals().extreme_bg_color,
                );
                let step_secs = ruler_step(layout.zoom);
                let mut sec = 0.0;
                while tick_to_x(openprem_core::ticks_from_secs(sec)) < layout.content_width {
                    let x = tick_to_x(openprem_core::ticks_from_secs(sec));
                    painter.line_segment(
                        [egui::pos2(rect.left() + x, rect.top() + RULER_HEIGHT - 6.0),
                         egui::pos2(rect.left() + x, rect.top() + RULER_HEIGHT)],
                        egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
                    );
                    if sec % (step_secs * 5.0).max(step_secs) < 0.5 {
                        let small = egui::TextStyle::Small.resolve(ui.style());
                        painter.text(
                            egui::pos2(rect.left() + x + 3.0, rect.top() + 4.0),
                            egui::Align2::LEFT_TOP,
                            format_seconds_short(sec),
                            small,
                            ui.visuals().text_color(),
                        );
                    }
                    sec += step_secs;
                }

                // Track backgrounds.
                for track in project.tracks.iter().rev() {
                    let y = layout.track_y[&track.id];
                    let bg = rect.translate(egui::vec2(0.0, RULER_HEIGHT + y));
                    painter.rect_filled(
                        egui::Rect::from_min_size(
                            bg.min,
                            egui::vec2(layout.content_width, TRACK_HEIGHT),
                        ),
                        3.0,
                        row_color(ui, track.kind),
                    );
                }

                // Clips.
                let selected: Vec<uuid::Uuid> = project
                    .editor
                    .selection
                    .iter()
                    .filter_map(|s| match s {
                        openprem_core::Selection::Clip(id) => Some(*id),
                        _ => None,
                    })
                    .collect();
                let mut clip_rects: Vec<(uuid::Uuid, egui::Rect)> = Vec::new();
                for clip in &project.clips {
                    let Some(track) = project.track(&clip.track_id) else { continue };
                    let y = layout.track_y[&track.id];
                    let x0 = rect.left() + tick_to_x(clip.timeline_start);
                    let x1 = rect.left() + tick_to_x(clip.timeline_end());
                    let clip_rect = egui::Rect::from_min_max(
                        egui::pos2(x0, rect.top() + RULER_HEIGHT + y + 2.0),
                        egui::pos2(x1, rect.top() + RULER_HEIGHT + y + TRACK_HEIGHT - 2.0),
                    );
                    let is_sel = selected.contains(&clip.id);
                    let is_gen = project
                        .asset(&clip.asset_id)
                        .map(|a| a.generated.is_some())
                        .unwrap_or(false);
                    let fill = if is_sel {
                        clip_fill(ui, track.kind, is_gen).gamma_multiply(1.6)
                    } else {
                        clip_fill(ui, track.kind, is_gen)
                    };
                    painter.rect_filled(clip_rect, 4.0, fill);
                    if is_sel {
                        painter.rect_stroke(clip_rect, 4.0, egui::Stroke::new(2.0, egui::Color32::WHITE), egui::StrokeKind::Inside);
                    } else {
                        painter.rect_stroke(clip_rect, 4.0, egui::Stroke::new(1.0, egui::Color32::from_gray(110)), egui::StrokeKind::Inside);
                    }
                    // Label: asset name + duration.
                    if clip_rect.width() > 34.0 {
                        let small = egui::TextStyle::Small.resolve(ui.style());
                        let name = project
                            .asset(&clip.asset_id)
                            .map(|a| a.name.clone())
                            .unwrap_or_default();
                        painter.text(
                            clip_rect.left_top() + egui::vec2(5.0, 3.0),
                            egui::Align2::LEFT_TOP,
                            name,
                            small.clone(),
                            egui::Color32::WHITE,
                        );
                        painter.text(
                            clip_rect.left_bottom() + egui::vec2(5.0, -12.0),
                            egui::Align2::LEFT_TOP,
                            format!(
                                "{:.1}s",
                                openprem_core::secs_from_ticks(clip.source_duration())
                            ),
                            small,
                            egui::Color32::LIGHT_GRAY,
                        );
                    }
                    // Fades.
                    if clip.fade_in_ticks > 0 {
                        let w = tick_to_x(clip.fade_in_ticks);
                        painter.line_segment(
                            [clip_rect.left_top() + egui::vec2(0.0, 3.0),
                             egui::pos2(clip_rect.left() + w, clip_rect.bottom() - 3.0)],
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(220, 220, 120)),
                        );
                    }
                    clip_rects.push((clip.id, clip_rect));
                }

                // Transitions markers.
                for tr in &project.transitions {
                    let x = rect.left() + tick_to_x(tr.at);
                    let half = (tick_to_x(tr.duration) * 0.5).clamp(6.0, 24.0);
                    let cy = rect.top() + RULER_HEIGHT + 8.0;
                    let pts = [
                        egui::pos2(x - half, cy - half),
                        egui::pos2(x + half, cy - half),
                        egui::pos2(x, cy + half * 0.8),
                    ];
                    painter.add(egui::Shape::convex_polygon(
                        pts.to_vec(),
                        egui::Color32::from_rgb(255, 170, 60),
                        egui::Stroke::new(1.0, egui::Color32::BLACK),
                    ));
                }

                // Playhead.
                let playhead_x = rect.left() + tick_to_x(project.editor.playhead_ticks);
                painter.line_segment(
                    [egui::pos2(playhead_x, rect.top()),
                     egui::pos2(playhead_x, rect.bottom())],
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 90, 90)),
                );
                painter.circle_filled(
                    egui::pos2(playhead_x, rect.top() + RULER_HEIGHT / 2.0),
                    5.0,
                    egui::Color32::from_rgb(255, 90, 90),
                );

                // ---- interaction ----
                let pos = response.hover_pos();
                let x_of = |p: egui::Pos2| p.x - rect.left();
                if response.drag_started() {
                    if let Some(p) = pos {
                        let in_ruler = p.y - rect.top() < RULER_HEIGHT;
                        if in_ruler {
                            drag_state.scrubbing = true;
                            action.seek = Some(x_to_ticks(x_of(p)));
                        } else {
                            for (id, r) in &clip_rects {
                                if r.contains(p) {
                                    drag_state.clip_id = Some(*id);
                                    drag_state.grab_offset_ticks = x_to_ticks(x_of(p))
                                        - project
                                            .clip(id)
                                            .map(|c: &Clip| c.timeline_start)
                                            .unwrap_or(0);
                                    break;
                                }
                            }
                            if drag_state.clip_id.is_none() {
                                drag_state.scrubbing = true;
                                action.seek = Some(x_to_ticks(x_of(p)));
                            }
                        }
                    }
                }
                if response.dragged() {
                    if let Some(p) = pos {
                        if drag_state.scrubbing {
                            action.seek = Some(x_to_ticks(x_of(p)));
                        } else if drag_state.clip_id.is_some() {
                            drag_state.pending_x = Some(x_of(p));
                        }
                    }
                }
                if response.drag_stopped() {
                    if let (Some(clip_id), Some(pending_x)) =
                        (drag_state.clip_id.take(), drag_state.pending_x.take())
                    {
                        if let Some(clip) = project.clip(&clip_id) {
                            let raw = x_to_ticks(pending_x) - drag_state.grab_offset_ticks;
                            let snap = TICKS_PER_SEC / 100; // 10 ms
                            let new_start = ((raw + snap / 2) / snap * snap).max(0);
                            if new_start != clip.timeline_start {
                                action.move_clip = Some((clip_id, new_start));
                            }
                        }
                    }
                    drag_state.scrubbing = false;
                }
                if response.clicked() {
                    if let Some(p) = pos {
                        if p.y - rect.top() < RULER_HEIGHT {
                            action.seek = Some(x_to_ticks(x_of(p)));
                        } else {
                            for (id, r) in &clip_rects {
                                if r.contains(p) {
                                    action.select_clip = Some(*id);
                                    break;
                                }
                            }
                            if action.select_clip.is_none() {
                                action.seek = Some(x_to_ticks(x_of(p)));
                            }
                        }
                    }
                    drag_state.scrubbing = false;
                }
            });
    });

    action
}

fn ruler_step(zoom: f32) -> f64 {
    // Aim for ~80 px between ticks.
    let target = 80.0 / zoom.max(1.0) as f64;
    for step in [0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0] {
        if step >= target {
            return step;
        }
    }
    600.0
}

fn format_seconds_short(secs: f64) -> String {
    let total = secs as i64;
    let ms = ((secs - total as f64) * 10.0) as i64;
    let s = total % 60;
    let m = (total / 60) % 60;
    let h = total / 3600;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms}")
    } else {
        format!("{m:02}:{s:02}.{ms}")
    }
}

fn row_color(ui: &egui::Ui, kind: TrackKind) -> egui::Color32 {
    let _ = ui;
    match kind {
        TrackKind::Video => egui::Color32::from_rgb(38, 44, 58),
        TrackKind::Audio => egui::Color32::from_rgb(34, 48, 40),
    }
}

fn clip_fill(ui: &egui::Ui, kind: TrackKind, generated: bool) -> egui::Color32 {
    let _ = ui;
    if generated {
        egui::Color32::from_rgb(122, 74, 160)
    } else {
        match kind {
            TrackKind::Video => egui::Color32::from_rgb(52, 92, 158),
            TrackKind::Audio => egui::Color32::from_rgb(46, 122, 84),
        }
    }
}

/// Mutable interaction state owned by the app.
#[derive(Default)]
pub struct DragState {
    pub scrubbing: bool,
    pub clip_id: Option<uuid::Uuid>,
    pub grab_offset_ticks: Ticks,
    pub pending_x: Option<f32>,
}

/// Small helper used by the inspector: seconds <-> ticks DragValue.
pub fn seconds_field(
    ui: &mut egui::Ui,
    label: &str,
    ticks: Ticks,
) -> Option<Ticks> {
    let mut secs = openprem_core::secs_from_ticks(ticks) as f32;
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        changed |= ui.add(
            egui::DragValue::new(&mut secs)
                .speed(0.05)
                .suffix(" s")
                .range(0.0..=86_400.0),
        )
        .changed();
    });
    if changed {
        Some(openprem_core::ticks_from_secs(secs as f64))
    } else {
        None
    }
}

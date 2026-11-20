//! Pure application of edit commands against a [`Project`].
//!
//! Everything here is deterministic and side-effect free: the same command
//! applied to the same document always yields the same document. The engine
//! wraps these functions with snapshots (undo), revision bumping, and
//! persistence; `dry_run` clones the project so agents can validate a batch
//! without landing it.

use crate::commands::{validate_hex_color, Command, TimeRange};
use crate::error::CoreError;
use crate::model::{
    AssetKind, Caption, CaptionStyle, Clip, Project, Selection, Track, TrackKind, Transition,
    TransitionKind,
};
use crate::ticks::Ticks;
use serde_json::{json, Value};
use uuid::Uuid;


fn invalid(msg: impl Into<String>) -> CoreError {
    CoreError::Validation(msg.into())
}

fn require_track<'a>(project: &'a Project, id: &Uuid) -> Result<&'a Track, CoreError> {
    project.track(id).ok_or_else(|| {
        CoreError::InvalidId(format!("unknown track {id}"))
    })
}

fn require_clip<'a>(project: &'a Project, id: &Uuid) -> Result<&'a Clip, CoreError> {
    project.clip(id).ok_or_else(|| {
        CoreError::InvalidId(format!("unknown clip {id}"))
    })
}

fn require_clip_mut<'a>(
    project: &'a mut Project,
    id: &Uuid,
) -> Result<&'a mut Clip, CoreError> {
    project
        .clips
        .iter_mut()
        .find(|c| &c.id == id)
        .ok_or_else(|| CoreError::InvalidId(format!("unknown clip {id}")))
}

fn ensure_unlocked(project: &Project, track_id: &Uuid) -> Result<(), CoreError> {
    let track = require_track(project, track_id)?;
    if track.locked {
        return Err(invalid(format!(
            "track `{}` is locked; unlock it first",
            track.name
        )));
    }
    Ok(())
}

fn check_overlap(
    project: &Project,
    track_id: &Uuid,
    start: Ticks,
    end: Ticks,
    except: Option<&Uuid>,
) -> Result<(), CoreError> {
    if end <= start {
        return Err(CoreError::InvalidTime(format!(
            "clip range {start}..{end} is empty"
        )));
    }
    for other in &project.clips {
        if other.track_id != *track_id {
            continue;
        }
        if Some(&other.id) == except {
            continue;
        }
        let os = other.timeline_start;
        let oe = other.timeline_end();
        if start < oe && os < end {
            return Err(invalid(format!(
                "clip overlaps `{}` ({}..{})",
                other.id,
                crate::ticks::format_seconds(os),
                crate::ticks::format_seconds(oe)
            )));
        }
    }
    Ok(())
}

fn validate_clip_times(
    project: &Project,
    asset_id: &Uuid,
    source_in: Ticks,
    source_out: Ticks,
) -> Result<(), CoreError> {
    if source_in < 0 {
        return Err(CoreError::InvalidTime("source_in must be >= 0".into()));
    }
    if source_out <= source_in {
        return Err(CoreError::InvalidTime(format!(
            "source_out {source_out} must be greater than source_in {source_in}"
        )));
    }
    let asset = project
        .asset(asset_id)
        .ok_or_else(|| CoreError::InvalidId(format!("unknown asset {asset_id}")))?;
    let limit = asset.editable_duration();
    if source_out > limit {
        return Err(CoreError::InvalidTime(format!(
            "source_out {} exceeds asset duration {}",
            crate::ticks::format_seconds(source_out),
            crate::ticks::format_seconds(limit)
        )));
    }
    Ok(())
}

fn asset_fits_track(project: &Project, asset_id: &Uuid, track: &Track) -> Result<(), CoreError> {
    let asset = project
        .asset(asset_id)
        .ok_or_else(|| CoreError::InvalidId(format!("unknown asset {asset_id}")))?;
    match track.kind {
        TrackKind::Video => match asset.kind {
            AssetKind::Video | AssetKind::Image => Ok(()),
            AssetKind::Audio => Err(invalid(
                "audio-only assets belong on an audio track",
            )),
        },
        TrackKind::Audio => match asset.kind {
            AssetKind::Audio => Ok(()),
            AssetKind::Video => {
                let has_audio = asset.probe.as_ref().map(|p| p.has_audio).unwrap_or(false);
                if has_audio {
                    Ok(())
                } else {
                    Err(invalid(
                        "video asset has no audio to place on an audio track",
                    ))
                }
            }
            AssetKind::Image => Err(invalid("images cannot go on audio tracks")),
        },
    }
}

fn new_clip(
    asset_id: Uuid,
    track_id: Uuid,
    timeline_start: Ticks,
    source_in: Ticks,
    source_out: Ticks,
    link_group: Option<Uuid>,
) -> Clip {
    Clip {
        id: Uuid::new_v4(),
        asset_id,
        track_id,
        timeline_start,
        source_in,
        source_out,
        link_group,
        opacity: 1.0,
        scale: 1.0,
        x_offset: 0.0,
        y_offset: 0.0,
        volume: 1.0,
        fade_in_ticks: 0,
        fade_out_ticks: 0,
    }
}

fn caption_json(c: &Caption) -> Value {
    json!({
        "caption_id": c.id,
        "start": c.start,
        "end": c.end,
        "text": c.text,
    })
}

fn clip_json(c: &Clip) -> Value {
    json!({
        "clip_id": c.id,
        "asset_id": c.asset_id,
        "track_id": c.track_id,
        "timeline_start": c.timeline_start,
        "source_in": c.source_in,
        "source_out": c.source_out,
        "link_group": c.link_group,
    })
}

/// Apply a pure project edit in place. Returns a JSON summary of what changed.
pub fn apply_edit(project: &mut Project, cmd: &Command) -> Result<Value, CoreError> {
    if !cmd.is_pure_edit() {
        return Err(CoreError::NotPure { kind: cmd.kind() });
    }
    match cmd {
        // ----- settings -----
        Command::UpdateProjectSettings {
            name,
            width,
            height,
            fps_num,
            fps_den,
        } => {
            if let Some(n) = name {
                if n.trim().is_empty() {
                    return Err(invalid("project name cannot be empty"));
                }
                project.name = n.trim().to_string();
            }
            if let Some(w) = width {
                let w = *w;
                validate_dimension(w, "width")?;
                project.settings.width = w;
            }
            if let Some(h) = height {
                let h = *h;
                validate_dimension(h, "height")?;
                project.settings.height = h;
            }
            if let (Some(n), Some(d)) = (fps_num, fps_den) {
                let fps = crate::commands::fps_from_parts(*n, *d)
                    .map_err(CoreError::Validation)?;
                project.settings.fps = fps;
            } else if fps_num.is_some() || fps_den.is_some() {
                return Err(invalid(
                    "updating fps requires both fps_num and fps_den",
                ));
            }
            Ok(json!({"name": project.name, "width": project.settings.width,
                      "height": project.settings.height,
                      "fps": project.settings.fps.display()}))
        }

        // ----- tracks -----
        Command::AddTrack { track_kind, name } => {
            let track_kind = parse_track_kind(track_kind)?;
            let count = project
                .tracks
                .iter()
                .filter(|t| t.kind == track_kind)
                .count();
            let name = name.clone().unwrap_or_else(|| {
                let prefix = if track_kind == TrackKind::Video { "V" } else { "A" };
                format!("{prefix}{}", count + 1)
            });
            let track = Track {
                id: Uuid::new_v4(),
                kind: track_kind,
                name,
                muted: false,
                hidden: false,
                locked: false,
                volume: 1.0,
            };
            let id = track.id;
            project.tracks.push(track);
            Ok(json!({"track_id": id}))
        }
        Command::UpdateTrack {
            track_id,
            name,
            muted,
            hidden,
            locked,
            volume,
        } => {
            ensure_unlocked(project, track_id)?;
            let track = project.track_mut(track_id).ok_or_else(|| {
                CoreError::InvalidId(format!("unknown track {track_id}"))
            })?;
            if let Some(n) = name {
                if n.trim().is_empty() {
                    return Err(invalid("track name cannot be empty"));
                }
                track.name = n.trim().to_string();
            }
            if let Some(v) = muted {
                track.muted = *v;
            }
            if let Some(v) = hidden {
                track.hidden = *v;
            }
            if let Some(v) = locked {
                track.locked = *v;
            }
            if let Some(v) = volume {
                validate_gain(*v)?;
                track.volume = *v;
            }
            Ok(json!({"track_id": track_id}))
        }
        Command::MoveTrack { track_id, to_index } => {
            let current = project
                .tracks
                .iter()
                .position(|t| &t.id == track_id)
                .ok_or_else(|| CoreError::InvalidId(format!("unknown track {track_id}")))?;
            let target = *to_index as usize;
            if target >= project.tracks.len() {
                return Err(invalid(format!(
                    "track index {target} out of range (0..{})",
                    project.tracks.len()
                )));
            }
            let track = project.tracks.remove(current);
            project.tracks.insert(target, track);
            Ok(json!({"track_id": track_id, "index": target}))
        }
        Command::RemoveTrack { track_id } => {
            ensure_unlocked(project, track_id)?;
            let has_clips = project.clips.iter().any(|c| &c.track_id == track_id);
            if has_clips {
                return Err(invalid(
                    "track still has clips; remove them first",
                ));
            }
            let before = project.tracks.len();
            project.tracks.retain(|t| &t.id != track_id);
            if project.tracks.len() == before {
                return Err(CoreError::InvalidId(format!("unknown track {track_id}")));
            }
            Ok(json!({"removed": true}))
        }

        // ----- clips -----
        Command::InsertClip {
            asset_id,
            track_id,
            timeline_start,
            source_in,
            source_out,
            link_group,
        } => {
            ensure_unlocked(project, track_id)?;
            let track = require_track(project, track_id)?;
            asset_fits_track(project, asset_id, track)?;
            validate_clip_times(project, asset_id, *source_in, *source_out)?;
            if *timeline_start < 0 {
                return Err(CoreError::InvalidTime("timeline_start must be >= 0".into()));
            }
            check_overlap(
                project,
                track_id,
                *timeline_start,
                *timeline_start + (source_out - source_in),
                None,
            )?;
            let clip = new_clip(
                *asset_id,
                *track_id,
                *timeline_start,
                *source_in,
                *source_out,
                *link_group,
            );
            let data = clip_json(&clip);
            let id = clip.id;
            project.clips.push(clip);
            Ok(json!({"clip_id": id, "inserted": data}))
        }
        Command::MoveClip {
            clip_id,
            timeline_start,
            track_id,
        } => {
            let clip = require_clip(project, clip_id)?;
            ensure_unlocked(project, &clip.track_id)?;
            let old_start = clip.timeline_start;
            let old_track = clip.track_id;
            let group = clip.link_group;
            let delta = *timeline_start - old_start;
            if *timeline_start < 0 {
                return Err(CoreError::InvalidTime("timeline_start must be >= 0".into()));
            }
            let target_track = track_id.unwrap_or(old_track);
            ensure_unlocked(project, &target_track)?;
            if target_track != old_track {
                let track = require_track(project, &target_track)?;
                asset_fits_track(project, &clip.asset_id, track)?;
            }
            let duration = clip.source_duration();
            check_overlap(
                project,
                &target_track,
                *timeline_start,
                *timeline_start + duration,
                Some(clip_id),
            )?;
            if let Some(group) = group {
                // Validate every partner's new position before moving any.
                let partners: Vec<(Uuid, Uuid, Ticks, Ticks)> = project
                    .clips
                    .iter()
                    .filter(|other| {
                        other.link_group == Some(group) && &other.id != clip_id
                    })
                    .map(|other| {
                        (
                            other.id,
                            other.track_id,
                            other.timeline_start,
                            other.source_duration(),
                        )
                    })
                    .collect();
                for (id, track, start, duration) in partners {
                    let new_start = (start + delta).max(0);
                    check_overlap(
                        project,
                        &track,
                        new_start,
                        new_start + duration,
                        Some(&id),
                    )?;
                }
                for other in project.clips.iter_mut() {
                    if other.link_group == Some(group) && &other.id != clip_id {
                        other.timeline_start = (other.timeline_start + delta).max(0);
                    }
                }
            }
            let clip = require_clip_mut(project, clip_id)?;
            clip.timeline_start = *timeline_start;
            if let Some(t) = track_id {
                clip.track_id = *t;
            }
            Ok(json!({"clip_id": clip_id, "timeline_start": *timeline_start,
                      "moved_group": group.is_some()}))
        }
        Command::TrimClip {
            clip_id,
            timeline_start,
            source_in,
            source_out,
        } => {
            let existing = require_clip(project, clip_id)?;
            ensure_unlocked(project, &existing.track_id)?;
            let new_start = timeline_start.unwrap_or(existing.timeline_start);
            let new_in = source_in.unwrap_or(existing.source_in);
            let new_out = source_out.unwrap_or(existing.source_out);
            validate_clip_times(project, &existing.asset_id, new_in, new_out)?;
            if new_start < 0 {
                return Err(CoreError::InvalidTime("timeline_start must be >= 0".into()));
            }
            check_overlap(
                project,
                &existing.track_id,
                new_start,
                new_start + (new_out - new_in),
                Some(clip_id),
            )?;
            let clip = require_clip_mut(project, clip_id)?;
            clip.timeline_start = new_start;
            clip.source_in = new_in;
            clip.source_out = new_out;
            let fade_limit = clip.source_duration();
            clip.fade_in_ticks = clip.fade_in_ticks.min(fade_limit);
            clip.fade_out_ticks = clip.fade_out_ticks.min(fade_limit);
            Ok(json!({"clip_id": clip_id}))
        }
        Command::SplitClip { clip_id, at } => {
            let existing = require_clip(project, clip_id)?.clone();
            ensure_unlocked(project, &existing.track_id)?;
            let group = existing.link_group;
            let mut created: Vec<Value> = Vec::new();
            split_one(project, &existing, *at, &mut created)?;
            if let Some(group) = group {
                let partners: Vec<Clip> = project
                    .clips
                    .iter()
                    .filter(|c| c.link_group == Some(group) && &c.id != clip_id)
                    .cloned()
                    .collect();
                for partner in partners {
                    if partner.timeline_start < *at && *at < partner.timeline_end() {
                        split_one(project, &partner, *at, &mut created)?;
                    }
                }
            }
            Ok(json!({"clip_id": clip_id, "created": created}))
        }
        Command::RemoveClip { clip_id } => {
            let existing = require_clip(project, clip_id)?;
            ensure_unlocked(project, &existing.track_id)?;
            project.clips.retain(|c| &c.id != clip_id);
            Ok(json!({"clip_id": clip_id, "removed": true}))
        }
        Command::UpdateClip {
            clip_id,
            volume,
            opacity,
            scale,
            x_offset,
            y_offset,
            fade_in_ticks,
            fade_out_ticks,
        } => {
            let existing = require_clip(project, clip_id)?;
            ensure_unlocked(project, &existing.track_id)?;
            if let Some(v) = volume {
                validate_gain(*v)?;
            }
            if let Some(v) = opacity {
                if !(0.0..=1.0).contains(v) {
                    return Err(invalid("opacity must be 0..1"));
                }
            }
            if let Some(v) = scale {
                if !(0.05..=20.0).contains(v) {
                    return Err(invalid("scale must be 0.05..20"));
                }
            }
            let duration = existing.source_duration();
            if let Some(v) = fade_in_ticks {
                if *v < 0 || *v > duration {
                    return Err(invalid("fade_in exceeds clip duration"));
                }
            }
            if let Some(v) = fade_out_ticks {
                if *v < 0 || *v > duration {
                    return Err(invalid("fade_out exceeds clip duration"));
                }
            }
            let clip = require_clip_mut(project, clip_id)?;
            if let Some(v) = volume {
                clip.volume = *v;
            }
            if let Some(v) = opacity {
                clip.opacity = *v;
            }
            if let Some(v) = scale {
                clip.scale = *v;
            }
            if let Some(v) = x_offset {
                clip.x_offset = *v;
            }
            if let Some(v) = y_offset {
                clip.y_offset = *v;
            }
            if let Some(v) = fade_in_ticks {
                clip.fade_in_ticks = *v;
            }
            if let Some(v) = fade_out_ticks {
                clip.fade_out_ticks = *v;
            }
            Ok(json!({"clip_id": clip_id}))
        }

        // ----- range removal -----
        Command::RemoveRanges {
            start,
            end,
            ripple,
            tracks,
        } => {
            let range = TimeRange {
                start: *start,
                end: *end,
            };
            if range.end <= range.start {
                return Err(CoreError::InvalidTime(format!(
                    "range {}..{} is empty",
                    range.start, range.end
                )));
            }
            if range.start < 0 {
                return Err(CoreError::InvalidTime("range start must be >= 0".into()));
            }
            let ripple = ripple.unwrap_or(true);
            let targets: Vec<Uuid> = match tracks {
                Some(list) => {
                    for id in list {
                        require_track(project, id)?;
                        ensure_unlocked(project, id)?;
                    }
                    list.clone()
                }
                None => {
                    let mut all: Vec<Uuid> = Vec::new();
                    for track in &project.tracks {
                        if !track.locked {
                            all.push(track.id);
                        }
                    }
                    all
                }
            };
            let gap = range.end - range.start;
            let mut removed_clips = 0usize;
            let mut shifted_clips = 0usize;
            for track_id in &targets {
                let (removed, shifted) =
                    remove_range_on_track(project, track_id, &range, ripple);
                removed_clips += removed;
                shifted_clips += shifted;
            }
            // Captions: delete inside, trim overlapping, shift the rest.
            let mut kept: Vec<Caption> = Vec::new();
            for caption in project.captions.drain(..) {
                let mut c = caption;
                if c.end <= range.start || c.start >= range.end {
                    if ripple && c.start >= range.end {
                        c.start -= gap;
                        c.end -= gap;
                    }
                    if c.end > c.start {
                        kept.push(c);
                    }
                    continue;
                }
                if c.start < range.start {
                    let mut left = c.clone();
                    left.end = range.start;
                    if left.end > left.start {
                        left.id = Uuid::new_v4();
                        kept.push(left);
                    }
                }
                if c.end > range.end {
                    let mut right = c.clone();
                    right.start = range.end;
                    if ripple {
                        right.start -= gap;
                        right.end -= gap;
                    }
                    if right.end > right.start {
                        right.id = Uuid::new_v4();
                        kept.push(right);
                    }
                }
            }
            let before_caps = project.captions.len();
            project.captions = kept;
            // Transitions: delete inside the range, shift later ones.
            let mut kept_trs: Vec<Transition> = Vec::new();
            for tr in project.transitions.drain(..) {
                if tr.at >= range.start && tr.at < range.end {
                    continue;
                }
                let mut tr = tr;
                if ripple && tr.at >= range.end {
                    tr.at -= gap;
                }
                kept_trs.push(tr);
            }
            project.transitions = kept_trs;
            // Playhead lands at the gap start when inside the removed span.
            if project.editor.playhead_ticks >= range.start
                && project.editor.playhead_ticks < range.end
            {
                project.editor.playhead_ticks = range.start;
            } else if ripple && project.editor.playhead_ticks >= range.end {
                project.editor.playhead_ticks -= gap;
            }
            Ok(json!({
                "range": {"start": range.start, "end": range.end},
                "ripple": ripple,
                "tracks": targets,
                "removed_clips": removed_clips,
                "shifted_clips": shifted_clips,
                "captions_before": before_caps,
                "captions_after": project.captions.len(),
            }))
        }

        // ----- transitions -----
        Command::AddTransition {
            at,
            duration,
            transition_kind,
            color,
        } => {
            if *at < 0 {
                return Err(CoreError::InvalidTime("transition at must be >= 0".into()));
            }
            if *duration <= 0 {
                return Err(CoreError::InvalidTime(
                    "transition duration must be > 0".into(),
                ));
            }
            let color = color.clone().unwrap_or_else(|| "#000000".into());
            validate_hex_color(&color).map_err(CoreError::Validation)?;
            let kind = transition_kind.unwrap_or(TransitionKind::FadeThroughColor);
            if project.transitions.iter().any(|t| t.at == *at) {
                return Err(invalid(format!(
                    "a transition already exists at {}",
                    crate::ticks::format_seconds(*at)
                )));
            }
            let tr = Transition {
                id: Uuid::new_v4(),
                at: *at,
                duration: *duration,
                kind,
                color,
            };
            let id = tr.id;
            project.transitions.push(tr);
            Ok(json!({"transition_id": id}))
        }
        Command::RemoveTransition { transition_id } => {
            let before = project.transitions.len();
            project.transitions.retain(|t| &t.id != transition_id);
            if project.transitions.len() == before {
                return Err(CoreError::InvalidId(format!(
                    "unknown transition {transition_id}"
                )));
            }
            Ok(json!({"removed": true}))
        }

        // ----- captions -----
        Command::SetCaptionStyle { style } => {
            validate_caption_style(style)?;
            project.caption_style = style.clone();
            Ok(json!({"style": style}))
        }
        Command::AddCaption { start, end, text } => {
            if text.trim().is_empty() {
                return Err(invalid("caption text cannot be empty"));
            }
            if *end <= *start {
                return Err(CoreError::InvalidTime(format!(
                    "caption end {end} must be greater than start {start}"
                )));
            }
            if *start < 0 {
                return Err(CoreError::InvalidTime("caption start must be >= 0".into()));
            }
            let caption = Caption {
                id: Uuid::new_v4(),
                start: *start,
                end: *end,
                text: text.trim().to_string(),
            };
            let data = caption_json(&caption);
            let id = caption.id;
            project.captions.push(caption);
            project.captions.sort_by_key(|c| c.start);
            Ok(json!({"caption_id": id, "caption": data}))
        }
        Command::UpdateCaption {
            caption_id,
            start,
            end,
            text,
        } => {
            let existing = project
                .captions
                .iter()
                .find(|c| &c.id == caption_id)
                .cloned()
                .ok_or_else(|| CoreError::InvalidId(format!("unknown caption {caption_id}")))?;
            let new_start = start.unwrap_or(existing.start);
            let new_end = end.unwrap_or(existing.end);
            let new_text = text
                .as_ref()
                .map(|t| t.trim().to_string())
                .unwrap_or_else(|| existing.text.clone());
            if new_text.is_empty() {
                return Err(invalid("caption text cannot be empty"));
            }
            if new_end <= new_start {
                return Err(CoreError::InvalidTime(
                    "caption end must be greater than start".into(),
                ));
            }
            let caption = project
                .captions
                .iter_mut()
                .find(|c| &c.id == caption_id)
                .expect("caption was found above");
            caption.start = new_start;
            caption.end = new_end;
            caption.text = new_text;
            project.captions.sort_by_key(|c| c.start);
            Ok(json!({"caption_id": caption_id}))
        }
        Command::RemoveCaption { caption_id } => {
            let before = project.captions.len();
            project.captions.retain(|c| &c.id != caption_id);
            if project.captions.len() == before {
                return Err(CoreError::InvalidId(format!(
                    "unknown caption {caption_id}"
                )));
            }
            Ok(json!({"removed": true}))
        }

        other => Err(CoreError::NotPure { kind: other.kind() }),
    }
}

/// Apply a viewer-state command (playhead, selection, zoom).
pub fn apply_editor_state(project: &mut Project, cmd: &Command) -> Result<Value, CoreError> {
    match cmd {
        Command::SetPlayhead { ticks } => {
            if *ticks < 0 {
                return Err(CoreError::InvalidTime("playhead must be >= 0".into()));
            }
            project.editor.playhead_ticks = *ticks;
            Ok(json!({"playhead_ticks": *ticks}))
        }
        Command::SetSelection { selection } => {
            let mut cleaned: Vec<Selection> = Vec::new();
            for sel in selection {
                let valid = match sel {
                    Selection::Clip(id) => project.clip(id).is_some(),
                    Selection::Caption(id) => {
                        project.captions.iter().any(|c| &c.id == id)
                    }
                    Selection::Transition(id) => {
                        project.transitions.iter().any(|t| &t.id == id)
                    }
                    Selection::Asset(id) => project.asset(id).is_some(),
                    Selection::Track(id) => project.track(id).is_some(),
                };
                if valid && !cleaned.contains(sel) {
                    cleaned.push(sel.clone());
                }
            }
            project.editor.selection = cleaned;
            let count = project.editor.selection.len();
            Ok(json!({"selection_count": count}))
        }
        Command::SetZoom {
            pixels_per_second,
        } => {
            if !(1.0..=5_000.0).contains(pixels_per_second) {
                return Err(invalid(
                    "zoom must be between 1 and 5000 pixels per second",
                ));
            }
            project.editor.zoom_pixels_per_second = *pixels_per_second;
            Ok(json!({"pixels_per_second": *pixels_per_second}))
        }
        other => Err(CoreError::NotPure { kind: other.kind() }),
    }
}

/// Validate a command against a copy of the project without mutating it.
pub fn dry_run(project: &Project, cmd: &Command) -> Result<Value, CoreError> {
    let mut clone = project.clone();
    if cmd.is_pure_edit() {
        apply_edit(&mut clone, cmd)
    } else if cmd.is_editor_state() {
        apply_editor_state(&mut clone, cmd)
    } else {
        Err(CoreError::NotPure { kind: cmd.kind() })
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn parse_track_kind(s: &str) -> Result<TrackKind, CoreError> {
    match s.to_ascii_lowercase().as_str() {
        "video" | "v" => Ok(TrackKind::Video),
        "audio" | "a" => Ok(TrackKind::Audio),
        other => Err(invalid(format!(
            "unknown track kind `{other}` (expected video or audio)"
        ))),
    }
}

fn validate_dimension(v: u32, what: &str) -> Result<(), CoreError> {
    if !v.is_multiple_of(2) || !(2..=7680).contains(&v) {
        return Err(invalid(format!(
            "{what} must be even and between 2 and 7680 (got {v})"
        )));
    }
    Ok(())
}

fn validate_gain(v: f32) -> Result<(), CoreError> {
    if !(0.0..=4.0).contains(&v) {
        return Err(invalid("volume must be 0..4 (1.0 = unity)"));
    }
    Ok(())
}

fn validate_caption_style(style: &CaptionStyle) -> Result<(), CoreError> {
    validate_hex_color(&style.primary_color).map_err(CoreError::Validation)?;
    validate_hex_color(&style.outline_color).map_err(CoreError::Validation)?;
    if !(8..=512).contains(&style.font_size) {
        return Err(invalid("caption font_size must be 8..512"));
    }
    if !(1..=9).contains(&style.alignment) {
        return Err(invalid("caption alignment must be 1..9 (ASS numpad)"));
    }
    if style.font.trim().is_empty() {
        return Err(invalid("caption font cannot be empty"));
    }
    Ok(())
}

/// Split one clip at an absolute timeline tick, leaving the original as the
/// left half and creating the right half. Pushes new clip summaries.
fn split_one(
    project: &mut Project,
    existing: &Clip,
    at: Ticks,
    created: &mut Vec<Value>,
) -> Result<(), CoreError> {
    if at <= existing.timeline_start || at >= existing.timeline_end() {
        return Err(CoreError::InvalidTime(format!(
            "split point {} is not inside clip {}..{}",
            crate::ticks::format_seconds(at),
            crate::ticks::format_seconds(existing.timeline_start),
            crate::ticks::format_seconds(existing.timeline_end())
        )));
    }
    let left_duration = at - existing.timeline_start;
    let mut right = new_clip(
        existing.asset_id,
        existing.track_id,
        at,
        existing.source_in + left_duration,
        existing.source_out,
        None,
    );
    right.opacity = existing.opacity;
    right.scale = existing.scale;
    right.x_offset = existing.x_offset;
    right.y_offset = existing.y_offset;
    right.volume = existing.volume;
    right.fade_in_ticks = existing.fade_in_ticks.min(existing.source_duration() - left_duration);
    right.fade_out_ticks = existing.fade_out_ticks;
    let clip = require_clip_mut(project, &existing.id)?;
    clip.source_out = clip.source_in + left_duration;
    clip.fade_out_ticks = clip.fade_out_ticks.min(left_duration);
    clip.link_group = None;
    created.push(clip_json(&right));
    project.clips.push(right);
    Ok(())
}

/// Remove `range` on one track. Returns (removed clip count, shifted clip count).
fn remove_range_on_track(
    project: &mut Project,
    track_id: &Uuid,
    range: &TimeRange,
    ripple: bool,
) -> (usize, usize) {
    let gap = range.end - range.start;
    let mut kept: Vec<Clip> = Vec::new();
    let mut removed = 0usize;
    let mut shifted = 0usize;
    for clip in project.clips.drain(..) {
        if &clip.track_id != track_id {
            kept.push(clip);
            continue;
        }
        let s = clip.timeline_start;
        let e = clip.timeline_end();
        if e <= range.start {
            kept.push(clip);
            continue;
        }
        if s >= range.end {
            if ripple {
                let mut c = clip;
                c.timeline_start -= gap;
                shifted += 1;
                kept.push(c);
            } else {
                kept.push(clip);
            }
            continue;
        }
        // Overlaps the range: keep fragments outside it.
        if s < range.start {
            let mut left = clip.clone();
            left.source_out = left.source_in + (range.start - s);
            left.fade_out_ticks = left.fade_out_ticks.min(left.source_duration());
            left.link_group = None;
            if left.source_duration() > 0 {
                left.id = Uuid::new_v4();
                kept.push(left);
            }
        }
        if e > range.end {
            let mut right = clip.clone();
            let used_before = range.end - s; // source ticks consumed before the gap
            right.source_in += used_before;
            if ripple {
                right.timeline_start = range.start;
                shifted += 1;
            } else {
                right.timeline_start = range.end;
            }
            right.fade_in_ticks = right.fade_in_ticks.min(right.source_duration());
            right.link_group = None;
            if right.source_duration() > 0 {
                right.id = Uuid::new_v4();
                kept.push(right);
            }
        }
        removed += 1;
    }
    project.clips = kept;
    (removed, shifted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Asset, MediaProbe, ProjectSettings};

    fn video_asset(name: &str, duration_secs: f64) -> Asset {
        Asset {
            id: Uuid::new_v4(),
            kind: AssetKind::Video,
            name: name.into(),
            path: format!("/tmp/{name}.mp4").into(),
            probe: Some(MediaProbe {
                duration_ticks: crate::ticks::ticks_from_secs(duration_secs),
                width: 1920,
                height: 1080,
                fps: Some(crate::model::Fps::from_int(30)),
                has_audio: true,
                rotation_deg: 0,
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                sample_rate: Some(48_000),
            }),
            generated: None,
        }
    }

    fn project_with_clip() -> (Project, Uuid, Uuid, Uuid) {
        let mut p = Project::new("t", ProjectSettings::default());
        let asset = video_asset("a", 10.0);
        let asset_id = asset.id;
        p.assets.push(asset);
        let v1 = p.tracks[0].id;
        let a1 = p.tracks[1].id;
        let group = Uuid::new_v4();
        p.clips.push(new_clip(asset_id, v1, 0, 0, crate::ticks::ticks_from_secs(10.0), Some(group)));
        let a_clip = new_clip(asset_id, a1, 0, 0, crate::ticks::ticks_from_secs(10.0), Some(group));
        let a_clip_id = a_clip.id;
        p.clips.push(a_clip);
        let video_clip_id = p.clips[0].id;
        (p, asset_id, video_clip_id, a_clip_id)
    }

    #[test]
    fn split_cuts_linked_partner() {
        let (mut p, _asset, vid, aud) = project_with_clip();
        let at = crate::ticks::ticks_from_secs(4.0);
        apply_edit(
            &mut p,
            &Command::SplitClip {
                clip_id: vid,
                at,
            },
        )
        .unwrap();
        // 1 original video clip -> 2, partner audio also split -> 4 total.
        assert_eq!(p.clips.len(), 4);
        let video_count = p
            .clips
            .iter()
            .filter(|c| c.track_id == p.tracks[0].id)
            .count();
        assert_eq!(video_count, 2);
        let _ = aud;
    }

    #[test]
    fn remove_ranges_ripples_and_trims() {
        let (mut p, _asset, vid, _aud) = project_with_clip();
        let s = crate::ticks::TICKS_PER_SEC * 2;
        let e = crate::ticks::TICKS_PER_SEC * 5;
        let out = apply_edit(
            &mut p,
            &Command::RemoveRanges {
                start: s,
                end: e,
                ripple: Some(true),
                tracks: None,
            },
        )
        .unwrap();
        assert_eq!(out["removed_clips"], 2);
        // One video fragment pair + one audio fragment pair.
        assert_eq!(p.clips.len(), 4);
        let video_track = p.tracks[0].id;
        let video_total: i64 = p
            .clips
            .iter()
            .filter(|c| c.track_id == video_track)
            .map(|c| c.source_duration())
            .sum();
        assert_eq!(video_total, crate::ticks::ticks_from_secs(7.0));
        let _ = vid;
    }

    #[test]
    fn overlapping_insert_rejected() {
        let (mut p, asset, _vid, _aud) = project_with_clip();
        let v1 = p.tracks[0].id;
        let err = apply_edit(
            &mut p,
            &Command::InsertClip {
                asset_id: asset,
                track_id: v1,
                timeline_start: crate::ticks::TICKS_PER_SEC * 9,
                source_in: 0,
                source_out: crate::ticks::TICKS_PER_SEC * 2,
                link_group: None,
            },
        );
        assert!(err.is_err());
    }
}

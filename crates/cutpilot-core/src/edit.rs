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
    AssetKind, Caption, CaptionStyle, Clip, ClipMotion, ColorGrade, CropRect, Marker, Project,
    Selection, Track, TrackKind, Transition, TransitionKind,
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
        speed: 1.0,
        rotation_deg: 0.0,
        flip_h: false,
        flip_v: false,
        crop: CropRect::default(),
        motion: None,
        color: ColorGrade::default(),
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
            let duration = clip.timeline_duration();
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
                            other.timeline_duration(),
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
            let existing = require_clip(project, clip_id)?.clone();
            ensure_unlocked(project, &existing.track_id)?;
            let new_start = timeline_start.unwrap_or(existing.timeline_start);
            let new_in = source_in.unwrap_or(existing.source_in);
            let new_out = source_out.unwrap_or(existing.source_out);
            validate_clip_times(project, &existing.asset_id, new_in, new_out)?;
            if new_start < 0 {
                return Err(CoreError::InvalidTime("timeline_start must be >= 0".into()));
            }
            let new_tl_dur =
                (((new_out - new_in) as f64) / existing.effective_speed() as f64)
                    .round()
                    .max(1.0) as Ticks;
            check_overlap(
                project,
                &existing.track_id,
                new_start,
                new_start + new_tl_dur,
                Some(clip_id),
            )?;
            let delta_start = new_start - existing.timeline_start;
            let delta_in = new_in - existing.source_in;
            let delta_out = new_out - existing.source_out;

            // Validate linked partners before mutating anything.
            let mut partner_updates: Vec<(Uuid, Ticks, Ticks, Ticks)> = Vec::new();
            if let Some(group) = existing.link_group {
                let partners: Vec<Clip> = project
                    .clips
                    .iter()
                    .filter(|c| c.link_group == Some(group) && &c.id != clip_id)
                    .cloned()
                    .collect();
                for partner in partners {
                    ensure_unlocked(project, &partner.track_id)?;
                    let p_start = partner.timeline_start + delta_start;
                    let p_in = partner.source_in + delta_in;
                    let p_out = partner.source_out + delta_out;
                    if p_start < 0 {
                        return Err(CoreError::InvalidTime(
                            "linked partner timeline_start must be >= 0".into(),
                        ));
                    }
                    validate_clip_times(project, &partner.asset_id, p_in, p_out)?;
                    let p_tl_dur =
                        (((p_out - p_in) as f64) / partner.effective_speed() as f64)
                            .round()
                            .max(1.0) as Ticks;
                    check_overlap(
                        project,
                        &partner.track_id,
                        p_start,
                        p_start + p_tl_dur,
                        Some(&partner.id),
                    )?;
                    partner_updates.push((partner.id, p_start, p_in, p_out));
                }
            }

            let clip = require_clip_mut(project, clip_id)?;
            clip.timeline_start = new_start;
            clip.source_in = new_in;
            clip.source_out = new_out;
            let fade_limit = clip.timeline_duration();
            clip.fade_in_ticks = clip.fade_in_ticks.min(fade_limit);
            clip.fade_out_ticks = clip.fade_out_ticks.min(fade_limit);

            for (pid, p_start, p_in, p_out) in &partner_updates {
                if let Ok(pclip) = require_clip_mut(project, pid) {
                    pclip.timeline_start = *p_start;
                    pclip.source_in = *p_in;
                    pclip.source_out = *p_out;
                    let fl = pclip.timeline_duration();
                    pclip.fade_in_ticks = pclip.fade_in_ticks.min(fl);
                    pclip.fade_out_ticks = pclip.fade_out_ticks.min(fl);
                }
            }

            Ok(json!({
                "clip_id": clip_id,
                "trimmed_group": !partner_updates.is_empty(),
            }))
        }
        Command::SplitClip { clip_id, at } => {
            let existing = require_clip(project, clip_id)?.clone();
            ensure_unlocked(project, &existing.track_id)?;
            let group = existing.link_group;
            let (left_group, right_group) = if group.is_some() {
                (Some(Uuid::new_v4()), Some(Uuid::new_v4()))
            } else {
                (None, None)
            };
            let mut created: Vec<Value> = Vec::new();
            split_one(project, &existing, *at, left_group, right_group, &mut created)?;
            if let Some(group) = group {
                let partners: Vec<Clip> = project
                    .clips
                    .iter()
                    .filter(|c| c.link_group == Some(group) && &c.id != clip_id)
                    .cloned()
                    .collect();
                for partner in partners {
                    if partner.timeline_start < *at && *at < partner.timeline_end() {
                        ensure_unlocked(project, &partner.track_id)?;
                        split_one(project, &partner, *at, left_group, right_group, &mut created)?;
                    }
                }
            }
            Ok(json!({"clip_id": clip_id, "created": created}))
        }
        Command::RemoveClip { clip_id } => {
            let existing = require_clip(project, clip_id)?.clone();
            ensure_unlocked(project, &existing.track_id)?;
            let mut to_remove = vec![existing.id];
            if let Some(group) = existing.link_group {
                for other in &project.clips {
                    if other.link_group == Some(group) && other.id != existing.id {
                        ensure_unlocked(project, &other.track_id)?;
                        to_remove.push(other.id);
                    }
                }
            }
            project.clips.retain(|c| !to_remove.contains(&c.id));
            project.editor.selection.retain(|s| match s {
                Selection::Clip(id) => !to_remove.contains(id),
                _ => true,
            });
            Ok(json!({
                "clip_id": clip_id,
                "removed": true,
                "removed_count": to_remove.len(),
            }))
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
            rotation_deg,
            flip_h,
            flip_v,
            crop,
        } => {
            let existing = require_clip(project, clip_id)?;
            ensure_unlocked(project, &existing.track_id)?;
            if let Some(v) = volume {
                validate_gain(*v)?;
            }
            if let Some(v) = opacity {
                if !v.is_finite() || !(0.0..=1.0).contains(v) {
                    return Err(invalid("opacity must be 0..1"));
                }
            }
            if let Some(v) = scale {
                if !v.is_finite() || !(0.05..=20.0).contains(v) {
                    return Err(invalid("scale must be 0.05..20"));
                }
            }
            if let Some(v) = rotation_deg {
                if !v.is_finite() || !(-360.0..=360.0).contains(v) {
                    return Err(invalid("rotation_deg must be between -360 and 360"));
                }
            }
            if let Some(c) = crop {
                validate_crop(c)?;
            }
            let duration = existing.timeline_duration();
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
            if let Some(v) = rotation_deg {
                clip.rotation_deg = *v;
            }
            if let Some(v) = flip_h {
                clip.flip_h = *v;
            }
            if let Some(v) = flip_v {
                clip.flip_v = *v;
            }
            if let Some(c) = crop {
                clip.crop = *c;
            }
            Ok(json!({"clip_id": clip_id}))
        }
        Command::SetClipMotion { clip_id, motion } => {
            let existing = require_clip(project, clip_id)?;
            ensure_unlocked(project, &existing.track_id)?;
            if let Some(m) = motion {
                validate_motion(m)?;
            }
            let clip = require_clip_mut(project, clip_id)?;
            clip.motion = motion.clone();
            Ok(json!({"clip_id": clip_id, "motion": motion}))
        }
        Command::SetClipSpeed { clip_id, speed } => {
            if !speed.is_finite() || !(0.25..=8.0).contains(speed) {
                return Err(invalid(
                    "speed must be between 0.25 and 8.0 (1.0 = normal speed)",
                ));
            }
            let existing = require_clip(project, clip_id)?.clone();
            ensure_unlocked(project, &existing.track_id)?;
            let new_dur = (((existing.source_duration() as f64) / (*speed as f64))
                .round()
                .max(1.0)) as Ticks;
            check_overlap(
                project,
                &existing.track_id,
                existing.timeline_start,
                existing.timeline_start + new_dur,
                Some(clip_id),
            )?;
            let mut partner_ids: Vec<Uuid> = Vec::new();
            if let Some(group) = existing.link_group {
                let partners: Vec<Clip> = project
                    .clips
                    .iter()
                    .filter(|c| c.link_group == Some(group) && &c.id != clip_id)
                    .cloned()
                    .collect();
                for partner in partners {
                    ensure_unlocked(project, &partner.track_id)?;
                    let p_dur = (((partner.source_duration() as f64) / (*speed as f64))
                        .round()
                        .max(1.0)) as Ticks;
                    check_overlap(
                        project,
                        &partner.track_id,
                        partner.timeline_start,
                        partner.timeline_start + p_dur,
                        Some(&partner.id),
                    )?;
                    partner_ids.push(partner.id);
                }
            }
            let clip = require_clip_mut(project, clip_id)?;
            clip.speed = *speed;
            let fl = clip.timeline_duration();
            clip.fade_in_ticks = clip.fade_in_ticks.min(fl);
            clip.fade_out_ticks = clip.fade_out_ticks.min(fl);
            for pid in &partner_ids {
                if let Ok(pclip) = require_clip_mut(project, pid) {
                    pclip.speed = *speed;
                    let pfl = pclip.timeline_duration();
                    pclip.fade_in_ticks = pclip.fade_in_ticks.min(pfl);
                    pclip.fade_out_ticks = pclip.fade_out_ticks.min(pfl);
                }
            }
            Ok(json!({
                "clip_id": clip_id,
                "speed": *speed,
                "timeline_duration": new_dur,
                "updated_group": !partner_ids.is_empty(),
            }))
        }
        Command::SetClipColor { clip_id, color } => {
            let existing = require_clip(project, clip_id)?;
            ensure_unlocked(project, &existing.track_id)?;
            validate_color_grade(color)?;
            let clip = require_clip_mut(project, clip_id)?;
            clip.color = *color;
            Ok(json!({"clip_id": clip_id, "color": color}))
        }
        Command::DuplicateClip {
            clip_id,
            timeline_start,
        } => {
            let existing = require_clip(project, clip_id)?.clone();
            ensure_unlocked(project, &existing.track_id)?;
            let partners: Vec<Clip> = if let Some(group) = existing.link_group {
                let ps: Vec<Clip> = project
                    .clips
                    .iter()
                    .filter(|c| c.link_group == Some(group) && &c.id != clip_id)
                    .cloned()
                    .collect();
                for p in &ps {
                    ensure_unlocked(project, &p.track_id)?;
                }
                ps
            } else {
                Vec::new()
            };

            let dest_start = match timeline_start {
                Some(ts) => {
                    if *ts < 0 {
                        return Err(CoreError::InvalidTime("timeline_start must be >= 0".into()));
                    }
                    *ts
                }
                None => {
                    // Prefer immediately after the source clip; if that slot is
                    // occupied on the clip's track or any linked partner's track,
                    // place at the end of those tracks.
                    let immediate = existing.timeline_end();
                    let delta = immediate - existing.timeline_start;
                    let immediate_ok = check_overlap(
                        project,
                        &existing.track_id,
                        immediate,
                        immediate + existing.timeline_duration(),
                        None,
                    )
                    .is_ok()
                        && partners.iter().all(|p| {
                            let ps = p.timeline_start + delta;
                            ps >= 0
                                && check_overlap(
                                    project,
                                    &p.track_id,
                                    ps,
                                    ps + p.timeline_duration(),
                                    None,
                                )
                                .is_ok()
                        });
                    if immediate_ok {
                        immediate
                    } else {
                        let mut track_ids = vec![existing.track_id];
                        for p in &partners {
                            if !track_ids.contains(&p.track_id) {
                                track_ids.push(p.track_id);
                            }
                        }
                        project
                            .clips
                            .iter()
                            .filter(|c| track_ids.contains(&c.track_id))
                            .map(|c| c.timeline_end())
                            .max()
                            .unwrap_or(0)
                    }
                }
            };

            let delta = dest_start - existing.timeline_start;
            check_overlap(
                project,
                &existing.track_id,
                dest_start,
                dest_start + existing.timeline_duration(),
                None,
            )?;
            for p in &partners {
                let p_start = p.timeline_start + delta;
                if p_start < 0 {
                    return Err(CoreError::InvalidTime(
                        "duplicated linked partner timeline_start must be >= 0".into(),
                    ));
                }
                check_overlap(
                    project,
                    &p.track_id,
                    p_start,
                    p_start + p.timeline_duration(),
                    None,
                )?;
            }

            let new_group = if !partners.is_empty() {
                Some(Uuid::new_v4())
            } else {
                None
            };
            let mut copy = existing.clone();
            copy.id = Uuid::new_v4();
            copy.timeline_start = dest_start;
            copy.link_group = new_group;
            let new_clip_id = copy.id;
            let mut created_ids = vec![new_clip_id];
            project.clips.push(copy);

            for p in partners {
                let mut p_copy = p;
                p_copy.id = Uuid::new_v4();
                p_copy.timeline_start += delta;
                p_copy.link_group = new_group;
                created_ids.push(p_copy.id);
                project.clips.push(p_copy);
            }

            Ok(json!({
                "clip_id": new_clip_id,
                "timeline_start": dest_start,
                "duplicated_count": created_ids.len(),
                "created_ids": created_ids,
            }))
        }
        Command::UnlinkClip { clip_id } => {
            let existing = require_clip(project, clip_id)?.clone();
            ensure_unlocked(project, &existing.track_id)?;
            let Some(group) = existing.link_group else {
                return Ok(json!({"clip_id": clip_id, "unlinked": false}));
            };
            let clip = require_clip_mut(project, clip_id)?;
            clip.link_group = None;
            // If only a single clip remains in the old link group, clear its
            // link_group too so there are no orphan 1-member groups.
            let remaining: Vec<Uuid> = project
                .clips
                .iter()
                .filter(|c| c.link_group == Some(group))
                .map(|c| c.id)
                .collect();
            if remaining.len() == 1 {
                if let Ok(sole) = require_clip_mut(project, &remaining[0]) {
                    sole.link_group = None;
                }
            }
            Ok(json!({"clip_id": clip_id, "unlinked": true}))
        }

        // ----- markers -----
        Command::AddMarker { at, label, color } => {
            if *at < 0 {
                return Err(CoreError::InvalidTime("marker at must be >= 0".into()));
            }
            let label = label.trim().to_string();
            if label.is_empty() {
                return Err(invalid("marker label cannot be empty"));
            }
            let color = color.clone().unwrap_or_else(|| "#F59E0B".into());
            validate_hex_color(&color).map_err(CoreError::Validation)?;
            let marker = Marker {
                id: Uuid::new_v4(),
                at: *at,
                label: label.clone(),
                color: color.clone(),
            };
            let id = marker.id;
            project.markers.push(marker);
            project.markers.sort_by_key(|m| m.at);
            Ok(json!({
                "marker_id": id,
                "at": *at,
                "label": label,
                "color": color,
            }))
        }
        Command::RemoveMarker { marker_id } => {
            let before = project.markers.len();
            project.markers.retain(|m| &m.id != marker_id);
            if project.markers.len() == before {
                return Err(CoreError::InvalidId(format!(
                    "unknown marker {marker_id}"
                )));
            }
            Ok(json!({"removed": true}))
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
            let mut link_remap: std::collections::HashMap<(Uuid, bool), Uuid> =
                std::collections::HashMap::new();
            for track_id in &targets {
                let (removed, shifted) = remove_range_on_track(
                    project,
                    track_id,
                    &range,
                    ripple,
                    &mut link_remap,
                );
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
            // Markers: delete inside the range, shift later ones when rippling.
            let mut kept_markers: Vec<Marker> = Vec::new();
            for m in project.markers.drain(..) {
                if m.at >= range.start && m.at < range.end {
                    continue;
                }
                let mut m = m;
                if ripple && m.at >= range.end {
                    m.at -= gap;
                }
                kept_markers.push(m);
            }
            project.markers = kept_markers;
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
        Command::TranscriptToCaptions { asset_id, replace } => {
            if let Some(aid) = asset_id {
                if project.transcript(aid).is_none() {
                    return Err(invalid(format!(
                        "asset {aid} has not been transcribed yet"
                    )));
                }
            } else if project.transcripts.is_empty() {
                return Err(invalid("no transcripts exist in this project yet"));
            }
            let mut cues: Vec<(Ticks, Ticks, String)> = project
                .timeline_transcript_segments(*asset_id)
                .into_iter()
                .map(|s| (s.timeline_start, s.timeline_end, s.text))
                .collect();
            // Fallback: if the asset isn't placed on the timeline yet, use its
            // raw source timestamps directly.
            if cues.is_empty() {
                if let Some(aid) = asset_id {
                    if let Some(tr) = project.transcript(aid) {
                        cues = tr
                            .segments
                            .iter()
                            .filter(|s| s.end > s.start && !s.text.trim().is_empty())
                            .map(|s| (s.start, s.end, s.text.trim().to_string()))
                            .collect();
                    }
                }
            }
            if cues.is_empty() {
                return Err(invalid("transcript contains no timed speech segments"));
            }
            if replace.unwrap_or(false) {
                project.captions.clear();
            }
            let mut added_ids = Vec::with_capacity(cues.len());
            for (start, end, text) in cues {
                let text = text.trim().to_string();
                if text.is_empty() || end <= start {
                    continue;
                }
                let cap = Caption {
                    id: Uuid::new_v4(),
                    start,
                    end,
                    text,
                };
                added_ids.push(cap.id);
                project.captions.push(cap);
            }
            project.captions.sort_by_key(|c| c.start);
            Ok(json!({
                "added": added_ids.len(),
                "caption_ids": added_ids,
            }))
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
            if !pixels_per_second.is_finite() || !(0.0001..=10_000.0).contains(pixels_per_second) {
                return Err(invalid(
                    "zoom must be between 0.0001 and 10000 pixels per second",
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
    if !v.is_finite() || !(0.0..=4.0).contains(&v) {
        return Err(invalid("volume must be 0..4 (1.0 = unity)"));
    }
    Ok(())
}

fn validate_crop(c: &CropRect) -> Result<(), CoreError> {
    for (name, v) in [
        ("crop.left", c.left),
        ("crop.right", c.right),
        ("crop.top", c.top),
        ("crop.bottom", c.bottom),
    ] {
        if !v.is_finite() || !(0.0..=0.45).contains(&v) {
            return Err(invalid(format!("{name} must be between 0.0 and 0.45")));
        }
    }
    if c.left + c.right >= 0.95 {
        return Err(invalid("crop.left + crop.right must be < 0.95"));
    }
    if c.top + c.bottom >= 0.95 {
        return Err(invalid("crop.top + crop.bottom must be < 0.95"));
    }
    Ok(())
}

fn validate_motion(m: &ClipMotion) -> Result<(), CoreError> {
    if !m.start_scale.is_finite() || !(1.0..=10.0).contains(&m.start_scale) {
        return Err(invalid("motion.start_scale must be between 1.0 and 10.0"));
    }
    if !m.end_scale.is_finite() || !(1.0..=10.0).contains(&m.end_scale) {
        return Err(invalid("motion.end_scale must be between 1.0 and 10.0"));
    }
    for (name, v) in [
        ("start_pan_x", m.start_pan_x),
        ("start_pan_y", m.start_pan_y),
        ("end_pan_x", m.end_pan_x),
        ("end_pan_y", m.end_pan_y),
    ] {
        if !v.is_finite() || !(-1.0..=1.0).contains(&v) {
            return Err(invalid(format!("motion.{name} must be between -1.0 and 1.0")));
        }
    }
    Ok(())
}

fn validate_color_grade(c: &ColorGrade) -> Result<(), CoreError> {
    if !c.brightness.is_finite() || !(-1.0..=1.0).contains(&c.brightness) {
        return Err(invalid("color.brightness must be between -1.0 and 1.0"));
    }
    if !c.contrast.is_finite() || !(0.0..=3.0).contains(&c.contrast) {
        return Err(invalid("color.contrast must be between 0.0 and 3.0"));
    }
    if !c.saturation.is_finite() || !(0.0..=3.0).contains(&c.saturation) {
        return Err(invalid("color.saturation must be between 0.0 and 3.0"));
    }
    if !c.gamma.is_finite() || !(0.1..=4.0).contains(&c.gamma) {
        return Err(invalid("color.gamma must be between 0.1 and 4.0"));
    }
    if !c.blur.is_finite() || !(0.0..=20.0).contains(&c.blur) {
        return Err(invalid("color.blur must be between 0.0 and 20.0"));
    }
    if !c.vignette.is_finite() || !(0.0..=1.0).contains(&c.vignette) {
        return Err(invalid("color.vignette must be between 0.0 and 1.0"));
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
    left_group: Option<Uuid>,
    right_group: Option<Uuid>,
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
    let left_tl_dur = at - existing.timeline_start;
    let left_src_delta = existing.source_offset_for_timeline_delta(left_tl_dur);
    if left_src_delta <= 0 || existing.source_in + left_src_delta >= existing.source_out {
        return Err(CoreError::InvalidTime(
            "split point produces a zero-length fragment".into(),
        ));
    }
    let mut right = existing.clone();
    right.id = Uuid::new_v4();
    right.timeline_start = at;
    right.source_in = existing.source_in + left_src_delta;
    right.link_group = right_group;
    let right_tl_dur = right.timeline_duration();
    right.fade_in_ticks = existing.fade_in_ticks.min(right_tl_dur);
    right.fade_out_ticks = existing.fade_out_ticks.min(right_tl_dur);

    let clip = require_clip_mut(project, &existing.id)?;
    clip.source_out = clip.source_in + left_src_delta;
    let left_new_tl = clip.timeline_duration();
    clip.fade_in_ticks = clip.fade_in_ticks.min(left_new_tl);
    clip.fade_out_ticks = clip.fade_out_ticks.min(left_new_tl);
    clip.link_group = left_group;
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
    link_remap: &mut std::collections::HashMap<(Uuid, bool), Uuid>,
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
        // Overlaps the range: keep fragments outside it, preserving link groups
        // per fragment side so video+audio pairs stay linked after dead-space cuts.
        if s < range.start {
            let mut left = clip.clone();
            let src_keep = clip.source_offset_for_timeline_delta(range.start - s);
            left.source_out = (left.source_in + src_keep).min(clip.source_out);
            left.fade_out_ticks = left.fade_out_ticks.min(left.timeline_duration());
            left.link_group = clip
                .link_group
                .map(|g| *link_remap.entry((g, false)).or_insert_with(Uuid::new_v4));
            if left.source_duration() > 0 {
                left.id = Uuid::new_v4();
                kept.push(left);
            }
        }
        if e > range.end {
            let mut right = clip.clone();
            let used_before = clip.source_offset_for_timeline_delta(range.end - s);
            right.source_in = (right.source_in + used_before).min(clip.source_out);
            if ripple {
                right.timeline_start = range.start;
                shifted += 1;
            } else {
                right.timeline_start = range.end;
            }
            right.fade_in_ticks = right.fade_in_ticks.min(right.timeline_duration());
            right.link_group = clip
                .link_group
                .map(|g| *link_remap.entry((g, true)).or_insert_with(Uuid::new_v4));
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
        // Left halves stay linked to each other; right halves stay linked to each other.
        let left_v = p.clip(&vid).unwrap();
        let left_a = p.clip(&aud).unwrap();
        assert!(left_v.link_group.is_some());
        assert_eq!(left_v.link_group, left_a.link_group);
        let right_clips: Vec<&Clip> = p.clips.iter().filter(|c| c.timeline_start == at).collect();
        assert_eq!(right_clips.len(), 2);
        assert!(right_clips[0].link_group.is_some());
        assert_eq!(right_clips[0].link_group, right_clips[1].link_group);
        assert_ne!(left_v.link_group, right_clips[0].link_group);
    }

    #[test]
    fn trim_and_remove_affect_linked_partner() {
        let (mut p, _asset, vid, aud) = project_with_clip();
        let two = crate::ticks::ticks_from_secs(2.0);
        let eight = crate::ticks::ticks_from_secs(8.0);
        apply_edit(
            &mut p,
            &Command::TrimClip {
                clip_id: vid,
                timeline_start: Some(two),
                source_in: Some(two),
                source_out: Some(eight),
            },
        )
        .unwrap();
        let a = p.clip(&aud).unwrap();
        assert_eq!(a.timeline_start, two);
        assert_eq!(a.source_in, two);
        assert_eq!(a.source_out, eight);

        apply_edit(&mut p, &Command::RemoveClip { clip_id: vid }).unwrap();
        assert!(p.clips.is_empty(), "linked partner should be removed too");
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

    #[test]
    fn set_zoom_allows_very_wide_zoom_out() {
        let (mut p, _, _, _) = project_with_clip();
        let out = apply_editor_state(
            &mut p,
            &Command::SetZoom {
                pixels_per_second: 0.005,
            },
        )
        .unwrap();
        assert!((out["pixels_per_second"].as_f64().unwrap() - 0.005).abs() < 1e-6);
        assert!((p.editor.zoom_pixels_per_second - 0.005).abs() < 1e-6);
        // Zero and negative remain rejected.
        assert!(apply_editor_state(
            &mut p,
            &Command::SetZoom {
                pixels_per_second: 0.0
            }
        )
        .is_err());
    }

    #[test]
    fn transcript_maps_through_cuts_and_populates_captions() {
        use crate::model::{Transcript, TranscriptSegment};
        let (mut p, asset, _vid, _aud) = project_with_clip();
        p.upsert_transcript(Transcript {
            asset_id: asset,
            model: "base.en".into(),
            language: "en".into(),
            segments: vec![
                TranscriptSegment {
                    start: 0,
                    end: crate::ticks::TICKS_PER_SEC * 2,
                    text: "Hello world".into(),
                },
                TranscriptSegment {
                    start: crate::ticks::TICKS_PER_SEC * 2,
                    end: crate::ticks::TICKS_PER_SEC * 5,
                    text: "Cut me out".into(),
                },
                TranscriptSegment {
                    start: crate::ticks::TICKS_PER_SEC * 6,
                    end: crate::ticks::TICKS_PER_SEC * 9,
                    text: "After the cut".into(),
                },
            ],
        });
        // Remove 2s..5s with ripple: 6s..9s should shift to 3s..6s on timeline!
        apply_edit(
            &mut p,
            &Command::RemoveRanges {
                start: crate::ticks::TICKS_PER_SEC * 2,
                end: crate::ticks::TICKS_PER_SEC * 5,
                ripple: Some(true),
                tracks: None,
            },
        )
        .unwrap();

        let mapped = p.timeline_transcript_segments(None);
        // Deduplicated across linked V1 + A1 clips -> exactly 2 segments.
        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[0].text, "Hello world");
        assert_eq!(mapped[0].timeline_start, 0);
        assert_eq!(mapped[0].timeline_end, crate::ticks::TICKS_PER_SEC * 2);
        assert_eq!(mapped[1].text, "After the cut");
        assert_eq!(mapped[1].timeline_start, crate::ticks::TICKS_PER_SEC * 3);
        assert_eq!(mapped[1].timeline_end, crate::ticks::TICKS_PER_SEC * 6);

        let res = apply_edit(
            &mut p,
            &Command::TranscriptToCaptions {
                asset_id: Some(asset),
                replace: Some(true),
            },
        )
        .unwrap();
        assert_eq!(res["added"], 2);
        assert_eq!(p.captions.len(), 2);
        assert_eq!(p.captions[1].text, "After the cut");
        assert_eq!(p.captions[1].start, crate::ticks::TICKS_PER_SEC * 3);
    }

    #[test]
    fn zoom_pan_motion_color_and_transform_update() {
        let (mut p, _asset, vid, _aud) = project_with_clip();
        let motion = ClipMotion::preset("zoom_in").unwrap().unwrap();
        apply_edit(
            &mut p,
            &Command::SetClipMotion {
                clip_id: vid,
                motion: Some(motion.clone()),
            },
        )
        .unwrap();
        assert_eq!(p.clip(&vid).unwrap().motion.as_ref(), Some(&motion));

        let color = ColorGrade {
            brightness: 0.1,
            contrast: 1.2,
            saturation: 0.0,
            gamma: 1.0,
            blur: 2.0,
            vignette: 0.5,
        };
        apply_edit(
            &mut p,
            &Command::SetClipColor {
                clip_id: vid,
                color,
            },
        )
        .unwrap();
        assert_eq!(p.clip(&vid).unwrap().color, color);

        let crop = CropRect {
            left: 0.1,
            right: 0.1,
            top: 0.05,
            bottom: 0.05,
        };
        apply_edit(
            &mut p,
            &Command::UpdateClip {
                clip_id: vid,
                volume: None,
                opacity: None,
                scale: Some(1.3),
                x_offset: Some(40.0),
                y_offset: Some(-20.0),
                fade_in_ticks: None,
                fade_out_ticks: None,
                rotation_deg: Some(90.0),
                flip_h: Some(true),
                flip_v: Some(false),
                crop: Some(crop),
            },
        )
        .unwrap();
        let c = p.clip(&vid).unwrap();
        assert_eq!(c.rotation_deg, 90.0);
        assert!(c.flip_h);
        assert_eq!(c.crop, crop);
        assert!(c.has_video_effects());

        // Invalid motion scale (< 1.0) is rejected.
        assert!(apply_edit(
            &mut p,
            &Command::SetClipMotion {
                clip_id: vid,
                motion: Some(ClipMotion {
                    start_scale: 0.5,
                    end_scale: 1.2,
                    ..motion
                }),
            },
        )
        .is_err());
    }

    #[test]
    fn speed_scales_linked_duration_and_splits_accurately() {
        let (mut p, _asset, vid, aud) = project_with_clip();
        // 10s clip at 2.0x -> 5s on timeline.
        let out = apply_edit(
            &mut p,
            &Command::SetClipSpeed {
                clip_id: vid,
                speed: 2.0,
            },
        )
        .unwrap();
        assert_eq!(out["updated_group"], true);
        assert_eq!(
            p.clip(&vid).unwrap().timeline_duration(),
            crate::ticks::TICKS_PER_SEC * 5
        );
        assert_eq!(
            p.clip(&aud).unwrap().timeline_duration(),
            crate::ticks::TICKS_PER_SEC * 5
        );

        // Split at 2s on timeline -> 4s in source media!
        let at = crate::ticks::TICKS_PER_SEC * 2;
        apply_edit(&mut p, &Command::SplitClip { clip_id: vid, at }).unwrap();
        let left_v = p.clip(&vid).unwrap();
        assert_eq!(left_v.source_out, crate::ticks::TICKS_PER_SEC * 4);
        assert_eq!(left_v.timeline_duration(), crate::ticks::TICKS_PER_SEC * 2);
        let right_v = p
            .clips
            .iter()
            .find(|c| c.track_id == p.tracks[0].id && c.timeline_start == at)
            .unwrap();
        assert_eq!(right_v.source_in, crate::ticks::TICKS_PER_SEC * 4);
        assert_eq!(right_v.speed, 2.0);
        assert_eq!(right_v.timeline_duration(), crate::ticks::TICKS_PER_SEC * 3);
    }

    #[test]
    fn duplicate_unlink_and_markers_with_ripple() {
        let (mut p, _asset, vid, aud) = project_with_clip();
        // Duplicate linked pair -> lands at 10s with a fresh link_group.
        let dup = apply_edit(
            &mut p,
            &Command::DuplicateClip {
                clip_id: vid,
                timeline_start: None,
            },
        )
        .unwrap();
        assert_eq!(dup["duplicated_count"], 2);
        assert_eq!(p.clips.len(), 4);
        let new_vid: Uuid = serde_json::from_value(dup["clip_id"].clone()).unwrap();
        assert_eq!(
            p.clip(&new_vid).unwrap().timeline_start,
            crate::ticks::TICKS_PER_SEC * 10
        );
        assert_ne!(
            p.clip(&new_vid).unwrap().link_group,
            p.clip(&vid).unwrap().link_group
        );

        // Unlink original video/audio pair.
        let unl = apply_edit(&mut p, &Command::UnlinkClip { clip_id: vid }).unwrap();
        assert_eq!(unl["unlinked"], true);
        assert!(p.clip(&vid).unwrap().link_group.is_none());
        assert!(p.clip(&aud).unwrap().link_group.is_none());

        // Markers: add two, ripple-delete over the first, verify second shifts.
        let m1 = apply_edit(
            &mut p,
            &Command::AddMarker {
                at: crate::ticks::TICKS_PER_SEC * 3,
                label: "Intro".into(),
                color: None,
            },
        )
        .unwrap();
        let m2 = apply_edit(
            &mut p,
            &Command::AddMarker {
                at: crate::ticks::TICKS_PER_SEC * 8,
                label: "Main point".into(),
                color: Some("#22C55E".into()),
            },
        )
        .unwrap();
        assert_eq!(p.markers.len(), 2);
        let m2_id: Uuid = serde_json::from_value(m2["marker_id"].clone()).unwrap();
        let _ = m1;

        apply_edit(
            &mut p,
            &Command::RemoveRanges {
                start: crate::ticks::TICKS_PER_SEC * 2,
                end: crate::ticks::TICKS_PER_SEC * 5,
                ripple: Some(true),
                tracks: None,
            },
        )
        .unwrap();
        // Marker at 3s was deleted; marker at 8s shifted left by 3s to 5s.
        assert_eq!(p.markers.len(), 1);
        assert_eq!(p.markers[0].id, m2_id);
        assert_eq!(p.markers[0].at, crate::ticks::TICKS_PER_SEC * 5);

        apply_edit(&mut p, &Command::RemoveMarker { marker_id: m2_id }).unwrap();
        assert!(p.markers.is_empty());
    }
}

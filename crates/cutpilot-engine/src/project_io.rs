//! Project document persistence. Atomic writes; small human-readable JSON.

use crate::error::EngineError;
use cutpilot_core::{Project, SCHEMA_VERSION};
use std::path::{Path, PathBuf};

/// The canonical project file inside a project directory.
pub fn project_file(project_dir: &Path) -> PathBuf {
    if project_dir.is_file() {
        project_dir.to_path_buf()
    } else {
        project_dir.join("project.json")
    }
}

/// Directory that owns `project.json` for a given project path.
pub fn project_dir(path: &Path) -> PathBuf {
    if path.is_file() {
        path.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        path.to_path_buf()
    }
}

/// Create a fresh project directory. Fails if one already exists.
pub fn create_new(
    dir: &Path,
    project: &Project,
) -> Result<PathBuf, EngineError> {
    std::fs::create_dir_all(dir)?;
    let file = project_file(dir);
    if file.exists() {
        return Err(EngineError::Validation(format!(
            "`{}` already exists; choose a new directory or use open_project",
            file.display()
        )));
    }
    write_atomic(&file, project)?;
    // Standard subdirectories.
    std::fs::create_dir_all(dir.join("hyperframes"))?;
    Ok(dir.to_path_buf())
}

/// Write the project atomically (tmp file + rename).
pub fn write_atomic(path: &Path, project: &Project) -> Result<(), EngineError> {
    let json = serde_json::to_string_pretty(project)
        .map_err(|e| EngineError::Validation(format!("serialize project: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Load and validate a project document.
pub fn load(path: &Path) -> Result<Project, EngineError> {
    let file = project_file(path);
    let raw = std::fs::read_to_string(&file).map_err(|e| {
        EngineError::Validation(format!(
            "cannot read `{}`: {e}",
            file.display()
        ))
    })?;
    let project: Project = serde_json::from_str(&raw).map_err(|e| {
        EngineError::Validation(format!(
            "`{}` is not a valid CutPilot project: {e}",
            file.display()
        ))
    })?;
    if project.schema_version != SCHEMA_VERSION {
        return Err(EngineError::Validation(format!(
            "project schema version {} is not supported (this build understands {SCHEMA_VERSION})",
            project.schema_version
        )));
    }
    project
        .settings
        .fps
        .validate()
        .map_err(EngineError::Validation)?;
    let dir = project_dir(path);
    validate_loaded_project(&project, &dir)?;
    Ok(project)
}

fn has_disallowed_protocol_scheme(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    if raw.is_empty() || raw.contains('\0') {
        return true;
    }
    if let Some(colon) = raw.find(':') {
        let is_windows_drive = colon == 1
            && raw.as_bytes()[0].is_ascii_alphabetic()
            && matches!(raw.as_bytes().get(2), Some(b'\\' | b'/'));
        let prefix = &raw[..colon];
        let has_path_sep = prefix.contains('/') || prefix.contains('\\');
        if !is_windows_drive && !has_path_sep {
            return true;
        }
    }
    false
}

/// Validate structural and security invariants of a deserialized `project.json`
/// before exposing it to the engine, FFmpeg, or HyperFrames.
fn validate_loaded_project(project: &Project, project_dir: &Path) -> Result<(), EngineError> {
    let w = project.settings.width;
    let h = project.settings.height;
    if !w.is_multiple_of(2) || !(2..=7680).contains(&w) {
        return Err(EngineError::Validation(format!(
            "invalid project width {w} (must be even, 2..=7680)"
        )));
    }
    if !h.is_multiple_of(2) || !(2..=7680).contains(&h) {
        return Err(EngineError::Validation(format!(
            "invalid project height {h} (must be even, 2..=7680)"
        )));
    }
    if !(8_000..=384_000).contains(&project.settings.audio_sample_rate) {
        return Err(EngineError::Validation(format!(
            "invalid audio_sample_rate {}",
            project.settings.audio_sample_rate
        )));
    }

    let font = project.caption_style.font.trim();
    if font.is_empty()
        || font.len() > 128
        || font
            .chars()
            .any(|c| c.is_control() || matches!(c, ',' | '{' | '}' | '\\'))
    {
        return Err(EngineError::Validation(
            "project caption_style.font contains invalid characters".into(),
        ));
    }
    cutpilot_core::commands::validate_hex_color(&project.caption_style.primary_color)
        .map_err(EngineError::Validation)?;
    cutpilot_core::commands::validate_hex_color(&project.caption_style.outline_color)
        .map_err(EngineError::Validation)?;

    let hf_root = cutpilot_media::hyperframes::compositions_dir(project_dir);
    let hf_root_canon = std::fs::canonicalize(&hf_root).ok();

    for asset in &project.assets {
        if has_disallowed_protocol_scheme(&asset.path) {
            return Err(EngineError::Validation(format!(
                "asset `{}` has disallowed or protocol URI path `{}`",
                asset.name,
                asset.path.display()
            )));
        }
        if let Some(gen) = &asset.generated {
            if has_disallowed_protocol_scheme(&gen.source_dir)
                || gen
                    .source_dir
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(EngineError::Validation(format!(
                    "HyperFrames asset `{}` has unsafe source_dir `{}`",
                    asset.name,
                    gen.source_dir.display()
                )));
            }
            if let (Some(root_c), Ok(src_c)) = (&hf_root_canon, std::fs::canonicalize(&gen.source_dir)) {
                if !src_c.starts_with(root_c) || src_c == *root_c {
                    return Err(EngineError::Validation(format!(
                        "HyperFrames asset `{}` source_dir escapes project directory",
                        asset.name
                    )));
                }
            } else if !gen.source_dir.starts_with(&hf_root) {
                return Err(EngineError::Validation(format!(
                    "HyperFrames asset `{}` source_dir `{}` is outside project hyperframes/",
                    asset.name,
                    gen.source_dir.display()
                )));
            }
        }
    }

    for clip in &project.clips {
        let floats = [
            clip.opacity,
            clip.scale,
            clip.x_offset,
            clip.y_offset,
            clip.volume,
            clip.speed,
            clip.rotation_deg,
            clip.crop.left,
            clip.crop.right,
            clip.crop.top,
            clip.crop.bottom,
            clip.color.brightness,
            clip.color.contrast,
            clip.color.saturation,
            clip.color.gamma,
            clip.color.blur,
            clip.color.vignette,
        ];
        if !floats.iter().all(|v| v.is_finite()) {
            return Err(EngineError::Validation(format!(
                "clip {} contains non-finite numeric parameters",
                clip.id
            )));
        }
        if !(-32_768.0..=32_768.0).contains(&clip.x_offset)
            || !(-32_768.0..=32_768.0).contains(&clip.y_offset)
        {
            return Err(EngineError::Validation(format!(
                "clip {} has out-of-range x_offset/y_offset",
                clip.id
            )));
        }
        if let Some(m) = &clip.motion {
            if ![
                m.start_scale,
                m.end_scale,
                m.start_pan_x,
                m.start_pan_y,
                m.end_pan_x,
                m.end_pan_y,
            ]
            .iter()
            .all(|v| v.is_finite())
            {
                return Err(EngineError::Validation(format!(
                    "clip {} motion contains non-finite values",
                    clip.id
                )));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutpilot_core::{Asset, AssetKind, Fps, GeneratedAsset, ProjectSettings};

    #[test]
    fn load_rejects_remote_asset_uri_and_escaped_hyperframes_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut p = Project::new("bad", ProjectSettings::default());
        p.assets.push(Asset {
            id: uuid::Uuid::new_v4(),
            kind: AssetKind::Video,
            name: "ssrf".into(),
            path: PathBuf::from("http://169.254.169.254/latest/meta-data/"),
            probe: None,
            generated: None,
        });
        write_atomic(&project_file(tmp.path()), &p).unwrap();
        assert!(load(tmp.path()).is_err());

        let mut p2 = Project::new("bad-hf", ProjectSettings::default());
        p2.assets.push(Asset {
            id: uuid::Uuid::new_v4(),
            kind: AssetKind::Video,
            name: "escape".into(),
            path: tmp.path().join("ok.mov"),
            probe: None,
            generated: Some(GeneratedAsset {
                source_dir: PathBuf::from("/tmp/outside-project"),
                format: "mov".into(),
                transparent: true,
                width: 1920,
                height: 1080,
                fps: Fps::from_int(30),
                duration_ticks: 3_600_000,
                html: "<div/>".into(),
            }),
        });
        write_atomic(&project_file(tmp.path()), &p2).unwrap();
        assert!(load(tmp.path()).is_err());
    }
}

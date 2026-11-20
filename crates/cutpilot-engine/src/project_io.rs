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
    Ok(project)
}

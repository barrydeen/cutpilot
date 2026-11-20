//! HyperFrames sidecar orchestration.
//!
//! CutPilot stores each generated composition as a managed directory inside
//! the project (`hyperframes/<name>/index.html`) and renders it with the
//! `hyperframes` CLI (`npx`). Transparent compositions render to ProRes 4444
//! MOV so overlays keep their alpha; opaque ones render to MP4 for the base
//! track. Rendering is asynchronous and cached — Chromium is never on the
//! preview or export frame path.

use crate::{process, MediaError};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Where CutPilot keeps managed compositions inside a project directory.
pub fn compositions_dir(project_dir: &Path) -> PathBuf {
    project_dir.join("hyperframes")
}

/// Slugify a composition name into a directory-safe id.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else if ch == ' ' {
            out.push('-');
        }
    }
    if out.is_empty() {
        out.push_str("clip");
    }
    out.truncate(64);
    out
}

/// Write a composition into its managed directory. Returns the directory.
pub fn scaffold_composition(
    project_dir: &Path,
    name: &str,
    html: &str,
) -> Result<PathBuf, MediaError> {
    let root = compositions_dir(project_dir);
    std::fs::create_dir_all(&root)?;
    let dir = root.join(slug(name));
    std::fs::create_dir_all(&dir)?;
    let validated = validate_composition_dir(project_dir, &dir)?;
    let index = validated.join("index.html");
    if let Ok(meta) = std::fs::symlink_metadata(&index) {
        if meta.file_type().is_symlink() {
            return Err(MediaError::BadData(
                "refusing to write HyperFrames index.html through a symlink".into(),
            ));
        }
    }
    std::fs::write(&index, html)?;
    Ok(validated)
}

/// Verify that `candidate` resolves strictly inside `<project_dir>/hyperframes/`
/// and that no untrusted `node_modules` directory or `package.json` exists
/// between `candidate` and `project_dir` (which could hijack `npx` execution).
pub fn validate_composition_dir(
    project_dir: &Path,
    candidate: &Path,
) -> Result<PathBuf, MediaError> {
    let root = compositions_dir(project_dir);
    let root_canon = std::fs::canonicalize(&root).map_err(|e| {
        MediaError::BadData(format!(
            "cannot resolve HyperFrames directory `{}`: {e}",
            root.display()
        ))
    })?;
    let cand_canon = std::fs::canonicalize(candidate).map_err(|e| {
        MediaError::BadData(format!(
            "cannot resolve composition directory `{}`: {e}",
            candidate.display()
        ))
    })?;
    if !cand_canon.starts_with(&root_canon) || cand_canon == root_canon {
        return Err(MediaError::BadData(format!(
            "HyperFrames source_dir `{}` escapes project directory `{}`",
            candidate.display(),
            root.display()
        )));
    }
    ensure_no_local_node_hijack(&cand_canon, project_dir)?;
    Ok(cand_canon)
}

/// Reject any `node_modules` or `package.json` inside the project hierarchy
/// above `dir` so an untrusted project directory cannot plant a malicious
/// `node_modules/.bin/hyperframes` executable.
fn ensure_no_local_node_hijack(dir: &Path, project_dir: &Path) -> Result<(), MediaError> {
    let project_canon = std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
    let mut cur = Some(dir);
    while let Some(p) = cur {
        if p.join("node_modules").exists() || p.join("package.json").exists() {
            return Err(MediaError::BadData(format!(
                "refusing to run HyperFrames: untrusted `node_modules` or `package.json` found in `{}`",
                p.display()
            )));
        }
        if p == project_canon {
            break;
        }
        cur = p.parent();
    }
    Ok(())
}

/// Output file for a composition render.
pub fn output_file(dir: &Path, transparent: bool) -> PathBuf {
    if transparent {
        dir.join("render.mov")
    } else {
        dir.join("render.mp4")
    }
}

/// Build the `npx hyperframes render` argv for a composition.
///
/// Uses `--no` (no automatic unpinned npm downloads) unless
/// `CUTPILOT_HYPERFRAMES_ALLOW_NPX_INSTALL=1` is explicitly set.
pub fn render_args(dir: &Path, transparent: bool) -> Vec<String> {
    let out = output_file(dir, transparent);
    let allow_install = std::env::var("CUTPILOT_HYPERFRAMES_ALLOW_NPX_INSTALL")
        .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(false);
    let mode_flag = if allow_install { "--yes" } else { "--no" };
    let mut args: Vec<String> = vec![mode_flag.into(), "hyperframes".into(), "render".into()];
    args.push("--output".into());
    args.push(out.display().to_string());
    if transparent {
        args.push("--format".into());
        args.push("mov".into());
    } else {
        args.push("--format".into());
        args.push("mp4".into());
    }
    args
}

/// Build direct `hyperframes render` argv when invoked without `npx`.
fn direct_render_args(dir: &Path, transparent: bool) -> Vec<String> {
    let out = output_file(dir, transparent);
    vec![
        "render".into(),
        "--output".into(),
        out.display().to_string(),
        "--format".into(),
        if transparent { "mov" } else { "mp4" }.into(),
    ]
}

fn which_bin(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file()))
        .unwrap_or(false)
}

fn direct_hyperframes_bin() -> Option<String> {
    if let Ok(custom) = std::env::var("CUTPILOT_HYPERFRAMES_BIN") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() && Path::new(trimmed).is_file() {
            return Some(trimmed.to_string());
        }
    }
    if which_bin("hyperframes") {
        return Some("hyperframes".into());
    }
    None
}

/// Render a composition synchronously with cancellation support.
pub fn render(
    dir: &Path,
    transparent: bool,
    cancel: Arc<AtomicBool>,
) -> Result<PathBuf, MediaError> {
    if let Some(bin) = direct_hyperframes_bin() {
        let args = direct_render_args(dir, transparent);
        process::run_in_dir(&bin, &args, dir, cancel)?;
    } else {
        // Guard against project-local `node_modules/.bin/hyperframes` hijacking
        // even when `render` is called directly.
        if let Some(parent) = dir.parent().and_then(Path::parent) {
            ensure_no_local_node_hijack(dir, parent)?;
        }
        let args = render_args(dir, transparent);
        process::run_in_dir("npx", &args, dir, cancel)?;
    }
    let out = output_file(dir, transparent);
    if !out.is_file() {
        return Err(MediaError::Ffmpeg {
            message: format!(
                "hyperframes reported success but `{}` is missing",
                out.display()
            ),
        });
    }
    Ok(out)
}

/// Check whether the sidecar can run on this machine.
pub fn sidecar_available() -> bool {
    if direct_hyperframes_bin().is_some() {
        return true;
    }
    std::process::Command::new("npx")
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_sanitizes() {
        assert_eq!(slug("My Cool Intro!"), "my-cool-intro");
        assert_eq!(slug("!!!"), "clip");
        assert_eq!(slug("a_b-1"), "a_b-1");
    }

    #[test]
    fn render_args_target_prores_for_transparent() {
        let dir = Path::new("/tmp/comp");
        let transparent = render_args(dir, true);
        assert!(transparent.join(" ").contains("--format mov"));
        assert!(transparent.join(" ").contains("render.mov"));
        assert!(
            !transparent.iter().any(|a| a == "--yes"),
            "npx must not auto-install unpinned packages by default: {transparent:?}"
        );
        let opaque = render_args(dir, false);
        assert!(opaque.join(" ").contains("--format mp4"));
        assert!(opaque.join(" ").contains("render.mp4"));
    }

    #[test]
    fn scaffold_writes_index() {
        let tmp = std::env::temp_dir().join(format!("op-hf-{}", uuid::Uuid::new_v4()));
        let dir = scaffold_composition(&tmp, "Intro Screen", "<div>hi</div>").unwrap();
        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert_eq!(html, "<div>hi</div>");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_composition_dir_rejects_escape_and_local_node_modules() {
        let tmp = std::env::temp_dir().join(format!("op-hf-sec-{}", uuid::Uuid::new_v4()));
        let outside = std::env::temp_dir().join(format!("op-hf-out-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        let dir = scaffold_composition(&tmp, "safe", "<div>ok</div>").unwrap();

        // Outside directory must be rejected.
        assert!(validate_composition_dir(&tmp, &outside).is_err());

        // Planting node_modules inside the project must be rejected to prevent npx hijacking.
        std::fs::create_dir_all(tmp.join("node_modules/.bin")).unwrap();
        assert!(validate_composition_dir(&tmp, &dir).is_err());

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&outside);
    }
}

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
pub fn scaffold_composition(project_dir: &Path, name: &str, html: &str) -> PathBuf {
    let dir = compositions_dir(project_dir).join(slug(name));
    std::fs::create_dir_all(&dir).expect("project directory must be writable");
    std::fs::write(dir.join("index.html"), html).expect("write index.html");
    dir
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
pub fn render_args(dir: &Path, transparent: bool) -> Vec<String> {
    let out = output_file(dir, transparent);
    let mut args: Vec<String> = vec!["--yes".into(), "hyperframes".into(), "render".into()];
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

/// Render a composition synchronously with cancellation support.
pub fn render(
    dir: &Path,
    transparent: bool,
    cancel: Arc<AtomicBool>,
) -> Result<PathBuf, MediaError> {
    let args = render_args(dir, transparent);
    process::run_in_dir("npx", &args, dir, cancel)?;
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
        let opaque = render_args(dir, false);
        assert!(opaque.join(" ").contains("--format mp4"));
        assert!(opaque.join(" ").contains("render.mp4"));
    }

    #[test]
    fn scaffold_writes_index() {
        let tmp = std::env::temp_dir().join(format!("op-hf-{}", uuid::Uuid::new_v4()));
        let dir = scaffold_composition(&tmp, "Intro Screen", "<div>hi</div>");
        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert_eq!(html, "<div>hi</div>");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

//! System file picker for importing media.
//!
//! Primary backend: rfd's XDG Desktop Portal dialogs — the chooser is hosted
//! by the desktop itself (e.g. xdg-desktop-portal-gtk), so the CutPilot
//! binary never links GTK and the whole thing degrades gracefully. On setups
//! without a FileChooser portal backend we fall back to `zenity`/`kdialog`
//! subprocesses, and the Media panel's paste-a-path field is the last resort.
//!
//! `pick_files_blocking` must run off the UI thread (it blocks on the
//! portal's nested request).

use std::path::PathBuf;
use std::process::Command;

pub enum PickerOutcome {
    Files(Vec<PathBuf>),
    Cancelled,
    Failed(String),
}

pub enum FolderPickerOutcome {
    Folder(PathBuf),
    Cancelled,
    Failed(String),
}

/// Open a blocking multi-file picker. Call from a worker thread.
pub fn pick_files_blocking(title: &str) -> PickerOutcome {
    if portal_filechooser_available() {
        return pick_via_portal(title);
    }
    if which("zenity") {
        return pick_zenity(title);
    }
    if which("kdialog") {
        return pick_kdialog(title);
    }
    PickerOutcome::Failed(
        "no file picker available — install xdg-desktop-portal with a \
         FileChooser backend (e.g. xdg-desktop-portal-gtk) or zenity; you can \
         also paste a media path in the Media panel"
            .into(),
    )
}

/// Open a blocking single-folder picker (for New Project location / Open Project / Save As).
/// Call from a worker thread.
pub fn pick_folder_blocking(title: &str) -> FolderPickerOutcome {
    if portal_filechooser_available() {
        return pick_folder_via_portal(title);
    }
    if which("zenity") {
        return pick_folder_zenity(title);
    }
    if which("kdialog") {
        return pick_folder_kdialog(title);
    }
    FolderPickerOutcome::Failed(
        "no folder picker available — install xdg-desktop-portal with a \
         FileChooser backend (e.g. xdg-desktop-portal-gtk) or zenity"
            .into(),
    )
}

/// Turn a user-typed project name into a safe directory stem (strips path separators).
pub fn sanitize_project_folder_stem(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            _ => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == '.' || c == ' ' || c == '-');
    if trimmed.is_empty() {
        "CutPilot Project".into()
    } else {
        trimmed.to_string()
    }
}

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "mkv", "webm", "avi", "m4v", "mpg", "mpeg", "ts", "mts", "m2ts", "3gp", "wmv",
];
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", "mp3", "aac", "flac", "ogg", "m4a", "opus", "aiff", "aif", "wma",
];
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "bmp", "gif", "tiff",
];

fn all_media_extensions() -> Vec<&'static str> {
    let mut all = Vec::with_capacity(
        VIDEO_EXTENSIONS.len() + AUDIO_EXTENSIONS.len() + IMAGE_EXTENSIONS.len(),
    );
    all.extend_from_slice(VIDEO_EXTENSIONS);
    all.extend_from_slice(AUDIO_EXTENSIONS);
    all.extend_from_slice(IMAGE_EXTENSIONS);
    all
}

fn zenity_filter_arg(label: &str, exts: &[&str]) -> String {
    let mut patterns = Vec::with_capacity(exts.len() * 2);
    for ext in exts {
        patterns.push(format!("*.{ext}"));
        patterns.push(format!("*.{}", ext.to_ascii_uppercase()));
    }
    format!("{label} | {}", patterns.join(" "))
}

fn pick_via_portal(title: &str) -> PickerOutcome {
    let all_media = all_media_extensions();
    let dialog = rfd::AsyncFileDialog::new()
        .set_title(title)
        .add_filter("All Media (Video, Audio, Images)", &all_media)
        .add_filter("Audio (WAV, MP3, FLAC…)", AUDIO_EXTENSIONS)
        .add_filter("Video", VIDEO_EXTENSIONS)
        .add_filter("Images", IMAGE_EXTENSIONS)
        .add_filter("All files", &["*"]);
    match pollster::block_on(dialog.pick_files()) {
        Some(handles) => {
            let paths: Vec<PathBuf> = handles
                .iter()
                .map(|h| h.path().to_path_buf())
                .collect();
            if paths.is_empty() {
                PickerOutcome::Cancelled
            } else {
                PickerOutcome::Files(paths)
            }
        }
        None => PickerOutcome::Cancelled,
    }
}

fn pick_folder_via_portal(title: &str) -> FolderPickerOutcome {
    let dialog = rfd::AsyncFileDialog::new().set_title(title);
    match pollster::block_on(dialog.pick_folder()) {
        Some(handle) => FolderPickerOutcome::Folder(handle.path().to_path_buf()),
        None => FolderPickerOutcome::Cancelled,
    }
}

/// True when an installed portal backend advertises the FileChooser
/// interface. Preflight keeps "no backend installed" from masquerading as
/// "user cancelled" on portal-less setups.
fn portal_filechooser_available() -> bool {
    let mut dirs: Vec<PathBuf> = std::env::var("XDG_DATA_DIRS")
        .ok()
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    if dirs.is_empty() {
        dirs.push(PathBuf::from("/usr/local/share"));
        dirs.push(PathBuf::from("/usr/share"));
    }
    for dir in dirs {
        let portals = dir.join("xdg-desktop-portal/portals");
        if let Ok(entries) = std::fs::read_dir(&portals) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "portal") {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        if text.contains("org.freedesktop.impl.portal.FileChooser") {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

fn pick_zenity(title: &str) -> PickerOutcome {
    let all_media = all_media_extensions();
    let output = Command::new("zenity")
        .args(["--file-selection", "--multiple", "--title", title, "--separator", "\n"])
        .arg("--file-filter")
        .arg(zenity_filter_arg("All Media (Video, Audio, Images)", &all_media))
        .arg("--file-filter")
        .arg(zenity_filter_arg("Audio (WAV, MP3, FLAC)", AUDIO_EXTENSIONS))
        .arg("--file-filter")
        .arg(zenity_filter_arg("Video", VIDEO_EXTENSIONS))
        .arg("--file-filter")
        .arg(zenity_filter_arg("Images", IMAGE_EXTENSIONS))
        .arg("--file-filter")
        .arg("All files | *")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let paths: Vec<PathBuf> = text.lines().filter_map(uri_to_path).collect();
            if paths.is_empty() {
                PickerOutcome::Cancelled
            } else {
                PickerOutcome::Files(paths)
            }
        }
        // zenity exits 1 when the user cancels.
        Ok(o) if o.status.code() == Some(1) => PickerOutcome::Cancelled,
        Ok(o) => PickerOutcome::Failed(format!(
            "zenity failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => PickerOutcome::Failed(format!("could not run zenity: {e}")),
    }
}

fn pick_kdialog(title: &str) -> PickerOutcome {
    let output = Command::new("kdialog")
        .args(["--title", title, "--getopenfilenames"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let paths: Vec<PathBuf> = text.split_whitespace().filter_map(uri_to_path).collect();
            if paths.is_empty() {
                PickerOutcome::Cancelled
            } else {
                PickerOutcome::Files(paths)
            }
        }
        Ok(_) => PickerOutcome::Cancelled,
        Err(e) => PickerOutcome::Failed(format!("could not run kdialog: {e}")),
    }
}

fn pick_folder_zenity(title: &str) -> FolderPickerOutcome {
    let output = Command::new("zenity")
        .args(["--file-selection", "--directory", "--title", title])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            match text.lines().find_map(uri_to_path) {
                Some(path) => FolderPickerOutcome::Folder(path),
                None => FolderPickerOutcome::Cancelled,
            }
        }
        Ok(o) if o.status.code() == Some(1) => FolderPickerOutcome::Cancelled,
        Ok(o) => FolderPickerOutcome::Failed(format!(
            "zenity failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => FolderPickerOutcome::Failed(format!("could not run zenity: {e}")),
    }
}

fn pick_folder_kdialog(title: &str) -> FolderPickerOutcome {
    let output = Command::new("kdialog")
        .args(["--title", title, "--getexistingdirectory"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            match text.lines().find_map(uri_to_path) {
                Some(path) => FolderPickerOutcome::Folder(path),
                None => FolderPickerOutcome::Cancelled,
            }
        }
        Ok(_) => FolderPickerOutcome::Cancelled,
        Err(e) => FolderPickerOutcome::Failed(format!("could not run kdialog: {e}")),
    }
}

/// `file:///path%20name` -> `PathBuf`, percent-decoded.
fn uri_to_path(s: &str) -> Option<PathBuf> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let raw = s.strip_prefix("file://").unwrap_or(s);
    Some(PathBuf::from(percent_decode(raw)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => 0xff,
            };
            let hi = hex(bytes[i + 1]);
            let lo = hex(bytes[i + 2]);
            if hi != 0xff && lo != 0xff {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn which(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_uris() {
        assert_eq!(
            uri_to_path("file:///home/u/my%20video.mp4").unwrap(),
            PathBuf::from("/home/u/my video.mp4")
        );
        assert_eq!(
            uri_to_path("file:///mnt/a+b/c%2Fd.mp4").unwrap(),
            PathBuf::from("/mnt/a+b/c/d.mp4")
        );
        // Bare paths and junk percent sequences pass through.
        assert_eq!(
            uri_to_path("/tmp/plain.mp4").unwrap(),
            PathBuf::from("/tmp/plain.mp4")
        );
        assert_eq!(percent_decode("100% sure"), "100% sure");
        assert!(uri_to_path("").is_none());
    }

    #[test]
    fn default_media_filter_includes_wav() {
        let all = all_media_extensions();
        assert!(all.contains(&"wav"));
        assert!(AUDIO_EXTENSIONS.contains(&"wav"));
        let zen = zenity_filter_arg("All Media", &all);
        assert!(zen.contains("*.wav"));
        assert!(zen.contains("*.WAV"));
    }

    #[test]
    fn sanitizes_project_folder_names() {
        assert_eq!(sanitize_project_folder_stem("  My Vlog 01  "), "My Vlog 01");
        assert_eq!(sanitize_project_folder_stem("a/b:c*d"), "a-b-c-d");
        assert_eq!(sanitize_project_folder_stem("..."), "CutPilot Project");
    }
}

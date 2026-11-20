//! System file picker for importing media.
//!
//! Primary backend: rfd's XDG Desktop Portal dialogs — the chooser is hosted
//! by the desktop itself (e.g. xdg-desktop-portal-gtk), so the OpenPrem
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

fn pick_via_portal(title: &str) -> PickerOutcome {
    let dialog = rfd::AsyncFileDialog::new()
        .set_title(title)
        .add_filter(
            "Video",
            &[
                "mp4", "mov", "mkv", "webm", "avi", "m4v", "mpg", "mpeg", "ts", "mts", "m2ts",
                "3gp", "wmv",
            ],
        )
        .add_filter("Audio", &["mp3", "wav", "aac", "flac", "ogg", "m4a", "opus", "aiff"])
        .add_filter("Images", &["jpg", "jpeg", "png", "webp", "bmp", "gif", "tiff"])
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
    let output = Command::new("zenity")
        .args(["--file-selection", "--multiple", "--title", title, "--separator", "\n"])
        .arg("--file-filter")
        .arg("Video | *.mp4 *.mov *.mkv *.webm *.avi *.m4v *.mpg *.mpeg *.ts *.mts *.m2ts *.3gp *.wmv")
        .arg("--file-filter")
        .arg("Audio | *.mp3 *.wav *.aac *.flac *.ogg *.m4a *.opus *.aiff")
        .arg("--file-filter")
        .arg("Images | *.jpg *.jpeg *.png *.webp *.bmp *.gif *.tiff")
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
}

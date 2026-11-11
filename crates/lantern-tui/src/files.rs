//! Reading directories and files for display.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    Image,
    File,
}

pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Visible children of a directory. Only counted for the current column; None if unreadable.
    pub count: Option<usize>,
}

const IMAGE_EXTENSIONS: &[&str] = &[
    "avif", "bmp", "gif", "heic", "heif", "jpeg", "jpg", "png", "svg", "tif", "tiff", "webp",
];

/// Directories first, then files, each alphabetically. An unreadable directory lists as empty.
pub fn list(dir: &Path, show_hidden: bool, with_counts: bool) -> Vec<Entry> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                return None;
            }
            let path = e.path();
            // Follow symlinks so a link to a folder behaves like the folder.
            let meta = fs::metadata(&path).or_else(|_| e.metadata()).ok()?;
            let kind = if meta.is_dir() {
                Kind::Dir
            } else if is_image(&path) {
                Kind::Image
            } else {
                Kind::File
            };
            let count = if with_counts && kind == Kind::Dir {
                count_visible(&path, show_hidden)
            } else {
                None
            };
            Some(Entry {
                path,
                name,
                kind,
                size: meta.len(),
                modified: meta.modified().ok(),
                count,
            })
        })
        .collect();
    entries.sort_by_cached_key(|e| (e.kind != Kind::Dir, e.name.to_lowercase()));
    entries
}

fn count_visible(dir: &Path, show_hidden: bool) -> Option<usize> {
    let read = fs::read_dir(dir).ok()?;
    Some(
        read.filter_map(Result::ok)
            .filter(|e| show_hidden || !e.file_name().to_string_lossy().starts_with('.'))
            .count(),
    )
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// The first lines of a text file, or None if it looks binary.
pub fn text_head(path: &Path, max_lines: usize) -> Option<Vec<String>> {
    let mut buf = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(16 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.contains(&0) {
        return None;
    }
    let text = match std::str::from_utf8(&buf) {
        Ok(text) => text,
        // The read may have cut a multi-byte character in half.
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&buf[..e.valid_up_to()]).ok()?,
        Err(_) => return None,
    };
    Some(
        text.lines()
            .take(max_lines)
            .map(|l| l.replace('\t', "    "))
            .collect(),
    )
}

/// The last path component, or the whole path for a root like `/` or `C:\`.
pub fn display_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes}B"),
        _ if size < 10.0 => format!("{size:.1}{}", UNITS[unit]),
        _ => format!("{size:.0}{}", UNITS[unit]),
    }
}

pub fn age(time: SystemTime) -> String {
    const DAY: u64 = 86_400;
    let secs = SystemTime::now()
        .duration_since(time)
        .map_or(0, |d| d.as_secs());
    let (n, unit) = match secs {
        s if s < 60 => return "just now".into(),
        s if s < 3600 => (s / 60, "minute"),
        s if s < DAY => (s / 3600, "hour"),
        s if s < 30 * DAY => (s / DAY, "day"),
        s if s < 365 * DAY => (s / (30 * DAY), "month"),
        s => (s / (365 * DAY), "year"),
    };
    format!("{} ago", plural(n as usize, unit))
}

pub fn plural(n: usize, unit: &str) -> String {
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(50 * 1024 * 1024), "50M");
    }

    #[test]
    fn ages() {
        let now = SystemTime::now();
        assert_eq!(age(now), "just now");
        assert_eq!(age(now - Duration::from_secs(3600)), "1 hour ago");
        assert_eq!(age(now - Duration::from_secs(3 * 86_400)), "3 days ago");
    }

    #[test]
    fn text_head_skips_binary() {
        let dir = std::env::temp_dir();
        let text = dir.join(format!("lantern-text-{}", std::process::id()));
        let binary = dir.join(format!("lantern-bin-{}", std::process::id()));
        fs::write(&text, "one\ntwo\nthree\n").unwrap();
        fs::write(&binary, [0u8, 1, 2]).unwrap();
        assert_eq!(text_head(&text, 2).unwrap(), ["one", "two"]);
        assert!(text_head(&binary, 2).is_none());
        fs::remove_file(text).unwrap();
        fs::remove_file(binary).unwrap();
    }
}

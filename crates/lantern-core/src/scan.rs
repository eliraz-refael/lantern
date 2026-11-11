//! Walking lit folders to find the files an agent can describe.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// How much of a text file is checked for secrets and sent to the agent.
pub const TEXT_LIMIT: usize = 64 * 1024;

/// Larger images are left out rather than decoded.
const IMAGE_LIMIT: u64 = 50 * 1024 * 1024;

/// Folders that are never worth describing, besides hidden ones: installed dependencies.
const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    "bower_components",
    "vendor",
    "venv",
    "site-packages",
    "__pycache__",
];

/// Generated files that say nothing about what a folder is for.
const SKIPPED_FILES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "Cargo.lock",
    "poetry.lock",
    "Pipfile.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "flake.lock",
];

/// Whether a file is generated or minified rather than written by someone.
fn is_generated(name: &str) -> bool {
    SKIPPED_FILES.contains(&name)
        || name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || name.ends_with(".map")
}

const IMAGE_EXTENSIONS: &[&str] = &["bmp", "gif", "jpeg", "jpg", "png", "tif", "tiff", "webp"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Image,
    Text,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Text => "text",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "image" => Some(Self::Image),
            "text" => Some(Self::Text),
            _ => None,
        }
    }
}

/// A regular file seen while walking, before its content is looked at.
pub(crate) struct Seen {
    pub path: PathBuf,
    pub size: u64,
    pub modified: i64,
}

/// Collects the visible regular files under `root`. Symlinks are skipped so a walk never leaves the
/// folder, and `other_roots` are left to their own walk. Returns false if `root` can't be read.
pub(crate) fn walk(root: &Path, other_roots: &[PathBuf], out: &mut Vec<Seen>) -> bool {
    let Ok(read) = fs::read_dir(root) else {
        return false;
    };
    for entry in read.filter_map(Result::ok) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_ref()) && !other_roots.contains(&path) {
                walk(&path, other_roots, out);
            }
        } else if file_type.is_file()
            && !is_generated(&name)
            && let Ok(meta) = entry.metadata()
        {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            out.push(Seen {
                path,
                size: meta.len(),
                modified,
            });
        }
    }
    true
}

/// What kind of file this is, with the text an agent would see for text files, or None if an
/// agent can't read it yet (PDFs, archives, video...).
pub(crate) fn classify(path: &Path, size: u64) -> io::Result<Option<(FileKind, Option<String>)>> {
    let is_image = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    if is_image {
        return Ok((size <= IMAGE_LIMIT).then_some((FileKind::Image, None)));
    }
    Ok(read_text(path)?.map(|text| (FileKind::Text, Some(text))))
}

/// The first [`TEXT_LIMIT`] bytes of a file as text, or None if it looks binary.
pub fn read_text(path: &Path) -> io::Result<Option<String>> {
    let mut bytes = Vec::with_capacity(TEXT_LIMIT.min(8192));
    File::open(path)?
        .take(TEXT_LIMIT as u64)
        .read_to_end(&mut bytes)?;
    if bytes.contains(&0) {
        return Ok(None);
    }
    match std::str::from_utf8(&bytes) {
        Ok(text) => Ok(Some(text.to_owned())),
        // The limit can cut a character in half; anything else isn't UTF-8 text.
        Err(e) if e.error_len().is_none() => {
            bytes.truncate(e.valid_up_to());
            Ok(Some(String::from_utf8(bytes).expect("checked above")))
        }
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::tempdir;

    #[test]
    fn walk_skips_hidden_and_other_roots() {
        let dir = tempdir("walk");
        fs::create_dir_all(dir.join("a/.git")).unwrap();
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::write(dir.join("a/one.txt"), "1").unwrap();
        fs::write(dir.join("a/package-lock.json"), "{}").unwrap();
        fs::create_dir_all(dir.join("a/node_modules/x")).unwrap();
        fs::write(dir.join("a/node_modules/x/index.js"), "x").unwrap();
        fs::write(dir.join("a/.git/config"), "x").unwrap();
        fs::write(dir.join(".hidden"), "x").unwrap();
        fs::write(dir.join("b/two.txt"), "2").unwrap();

        let mut out = Vec::new();
        walk(&dir, &[dir.join("b")], &mut out);
        let names: Vec<_> = out
            .iter()
            .map(|s| s.path.strip_prefix(&dir).unwrap())
            .collect();
        assert_eq!(names, vec![Path::new("a").join("one.txt")]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn text_is_cut_on_a_character_boundary() {
        let dir = tempdir("text");
        let path = dir.join("long.txt");
        // "é" is two bytes, so the limit falls inside one.
        fs::write(&path, format!("a{}", "é".repeat(TEXT_LIMIT))).unwrap();
        let text = read_text(&path).unwrap().unwrap();
        assert_eq!(text.len(), TEXT_LIMIT - 1);
        fs::write(&path, b"\x00\x01binary").unwrap();
        assert_eq!(read_text(&path).unwrap(), None);
        fs::remove_dir_all(dir).unwrap();
    }
}

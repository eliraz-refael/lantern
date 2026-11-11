//! Lantern's local index: which folders are lit, and what the agent wrote about each file.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("path is not valid UTF-8: {}", .0.display())]
    NonUtf8Path(PathBuf),
}

pub type Result<T> = std::result::Result<T, Error>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS lit_folders (
    path    TEXT PRIMARY KEY,
    lit_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS files (
    path       TEXT PRIMARY KEY,
    hash       TEXT NOT NULL,
    title      TEXT,
    summary    TEXT,
    tags       TEXT,
    sensitive  TEXT
);
";

pub struct Index {
    conn: Connection,
}

impl Index {
    /// Where the daemon and TUI share the index: ~/Library/Application Support/lantern on macOS,
    /// %APPDATA%\lantern on Windows.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "lantern").map(|d| d.data_dir().join("index.sqlite"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Marks a folder as lit so the agent may read it. Lighting it again is a no-op.
    pub fn light(&self, folder: &Path) -> Result<()> {
        let lit_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        self.conn.execute(
            "INSERT OR IGNORE INTO lit_folders (path, lit_at) VALUES (?1, ?2)",
            params![path_str(folder)?, lit_at],
        )?;
        Ok(())
    }

    pub fn unlight(&self, folder: &Path) -> Result<()> {
        self.conn.execute(
            "DELETE FROM lit_folders WHERE path = ?1",
            params![path_str(folder)?],
        )?;
        Ok(())
    }

    /// Number of files the agent has written an entry for.
    pub fn file_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        Ok(n as u64)
    }

    pub fn lit_folders(&self) -> Result<Vec<PathBuf>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM lit_folders ORDER BY path")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows
            .map(|r| r.map(PathBuf::from))
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// Content hash used to skip re-reading files that haven't changed.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::NonUtf8Path(path.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lighting_is_idempotent() {
        let index = Index::open_in_memory().unwrap();
        let folder = std::env::temp_dir().join("photos");
        index.light(&folder).unwrap();
        index.light(&folder).unwrap();
        assert_eq!(index.lit_folders().unwrap(), vec![folder.clone()]);
        index.unlight(&folder).unwrap();
        assert!(index.lit_folders().unwrap().is_empty());
    }

    #[test]
    fn hash_changes_with_content() {
        let path = std::env::temp_dir().join(format!("lantern-hash-{}", std::process::id()));
        std::fs::write(&path, "a").unwrap();
        let first = hash_file(&path).unwrap();
        std::fs::write(&path, "b").unwrap();
        let second = hash_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_ne!(first, second);
    }
}

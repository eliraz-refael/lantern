//! Lantern's local index: which folders are lit, and what the agent wrote about each file.

pub mod image;
pub mod scan;
pub mod sensitive;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

pub use scan::FileKind;

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

/// Bumped whenever the tables change; see [`migrate`].
const VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS lit_folders (
    path    TEXT PRIMARY KEY,
    lit_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS files (
    path        TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    modified    INTEGER NOT NULL,
    hash        TEXT NOT NULL,
    state       TEXT NOT NULL,
    note        TEXT,
    attempts    INTEGER NOT NULL DEFAULT 0,
    title       TEXT,
    summary     TEXT,
    tags        TEXT,
    agent       TEXT,
    indexed_at  INTEGER
);
CREATE INDEX IF NOT EXISTS files_by_state ON files (state, path);
CREATE TABLE IF NOT EXISTS agent_status (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    agent      TEXT NOT NULL,
    state      TEXT NOT NULL,
    detail     TEXT,
    heartbeat  INTEGER NOT NULL
);
";

/// A file is given up on after the agent is asked this many times without describing it.
pub const MAX_ATTEMPTS: i64 = 3;

/// Where a file is in indexing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for the agent.
    Pending,
    /// The agent described it.
    Indexed,
    /// Holds a secret, so it's never sent to the agent.
    Sensitive,
    /// The agent was asked [`MAX_ATTEMPTS`] times and didn't describe it.
    Failed,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Indexed => "indexed",
            Self::Sensitive => "sensitive",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "indexed" => Self::Indexed,
            "sensitive" => Self::Sensitive,
            _ => Self::Failed,
        }
    }
}

/// What the agent wrote about a file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Description {
    pub title: String,
    pub summary: String,
    pub tags: Vec<String>,
}

/// Everything the index knows about one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub kind: FileKind,
    pub state: State,
    /// Why it's sensitive or failed.
    pub note: Option<String>,
    pub description: Option<Description>,
    pub agent: Option<String>,
}

/// File counts by state, for a folder or the whole index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub pending: u64,
    pub indexed: u64,
    pub sensitive: u64,
    pub failed: u64,
}

/// What the daemon's agent is doing, as last reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentStatus {
    pub agent: String,
    pub state: AgentState,
    pub detail: Option<String>,
    /// When the daemon last wrote this. A stale heartbeat means the daemon isn't running.
    pub heartbeat: SystemTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    Error,
}

impl AgentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Error => "error",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "idle" => Self::Idle,
            "working" => Self::Working,
            _ => Self::Error,
        }
    }
}

/// What a scan changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
}

pub struct Index {
    conn: Connection,
}

impl Index {
    /// Where the daemon and TUI share the index: `$LANTERN_DB` if set, otherwise
    /// ~/Library/Application Support/lantern on macOS, %APPDATA%\lantern on Windows.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("LANTERN_DB") {
            return Some(path.into());
        }
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

    fn init(mut conn: Connection) -> Result<Self> {
        // The daemon, the TUI and the agent's MCP server all write; wait for each other's locks.
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// Marks a folder as lit so the agent may read it. Lighting it again is a no-op.
    pub fn light(&self, folder: &Path) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO lit_folders (path, lit_at) VALUES (?1, ?2)",
            params![path_str(folder)?, now()],
        )?;
        Ok(())
    }

    /// Unlights a folder and forgets what the agent wrote about its files, except those still
    /// inside another lit folder.
    pub fn unlight(&mut self, folder: &Path) -> Result<()> {
        let folder = path_str(folder)?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM lit_folders WHERE path = ?1", params![folder])?;
        let roots = lit_folders(&tx)?;
        let (lo, hi) = subtree(folder);
        let orphans: Vec<String> = {
            let mut stmt = tx.prepare("SELECT path FROM files WHERE path >= ?1 AND path < ?2")?;
            let rows = stmt.query_map(params![lo, hi], |row| row.get::<_, String>(0))?;
            rows.filter(|p| {
                p.as_ref()
                    .map_or(true, |p| !roots.iter().any(|r| Path::new(p).starts_with(r)))
            })
            .collect::<rusqlite::Result<_>>()?
        };
        for path in orphans {
            tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn lit_folders(&self) -> Result<Vec<PathBuf>> {
        lit_folders(&self.conn)
    }

    /// Brings the files table in line with what's on disk in every lit folder. New and changed
    /// files are queued for the agent, or marked sensitive; files that are gone are forgotten.
    pub fn scan(&mut self) -> Result<ScanReport> {
        let roots = self.lit_folders()?;
        let mut seen = Vec::new();
        // A folder that can't be read (an unmounted drive, say) keeps what's known about it.
        let mut unreadable = Vec::new();
        for (i, root) in roots.iter().enumerate() {
            let others: Vec<PathBuf> = roots
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .map(|(_, r)| r.clone())
                .collect();
            scan::walk(root, &others, &mut seen, &mut unreadable);
        }

        let known: HashMap<String, (i64, i64, String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT path, size, modified, hash FROM files")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };

        // Read and hash new or changed files before taking the write lock.
        let mut present = HashSet::new();
        let mut touched = Vec::new();
        let mut upserts = Vec::new();
        for file in seen {
            let Some(path) = file.path.to_str().map(str::to_owned) else {
                continue;
            };
            let size = file.size as i64;
            match known.get(&path) {
                Some((s, m, _)) if *s == size && *m == file.modified => {
                    present.insert(path);
                    continue;
                }
                _ => {}
            }
            // A file that can't be read right now (locked by another program, say) isn't gone;
            // keep what's known and look again next scan.
            let (kind, text) = match scan::classify(&file.path, file.size) {
                Ok(Some(classified)) => classified,
                Ok(None) => continue,
                Err(_) => {
                    present.insert(path);
                    continue;
                }
            };
            let Ok(hash) = hash_file(&file.path) else {
                present.insert(path);
                continue;
            };
            match known.get(&path) {
                // Touched but not changed: keep the description.
                Some((_, _, h)) if *h == hash => touched.push((path.clone(), size, file.modified)),
                previous => {
                    let sensitive = sensitivity(&file.path, text.as_deref());
                    let is_new = previous.is_none();
                    upserts.push((
                        path.clone(),
                        kind,
                        size,
                        file.modified,
                        hash,
                        sensitive,
                        is_new,
                    ));
                }
            }
            present.insert(path);
        }

        let mut report = ScanReport::default();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A folder may have been unlit while the scan was reading; its files stay out.
        let lit_now = lit_folders(&tx)?;
        let still_lit = |path: &str| lit_now.iter().any(|r| Path::new(path).starts_with(r));
        for (path, size, modified) in touched {
            tx.execute(
                "UPDATE files SET size = ?2, modified = ?3 WHERE path = ?1",
                params![path, size, modified],
            )?;
        }
        for (path, kind, size, modified, hash, sensitive, is_new) in upserts {
            if !still_lit(&path) {
                continue;
            }
            if is_new {
                report.added += 1;
            } else {
                report.changed += 1;
            }
            let state = if sensitive.is_some() {
                State::Sensitive
            } else {
                State::Pending
            };
            tx.execute(
                "INSERT INTO files (path, kind, size, modified, hash, state, note)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (path) DO UPDATE SET
                    kind = excluded.kind, size = excluded.size, modified = excluded.modified,
                    hash = excluded.hash, state = excluded.state, note = excluded.note,
                    attempts = 0, title = NULL, summary = NULL, tags = NULL, agent = NULL,
                    indexed_at = NULL",
                params![
                    path,
                    kind.as_str(),
                    size,
                    modified,
                    hash,
                    state.as_str(),
                    sensitive
                ],
            )?;
        }
        let gone = known.keys().filter(|p| {
            !present.contains(*p) && !unreadable.iter().any(|r| Path::new(p).starts_with(r))
        });
        for path in gone {
            tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;
            report.removed += 1;
        }
        // Detection gets stricter over time: a known file whose path now looks like it holds
        // credentials is pulled back, with whatever the agent wrote about it.
        let flagged = present
            .iter()
            .filter_map(|p| Some((p, sensitive::by_path(Path::new(p))?)));
        for (path, reason) in flagged {
            mark_sensitive(&tx, path, reason)?;
        }
        tx.commit()?;
        Ok(report)
    }

    /// Files waiting for the agent, oldest attempts first.
    pub fn pending(&self, limit: usize) -> Result<Vec<(PathBuf, FileKind)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, kind FROM files WHERE state = 'pending'
             ORDER BY attempts, path LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, kind) = row?;
            if let Some(kind) = FileKind::parse(&kind) {
                out.push((PathBuf::from(path), kind));
            }
        }
        Ok(out)
    }

    /// Counts a request to the agent against each file that's still pending, giving up instead on
    /// files that have already had [`MAX_ATTEMPTS`]. Returns the files to ask about.
    pub fn record_attempt(&mut self, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let tx = self.conn.transaction()?;
        let mut attempted = Vec::new();
        for path in paths {
            let p = path_str(path)?;
            let given_up = tx.execute(
                "UPDATE files SET state = 'failed', note = 'the agent did not describe it'
                 WHERE path = ?1 AND state = 'pending' AND attempts >= ?2",
                params![p, MAX_ATTEMPTS],
            )?;
            if given_up == 0
                && tx.execute(
                    "UPDATE files SET attempts = attempts + 1 WHERE path = ?1 AND state = 'pending'",
                    params![p],
                )? == 1
            {
                attempted.push(path.clone());
            }
        }
        tx.commit()?;
        Ok(attempted)
    }

    /// The kind of a file the agent may read: one in a lit folder that isn't sensitive.
    pub fn readable(&self, path: &Path) -> Result<Option<FileKind>> {
        let kind: Option<String> = self
            .conn
            .query_row(
                "SELECT kind FROM files WHERE path = ?1 AND state IN ('pending', 'indexed')",
                params![path_str(path)?],
                |row| row.get(0),
            )
            .optional()?;
        Ok(kind.as_deref().and_then(FileKind::parse))
    }

    /// Stores the agent's description. Returns false if the file isn't one it may describe.
    pub fn describe(&self, path: &Path, description: &Description, agent: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE files SET state = 'indexed', note = NULL, title = ?2, summary = ?3,
                tags = ?4, agent = ?5, indexed_at = ?6
             WHERE path = ?1 AND state IN ('pending', 'indexed')",
            params![
                path_str(path)?,
                description.title,
                description.summary,
                description.tags.join("\n"),
                agent,
                now(),
            ],
        )?;
        Ok(changed == 1)
    }

    /// Flags a file as sensitive after it was found to hold a secret on its way to the agent.
    pub fn mark_sensitive(&self, path: &Path, reason: &str) -> Result<()> {
        mark_sensitive(&self.conn, path_str(path)?, reason)
    }

    pub fn file(&self, path: &Path) -> Result<Option<FileInfo>> {
        let Some(path) = path.to_str() else {
            return Ok(None);
        };
        let row = self
            .conn
            .query_row(
                "SELECT kind, state, note, title, summary, tags, agent FROM files WHERE path = ?1",
                params![path],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        Ok(
            row.and_then(|(kind, state, note, title, summary, tags, agent)| {
                let description = title.map(|title| Description {
                    title,
                    summary: summary.unwrap_or_default(),
                    tags: tags
                        .unwrap_or_default()
                        .lines()
                        .map(str::to_owned)
                        .collect(),
                });
                Some(FileInfo {
                    kind: FileKind::parse(&kind)?,
                    state: State::parse(&state),
                    note,
                    description,
                    agent,
                })
            }),
        )
    }

    /// File counts under `folder`, or across the whole index.
    pub fn counts(&self, folder: Option<&Path>) -> Result<Counts> {
        let row = |row: &rusqlite::Row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?));
        let rows: Vec<(String, i64)> = match folder {
            Some(f) => {
                let (lo, hi) = subtree(path_str(f)?);
                self.conn
                    .prepare(
                        "SELECT state, COUNT(*) FROM files WHERE path >= ?1 AND path < ?2
                         GROUP BY state",
                    )?
                    .query_map(params![lo, hi], row)?
                    .collect::<rusqlite::Result<_>>()?
            }
            None => self
                .conn
                .prepare("SELECT state, COUNT(*) FROM files GROUP BY state")?
                .query_map([], row)?
                .collect::<rusqlite::Result<_>>()?,
        };
        let mut counts = Counts::default();
        for (state, n) in rows {
            let n = n as u64;
            match State::parse(&state) {
                State::Pending => counts.pending += n,
                State::Indexed => counts.indexed += n,
                State::Sensitive => counts.sensitive += n,
                State::Failed => counts.failed += n,
            }
        }
        Ok(counts)
    }

    pub fn set_agent_status(
        &self,
        agent: &str,
        state: AgentState,
        detail: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agent_status (id, agent, state, detail, heartbeat)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT (id) DO UPDATE SET agent = excluded.agent, state = excluded.state,
                detail = excluded.detail, heartbeat = excluded.heartbeat",
            params![agent, state.as_str(), detail, now()],
        )?;
        Ok(())
    }

    pub fn agent_status(&self) -> Result<Option<AgentStatus>> {
        Ok(self
            .conn
            .query_row(
                "SELECT agent, state, detail, heartbeat FROM agent_status WHERE id = 1",
                [],
                |row| {
                    Ok(AgentStatus {
                        agent: row.get(0)?,
                        state: AgentState::parse(&row.get::<_, String>(1)?),
                        detail: row.get(2)?,
                        heartbeat: UNIX_EPOCH
                            + Duration::from_secs(row.get::<_, i64>(3)?.max(0) as u64),
                    })
                },
            )
            .optional()?)
    }
}

/// Upgrades an index written by an older Lantern.
fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version < VERSION {
        // Version 1 had a placeholder files table that nothing wrote to.
        tx.execute_batch("DROP TABLE IF EXISTS files;")?;
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

/// Keeps a file from the agent and forgets anything it already wrote about it.
fn mark_sensitive(conn: &Connection, path: &str, reason: &str) -> Result<()> {
    conn.execute(
        "UPDATE files SET state = 'sensitive', note = ?2, title = NULL, summary = NULL,
            tags = NULL, agent = NULL, indexed_at = NULL
         WHERE path = ?1",
        params![path, reason],
    )?;
    Ok(())
}

fn lit_folders(conn: &Connection) -> Result<Vec<PathBuf>> {
    let mut stmt = conn.prepare("SELECT path FROM lit_folders ORDER BY path")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows
        .map(|r| r.map(PathBuf::from))
        .collect::<rusqlite::Result<_>>()?)
}

/// Why a file must stay local, judged by its name and, for text, the part an agent would see.
fn sensitivity(path: &Path, text: Option<&str>) -> Option<&'static str> {
    sensitive::by_path(path).or_else(|| text.and_then(sensitive::in_text))
}

/// Bounds for a range query matching every path inside `folder`: `lo <= path < hi`.
fn subtree(folder: &str) -> (String, String) {
    let mut lo = folder.to_owned();
    if !lo.ends_with(MAIN_SEPARATOR) {
        lo.push(MAIN_SEPARATOR);
    }
    // The separator is ASCII, so the next character up ends the range.
    let mut hi = lo.clone();
    hi.pop();
    hi.push((MAIN_SEPARATOR as u8 + 1) as char);
    (lo, hi)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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
pub(crate) mod tests {
    use super::*;
    use std::fs;

    pub(crate) fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lantern-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn describe(index: &Index, path: &Path, title: &str) {
        let description = Description {
            title: title.into(),
            summary: "s".into(),
            tags: vec!["a".into(), "b".into()],
        };
        assert!(index.describe(path, &description, "test").unwrap());
    }

    #[test]
    fn lighting_is_idempotent() {
        let mut index = Index::open_in_memory().unwrap();
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
        fs::write(&path, "a").unwrap();
        let first = hash_file(&path).unwrap();
        fs::write(&path, "b").unwrap();
        let second = hash_file(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn scan_queues_files_and_keeps_secrets_local() {
        let dir = tempdir("scan");
        fs::write(dir.join("notes.md"), "# Boat\nThe rudder needs work.").unwrap();
        fs::write(dir.join("keys.txt"), "AKIAIOSFODNN7EXAMPLE").unwrap();
        fs::write(dir.join(".env"), "x").unwrap();
        fs::write(dir.join("server.pem"), "x").unwrap();
        fs::write(dir.join("data.bin"), [0u8, 1, 2]).unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.light(&dir).unwrap();

        let report = index.scan().unwrap();
        assert_eq!(report.added, 3);
        let notes = dir.join("notes.md");
        assert_eq!(
            index.pending(10).unwrap(),
            vec![(notes.clone(), FileKind::Text)]
        );
        assert_eq!(index.readable(&notes).unwrap(), Some(FileKind::Text));
        assert_eq!(index.readable(&dir.join("keys.txt")).unwrap(), None);
        let keys = index.file(&dir.join("keys.txt")).unwrap().unwrap();
        assert_eq!(keys.state, State::Sensitive);
        assert_eq!(keys.note.as_deref(), Some("an AWS access key"));
        assert_eq!(index.file(&dir.join(".env")).unwrap(), None);

        describe(&index, &notes, "Boat notes");
        let counts = index.counts(Some(&dir)).unwrap();
        assert_eq!(
            (counts.indexed, counts.sensitive, counts.pending),
            (1, 2, 0)
        );
        assert_eq!(index.counts(None).unwrap(), counts);

        // Unchanged files keep their description; changed ones go back in the queue.
        assert_eq!(index.scan().unwrap(), ScanReport::default());
        assert_eq!(index.file(&notes).unwrap().unwrap().state, State::Indexed);
        fs::write(&notes, "# Boat\nThe rudder is fixed now, and longer.").unwrap();
        assert_eq!(index.scan().unwrap().changed, 1);
        let info = index.file(&notes).unwrap().unwrap();
        assert_eq!((info.state, info.description), (State::Pending, None));

        fs::remove_file(&notes).unwrap();
        assert_eq!(index.scan().unwrap().removed, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unlighting_forgets_files_outside_other_lit_folders() {
        let dir = tempdir("unlight");
        let inner = dir.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(dir.join("a.txt"), "a").unwrap();
        fs::write(inner.join("b.txt"), "b").unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.light(&dir).unwrap();
        index.light(&inner).unwrap();
        index.scan().unwrap();
        assert_eq!(index.counts(None).unwrap().pending, 2);

        index.unlight(&dir).unwrap();
        assert_eq!(index.file(&dir.join("a.txt")).unwrap(), None);
        assert!(index.file(&inner.join("b.txt")).unwrap().is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unreadable_folder_keeps_its_files() {
        let dir = tempdir("unreadable");
        let folder = dir.join("drive");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("a.txt"), "a").unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.light(&folder).unwrap();
        index.scan().unwrap();

        // Like an external drive being unmounted.
        fs::rename(&folder, dir.join("away")).unwrap();
        assert_eq!(index.scan().unwrap().removed, 0);
        assert!(index.file(&folder.join("a.txt")).unwrap().is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn files_are_given_up_on_after_repeated_attempts() {
        let dir = tempdir("attempts");
        let path = dir.join("a.txt");
        fs::write(&path, "a").unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.light(&dir).unwrap();
        index.scan().unwrap();
        let paths = std::slice::from_ref(&path);
        for _ in 0..MAX_ATTEMPTS {
            assert_eq!(index.pending(10).unwrap().len(), 1);
            assert_eq!(index.record_attempt(paths).unwrap(), paths);
            // The file stays readable for the attempt just counted.
            assert_eq!(index.readable(&path).unwrap(), Some(FileKind::Text));
        }
        assert!(index.record_attempt(paths).unwrap().is_empty());
        assert!(index.pending(10).unwrap().is_empty());
        assert_eq!(index.file(&path).unwrap().unwrap().state, State::Failed);
        assert!(
            !index
                .describe(&path, &Description::default(), "test")
                .unwrap()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn credential_folders_and_recovery_codes_stay_local() {
        let dir = tempdir("credentials");
        fs::create_dir_all(dir.join("Secrets")).unwrap();
        fs::write(dir.join("Secrets/dev.txt"), "server: dev.example.com").unwrap();
        fs::write(dir.join("codes.txt"), "1a2b3-4c5d6\n7e8f9-0a1b2").unwrap();
        fs::write(dir.join("login.txt"), "user: me\npassword: hunter22").unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.light(&dir).unwrap();
        index.scan().unwrap();
        for path in [
            dir.join("Secrets").join("dev.txt"),
            dir.join("codes.txt"),
            dir.join("login.txt"),
        ] {
            let info = index.file(&path).unwrap().unwrap();
            assert_eq!(info.state, State::Sensitive, "{}", path.display());
        }
        assert!(index.pending(10).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn subtree_bounds() {
        let (lo, hi) = subtree(&format!("{MAIN_SEPARATOR}a"));
        let inside = format!("{lo}b");
        let sibling = format!("{MAIN_SEPARATOR}ab");
        assert!(lo <= inside && inside < hi);
        assert!(!(lo <= sibling && sibling < hi));
    }
}

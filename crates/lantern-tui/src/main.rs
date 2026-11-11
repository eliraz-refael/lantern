//! Lantern TUI: Yazi-style parent / current / preview columns, with lit folders marked ✦.

mod files;
mod images;
mod ui;

use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context;
use lantern_core::{AgentStatus, Counts, Index};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use ratatui_image::picker::Picker;

use crate::files::{Entry, Kind};
use crate::images::Images;

/// How often the index is re-read to follow the agent.
const REFRESH: Duration = Duration::from_secs(1);

/// The daemon writes its status every few seconds; older than this, it has stopped.
const DAEMON_TIMEOUT: Duration = Duration::from_secs(15);

fn main() -> anyhow::Result<()> {
    let db = Index::default_path().context("could not determine a home directory")?;
    let index = Index::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let cwd = std::env::current_dir()?;
    let tmux = enable_tmux_passthrough();
    let mut terminal = ratatui::init();
    // Ask the terminal which image protocol it speaks (Kitty, Sixel, iTerm2), else use half-blocks.
    // This has to happen after entering the alternate screen and before reading any events.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let result =
        App::new(index, cwd, Images::new(picker)).and_then(|mut app| app.run(&mut terminal));
    ratatui::restore();
    if let Some(tmux) = tmux {
        tmux.restore();
    }
    result
}

/// Inside tmux, images only reach the real terminal with passthrough on. ratatui-image turns it on
/// without naming a pane, which tmux applies to the active pane rather than ours, so target ours.
/// Returns what's needed to put the pane's previous setting back on exit.
fn enable_tmux_passthrough() -> Option<TmuxPassthrough> {
    let pane = std::env::var("TMUX_PANE").ok()?;
    let output = std::process::Command::new("tmux")
        .args(["show", "-p", "-v", "-t", &pane, "allow-passthrough"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let previous = String::from_utf8_lossy(&output.stdout).trim().to_string();
    tmux(&["set", "-p", "-t", &pane, "allow-passthrough", "on"]);
    Some(TmuxPassthrough { pane, previous })
}

/// The pane's own `allow-passthrough` value before we changed it; empty if it wasn't set.
struct TmuxPassthrough {
    pane: String,
    previous: String,
}

impl TmuxPassthrough {
    fn restore(self) {
        if self.previous.is_empty() {
            tmux(&["set", "-p", "-u", "-t", &self.pane, "allow-passthrough"]);
        } else {
            tmux(&[
                "set",
                "-p",
                "-t",
                &self.pane,
                "allow-passthrough",
                &self.previous,
            ]);
        }
    }
}

fn tmux(args: &[&str]) {
    let _ = std::process::Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

struct App {
    index: Index,
    /// Folders lit directly. Their subfolders are lit through them.
    lit: Vec<PathBuf>,
    counts: Counts,
    /// The daemon's agent, if the daemon is running.
    agent: Option<AgentStatus>,
    home: Option<PathBuf>,
    cwd: PathBuf,
    entries: Vec<Entry>,
    table: TableState,
    parent: Vec<Entry>,
    show_hidden: bool,
    /// One-line message shown in the footer until the next key press.
    status: Option<String>,
    images: Images,
    last_reload: Instant,
}

impl App {
    fn new(index: Index, cwd: PathBuf, images: Images) -> anyhow::Result<Self> {
        let mut app = Self {
            index,
            lit: Vec::new(),
            counts: Counts::default(),
            agent: None,
            home: directories::BaseDirs::new().map(|d| d.home_dir().to_owned()),
            cwd: PathBuf::new(),
            entries: Vec::new(),
            table: TableState::default(),
            parent: Vec::new(),
            show_hidden: false,
            status: None,
            images,
            last_reload: Instant::now(),
        };
        app.reload_index()?;
        app.open(cwd, None);
        app.sync_preview();
        Ok(app)
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        let mut redraw = true;
        loop {
            if redraw {
                terminal.draw(|frame| ui::draw(frame, self))?;
            }
            // While an image is decoding, wake up often to pick it up; otherwise wake up now and
            // then to follow the agent's progress.
            let wait = if self.images.is_loading() {
                Duration::from_millis(50)
            } else {
                REFRESH
            };
            if !event::poll(wait)? {
                redraw = self.images.receive();
                if self.last_reload.elapsed() >= REFRESH {
                    // A busy index is read again on the next refresh; it shouldn't close the TUI.
                    match self.reload_index() {
                        Ok(changed) => redraw |= changed,
                        Err(e) => {
                            self.last_reload = Instant::now();
                            self.status = Some(format!("error: {e:#}"));
                            redraw = true;
                        }
                    }
                }
                continue;
            }
            redraw = true;
            // Windows reports key releases too; only act on presses.
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                if !self.handle_key(key) {
                    return Ok(());
                }
                self.sync_preview();
            }
        }
    }

    /// Returns false when the user quits.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        self.status = None;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => self.move_by(isize::MIN),
            KeyCode::Char('G') | KeyCode::End => self.move_by(isize::MAX),
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => self.leave(),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => self.enter(),
            KeyCode::Char('L') => {
                if let Err(e) = self.toggle_light() {
                    self.status = Some(format!("error: {e:#}"));
                }
            }
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                let keep = self.selected().map(|e| e.path.clone());
                self.open(self.cwd.clone(), keep.as_deref());
            }
            _ => {}
        }
        true
    }

    /// Starts decoding the selected image, if it is one.
    fn sync_preview(&mut self) {
        if let Some(entry) = self.selected().filter(|e| e.kind == Kind::Image) {
            let path = entry.path.clone();
            self.images.request(&path);
        }
    }

    fn open(&mut self, dir: PathBuf, select: Option<&Path>) {
        self.entries = files::list(&dir, self.show_hidden, true);
        self.parent = dir
            .parent()
            .map(|p| files::list(p, self.show_hidden, false))
            .unwrap_or_default();
        let i = select
            .and_then(|s| self.entries.iter().position(|e| e.path == s))
            .unwrap_or(0);
        self.table = TableState::default().with_selected((!self.entries.is_empty()).then_some(i));
        self.cwd = dir;
    }

    fn enter(&mut self) {
        if let Some(entry) = self.selected().filter(|e| e.kind == Kind::Dir) {
            let path = entry.path.clone();
            self.open(path, None);
        }
    }

    fn leave(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            let from = self.cwd.clone();
            self.open(parent, Some(&from));
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let i = self
            .table
            .selected()
            .unwrap_or(0)
            .saturating_add_signed(delta);
        self.table.select(Some(i.min(self.entries.len() - 1)));
    }

    /// Lights the selected folder (or the current one when a file is selected), or unlights it.
    fn toggle_light(&mut self) -> anyhow::Result<()> {
        let target = match self.selected() {
            Some(e) if e.kind == Kind::Dir => e.path.clone(),
            _ => self.cwd.clone(),
        };
        let name = files::display_name(&target);
        match self.lit_root(&target).map(Path::to_path_buf) {
            Some(root) if root == target => {
                self.index.unlight(&target)?;
                self.status = Some(format!(
                    "○ {name} is no longer lit, and its descriptions are forgotten"
                ));
            }
            Some(root) => {
                let via = files::display_name(&root);
                self.status = Some(format!("{name} is already lit through {via}"));
            }
            None => {
                self.index.light(&target)?;
                self.status = Some(match &self.agent {
                    Some(agent) => format!("✦ lit {name}. {} will describe its files", agent.agent),
                    None => format!("✦ lit {name}. Start lantern-daemon to index it"),
                });
            }
        }
        self.reload_index().map(|_| ())
    }

    /// Re-reads lit folders, counts and the agent's status. Returns true if any changed.
    fn reload_index(&mut self) -> anyhow::Result<bool> {
        let lit = self.index.lit_folders()?;
        let counts = self.index.counts(None)?;
        // A daemon that stopped beating is no longer running its agent.
        let agent = self.index.agent_status()?.filter(|s| {
            s.heartbeat
                .elapsed()
                .map_or(true, |age| age < DAEMON_TIMEOUT)
        });
        let changed = lit != self.lit || counts != self.counts || agent != self.agent;
        self.lit = lit;
        self.counts = counts;
        self.agent = agent;
        self.last_reload = Instant::now();
        Ok(changed)
    }

    fn selected(&self) -> Option<&Entry> {
        self.table.selected().and_then(|i| self.entries.get(i))
    }

    /// The lit folder that `path` is in, if any. The deepest one wins.
    fn lit_root(&self, path: &Path) -> Option<&Path> {
        self.lit
            .iter()
            .map(PathBuf::as_path)
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
    }

    fn display_path(&self, path: &Path) -> String {
        match self.home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
            Some(rest) if rest.as_os_str().is_empty() => "~".into(),
            Some(rest) => format!("~{MAIN_SEPARATOR}{}", rest.display()),
            None => path.display().to_string(),
        }
    }
}

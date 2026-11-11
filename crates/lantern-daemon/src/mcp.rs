//! The MCP server an agent indexes through. The agent starts it as `lantern-daemon mcp`, and it
//! is the agent's only way to see files: it reads only files the index says are in a lit folder
//! and not sensitive.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use base64::Engine;
use image::ImageFormat;
use lantern_core::{Description, FileKind, Index, scan, sensitive};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

/// Images are sent at most this many pixels per side: enough to caption, cheap in tokens.
const IMAGE_SIDE: u32 = 1024;

const MAX_TAGS: usize = 12;

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadFile {
    /// Absolute path of the file, as listed in the request.
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SaveFile {
    /// Absolute path of the file, as listed in the request.
    path: String,
    /// What the file is, in a few words, like a good file name a person would choose.
    title: String,
    /// One to three sentences on what it contains: people, places, dates, topics, text in it.
    summary: String,
    /// Three to eight lowercase keywords someone might search for.
    tags: Vec<String>,
}

#[derive(Clone)]
struct Lantern {
    index: Arc<Mutex<Index>>,
    agent: String,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Lantern {
    #[tool(
        description = "Read a file Lantern asked you to describe. Text comes back as text, \
                          images as an image."
    )]
    async fn read_file(
        &self,
        Parameters(ReadFile { path }): Parameters<ReadFile>,
    ) -> CallToolResult {
        let path = PathBuf::from(path);
        let index = self.index.clone();
        let result = tokio::task::spawn_blocking(move || read(&index, &path)).await;
        match result {
            Ok(Ok(content)) => CallToolResult::success(content),
            Ok(Err(message)) => error(message),
            Err(e) => error(format!("reading failed: {e}")),
        }
    }

    #[tool(
        description = "Save what a file is about so it can be found by meaning later. Call it \
                          once per file, after reading it."
    )]
    async fn save_file(&self, Parameters(input): Parameters<SaveFile>) -> CallToolResult {
        let description = Description {
            title: one_line(&input.title),
            summary: input.summary.trim().to_owned(),
            tags: input
                .tags
                .iter()
                .map(|t| one_line(t).to_lowercase())
                .filter(|t| !t.is_empty())
                .take(MAX_TAGS)
                .collect(),
        };
        if description.title.is_empty() {
            return error("the title is empty".into());
        }
        let saved =
            self.index
                .lock()
                .unwrap()
                .describe(Path::new(&input.path), &description, &self.agent);
        match saved {
            Ok(true) => CallToolResult::success(vec![ContentBlock::text("saved")]),
            Ok(false) => error(format!("{} is not a file Lantern asked about", input.path)),
            Err(e) => error(format!("saving failed: {e}")),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Lantern {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("lantern", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Lantern indexes the user's files. Read each file you're asked about with \
                 read_file, then describe it with save_file.",
            )
    }
}

/// Serves the tools on stdin and stdout until the agent disconnects.
pub async fn serve(db: &Path, agent: String) -> anyhow::Result<()> {
    let index = Index::open(db).with_context(|| format!("opening {}", db.display()))?;
    let server = Lantern {
        index: Arc::new(Mutex::new(index)),
        agent,
        tool_router: Lantern::tool_router(),
    };
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

fn read(index: &Mutex<Index>, path: &Path) -> Result<Vec<ContentBlock>, String> {
    let kind = index
        .lock()
        .unwrap()
        .readable(path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "{} is not readable: it isn't in a lit folder, or it holds a secret",
                path.display()
            )
        })?;
    // Scanning skips symlinks; one swapped in since then could point anywhere.
    let is_file = std::fs::symlink_metadata(path)
        .map_err(|e| format!("can't read {}: {e}", path.display()))?
        .is_file();
    if !is_file {
        return Err(format!("{} is no longer a regular file", path.display()));
    }
    if let Some(reason) = sensitive::by_path(path) {
        let _ = index.lock().unwrap().mark_sensitive(path, reason);
        return Err(format!(
            "{} looks like {reason}, so it stays local",
            path.display()
        ));
    }
    match kind {
        FileKind::Text => {
            let text = scan::read_text(path)
                .map_err(|e| format!("can't read {}: {e}", path.display()))?
                .ok_or_else(|| format!("{} is no longer text", path.display()))?;
            // The file may have changed since it was scanned; check again what's about to leave.
            if let Some(reason) = sensitive::in_text(&text) {
                let _ = index.lock().unwrap().mark_sensitive(path, reason);
                return Err(format!(
                    "{} holds {reason}, so it stays local",
                    path.display()
                ));
            }
            Ok(vec![ContentBlock::text(text)])
        }
        FileKind::Image => {
            let image = lantern_core::image::load(path, IMAGE_SIDE)
                .map_err(|e| format!("can't open {}: {e}", path.display()))?;
            let mut jpeg = Vec::new();
            image
                .to_rgb8()
                .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
                .map_err(|e| format!("can't encode {}: {e}", path.display()))?;
            let data = base64::engine::general_purpose::STANDARD.encode(jpeg);
            Ok(vec![ContentBlock::image(data, "image/jpeg")])
        }
    }
}

fn error(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

/// Collapses whitespace so titles and tags stay on one line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lantern_core::State;
    use std::fs;

    #[test]
    fn reads_only_files_the_index_allows() {
        let dir = std::env::temp_dir().join(format!("lantern-mcp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let notes = dir.join("notes.txt");
        fs::write(&notes, "the rudder needs work").unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.light(&dir).unwrap();
        index.scan().unwrap();
        let index = Mutex::new(index);

        let content = read(&index, &notes).unwrap();
        assert_eq!(content[0].as_text().unwrap().text, "the rudder needs work");
        assert!(read(&index, Path::new("/etc/hosts")).is_err());

        // A secret written after the scan is caught on the way out.
        fs::write(&notes, "AKIAIOSFODNN7EXAMPLE").unwrap();
        assert!(read(&index, &notes).unwrap_err().contains("stays local"));
        let info = index.lock().unwrap().file(&notes).unwrap().unwrap();
        assert_eq!(info.state, State::Sensitive);
        assert!(read(&index, &notes).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}

//! Driving an ACP agent: start it, give it Lantern's MCP server, and ask it to describe pending
//! files a batch at a time until none are left.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    ContentBlock, Implementation, InitializeRequest, McpServer, McpServerStdio, NewSessionRequest,
    PermissionOptionKind, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    TextContent,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, LineDirection};
use anyhow::Context;
use lantern_core::{AgentState, Index};
use serde_json::json;

use crate::status::Status;

/// Files per session. Each session starts with an empty context, so a batch's images don't
/// weigh on the next one.
const BATCH: usize = 8;

/// The tools the agent gets, as the MCP server names them.
const TOOLS: [&str; 2] = ["read_file", "save_file"];

/// An agent Lantern knows how to start.
pub struct Spec {
    pub name: &'static str,
    command: &'static str,
    args: &'static [&'static str],
}

pub const AGENTS: &[Spec] = &[
    Spec {
        name: "claude",
        command: "claude-agent-acp",
        args: &[],
    },
    Spec {
        name: "gemini",
        command: "gemini",
        args: &["--acp"],
    },
];

pub fn find(name: &str) -> Option<&'static Spec> {
    AGENTS.iter().find(|spec| spec.name == name)
}

/// Everything a run needs besides the index.
pub struct Run<'a> {
    pub spec: &'static Spec,
    /// Passed to Claude as its model; other agents use their own default.
    pub model: Option<&'a str>,
    pub db: &'a Path,
    /// An empty folder the agent runs in, so its own file tools have nothing to look at.
    pub workspace: &'a Path,
}

/// Starts the agent and has it describe pending files until there are none left.
pub async fn run(run: Run<'_>, index: Arc<Mutex<Index>>, status: &Status) -> anyhow::Result<()> {
    let spec = run.spec;
    let mcp = McpServer::Stdio(
        McpServerStdio::new("lantern", std::env::current_exe()?).args(vec![
            "mcp".into(),
            "--db".into(),
            run.db
                .to_str()
                .context("the index path is not valid UTF-8")?
                .into(),
            "--agent".into(),
            spec.name.into(),
        ]),
    );
    let meta = session_meta(spec, run.model);
    let workspace = run.workspace.to_owned();

    status.set(
        AgentState::Working,
        Some(&format!("starting {}", spec.name)),
    );
    let agent = AcpAgent::new(AcpAgentConfig::new(spec.command).args(spec.args.iter().copied()))
        .with_debug(|line, direction| {
            if matches!(direction, LineDirection::Stderr) {
                tracing::debug!(target: "agent", "{line}");
            }
        });

    Client
        .v1()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if let SessionUpdate::ToolCall(call) = notification.update {
                    tracing::debug!(title = %call.title, "tool call");
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                responder.respond(answer_permission(&request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |cx: ConnectionTo<Agent>| {
            cx.send_request(
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("lantern", env!("CARGO_PKG_VERSION"))),
            )
            .block_task()
            .await?;

            loop {
                let batch = index
                    .lock()
                    .unwrap()
                    .pending(BATCH)
                    .map_err(anyhow::Error::from)?;
                if batch.is_empty() {
                    return Ok(());
                }
                let paths: Vec<PathBuf> = batch.into_iter().map(|(path, _)| path).collect();
                let session = cx
                    .send_request(
                        NewSessionRequest::new(workspace.clone())
                            .mcp_servers(vec![mcp.clone()])
                            .meta(meta.clone()),
                    )
                    .block_task()
                    .await?;
                // Counted once the agent took a session but before the prompt, so a file that
                // crashes the agent is eventually given up on, while an agent that can't start a
                // session (not logged in, say) doesn't use up the files' attempts.
                index
                    .lock()
                    .unwrap()
                    .record_attempt(&paths)
                    .map_err(anyhow::Error::from)?;
                status.set(
                    AgentState::Working,
                    Some(&format!(
                        "{} is describing {} files",
                        spec.name,
                        paths.len()
                    )),
                );
                let prompt = ContentBlock::Text(TextContent::new(prompt(&paths)));
                let response = cx
                    .send_request(PromptRequest::new(session.session_id, vec![prompt]))
                    .block_task()
                    .await?;
                tracing::info!(files = paths.len(), stop = ?response.stop_reason, "batch done");
            }
        })
        .await?;
    Ok(())
}

/// A one-line reason for the TUI, naming the command when the agent isn't installed.
pub fn explain(spec: &Spec, error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    // The SDK reports a failed spawn as a plain message; "not found" is code 2 on Unix and Windows.
    if message.contains("(os error 2)") {
        format!("{} isn't installed", spec.command)
    } else {
        message.lines().next().unwrap_or("failed").to_owned()
    }
}

/// Allows Lantern's own tools and refuses everything else, so the agent can't reach files by
/// another route.
fn answer_permission(request: &RequestPermissionRequest) -> RequestPermissionResponse {
    let fields = &request.tool_call.fields;
    let named = [fields.name.as_deref(), fields.title.as_deref()];
    // Matched exactly as each agent names MCP tools, not by substring: a built-in tool's title
    // can mention a path that happens to contain "lantern" and "read_file".
    let ours = named.into_iter().flatten().any(|name| {
        TOOLS.iter().any(|tool| {
            name == format!("mcp__lantern__{tool}")
                || name.starts_with(&format!("{tool} (lantern MCP Server)"))
        })
    });
    let wanted = if ours {
        PermissionOptionKind::AllowOnce
    } else {
        PermissionOptionKind::RejectOnce
    };
    tracing::debug!(?named, ours, "permission request");
    let outcome = request
        .options
        .iter()
        .find(|option| option.kind == wanted)
        .map_or(RequestPermissionOutcome::Cancelled, |option| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ))
        });
    RequestPermissionResponse::new(outcome)
}

/// Agent-specific session options. For Claude: no built-in tools, only Lantern's MCP server,
/// none of the user's own settings, and a focused system prompt.
fn session_meta(spec: &Spec, model: Option<&str>) -> serde_json::Map<String, serde_json::Value> {
    if spec.name != "claude" {
        return serde_json::Map::new();
    }
    let allowed: Vec<String> = TOOLS.iter().map(|t| format!("mcp__lantern__{t}")).collect();
    let mut options = json!({
        "tools": [],
        "allowedTools": allowed,
        "settingSources": [],
        "strictMcpConfig": true,
    });
    if let Some(model) = model {
        options["model"] = json!(model);
    }
    let meta = json!({
        "systemPrompt": "You describe the user's files for Lantern, a local file search tool. \
                         You can only see files through Lantern's read_file tool and record \
                         what you find with save_file.",
        "claudeCode": { "options": options },
    });
    match meta {
        serde_json::Value::Object(map) => map,
        _ => unreachable!(),
    }
}

fn prompt(paths: &[PathBuf]) -> String {
    let list: String = paths
        .iter()
        .map(|p| format!("- {}\n", p.display()))
        .collect();
    format!(
        "Lantern is indexing these files so the user can find them later by describing what \
         they're looking for. For each file, in order:

1. Call read_file with its path.
2. Call save_file with its path and:
   - title: what it is in a few words, like a good file name a person would pick \
     (\"Rudder repair at the boatyard\", \"Car insurance renewal 2023\").
   - summary: one to three sentences on what it contains. Mention what someone might search \
     for: people, places, dates, amounts, topics, and any text visible in an image.
   - tags: three to eight lowercase keywords.

Write in English and keep names as they appear in the file. If read_file fails for a file, \
skip it. Don't ask questions. When every file is saved, reply \"done\".

Files:
{list}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{
        PermissionOption, PermissionOptionId, ToolCallUpdate, ToolCallUpdateFields,
    };

    fn request(title: &str) -> RequestPermissionRequest {
        let options = vec![
            PermissionOption::new(
                PermissionOptionId::new("allow"),
                "Allow",
                PermissionOptionKind::AllowOnce,
            ),
            PermissionOption::new(
                PermissionOptionId::new("reject"),
                "Reject",
                PermissionOptionKind::RejectOnce,
            ),
        ];
        let call = ToolCallUpdate::new("1", ToolCallUpdateFields::new().title(title));
        RequestPermissionRequest::new("s", call, options)
    }

    fn chosen(response: RequestPermissionResponse) -> Option<String> {
        match response.outcome {
            RequestPermissionOutcome::Selected(s) => Some(s.option_id.to_string()),
            _ => None,
        }
    }

    #[test]
    fn only_lantern_tools_are_allowed() {
        let allow = |title| chosen(answer_permission(&request(title)));
        assert_eq!(allow("mcp__lantern__read_file").as_deref(), Some("allow"));
        assert_eq!(
            allow("save_file (lantern MCP Server)").as_deref(),
            Some("allow")
        );
        assert_eq!(
            allow("Read /Users/me/lantern/notes.md").as_deref(),
            Some("reject")
        );
        assert_eq!(allow("Shell: cat ~/.ssh/id_rsa").as_deref(), Some("reject"));
        assert_eq!(
            allow("ReadFile /Users/me/lantern/read_file.md").as_deref(),
            Some("reject")
        );
    }
}

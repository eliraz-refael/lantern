# Lantern

A local file explorer that can search folders by meaning. Browsing works anywhere; to make a folder searchable you
light it up, and an ACP agent you choose reads it and writes captions and summaries into a local SQLite index.

Design mockups are kept locally (they use real personal files), so they are not in this repo.

## Layout

| Crate | Role |
|---|---|
| `crates/lantern-core` | The index (SQLite), folder scanning and local secret detection |
| `crates/lantern-daemon` | Runs the agent that fills the index, and serves it on `127.0.0.1:7777` |
| `crates/lantern-tui` | Terminal file browser |

## Run

The Nix flake provides the toolchain. With direnv, run `direnv allow` once; otherwise prefix commands with
`nix develop -c`.

```sh
cargo run -p lantern-daemon   # indexes lit folders with an agent; see --help
cargo run -p lantern-tui      # h/j/k/l to move, L to light a folder, q to quit
```

Both use the same index, in your platform's data folder unless `LANTERN_DB` points elsewhere.

## Agents

The daemon starts an [ACP](https://agentclientprotocol.com) agent whenever lit folders have files waiting, and
asks it to describe them a few at a time:

- `--agent claude` (default) runs [`claude-agent-acp`](https://github.com/agentclientprotocol/claude-agent-acp).
  `--model haiku` keeps captioning cheap.
- `--agent gemini` runs `gemini --acp`.

What the agent can see:

- The agent reads files only through Lantern's MCP server (`lantern-daemon mcp`), which serves only files inside a
  lit folder. Claude runs without its own file and shell tools and without your Claude settings. Any other tool
  permission the agent asks for is refused.
- Hidden files and folders, symlinks, `node_modules` and formats an agent can't read yet (PDF, video, archives) are
  skipped.
- Files that look like credentials (`.pem`, `id_rsa`, password stores) or contain keys and tokens (AWS, GitHub,
  Slack, Stripe, private keys) are detected locally and never sent. Text is checked again just before it is sent.
- Unlighting a folder forgets what the agent wrote about its files.

CI builds and tests on macOS, Windows and Linux.

## License

[MIT](LICENSE)

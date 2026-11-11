# Lantern

A local file explorer that can search folders by meaning. Browsing works anywhere; to make a folder searchable you
light it up, and an ACP agent you choose reads it and writes captions and summaries into a local SQLite index.

Design mockups are kept locally (they use real personal files), so they are not in this repo.

## Layout

| Crate | Role |
|---|---|
| `crates/lantern-core` | The index (SQLite) and file hashing |
| `crates/lantern-daemon` | Owns the index and serves it on `127.0.0.1:7777` |
| `crates/lantern-tui` | Terminal file browser |

## Run

The Nix flake provides the toolchain. With direnv, run `direnv allow` once; otherwise prefix commands with
`nix develop -c`.

```sh
cargo run -p lantern-daemon   # then: curl localhost:7777/api/health
cargo run -p lantern-tui      # h/j/k/l to move, q to quit
```

CI builds and tests on macOS, Windows and Linux.

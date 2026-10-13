# codex, cut down to the agent loop

A fork of [openai/codex](https://github.com/openai/codex) with the security layers and most of the product surface removed. What's left is the loop itself: the model asks for a tool, the tool runs, the result goes back to the model.

Commands run on your machine with your user's privileges. There is no sandbox and no approval prompt.

## Why

I run Codex in full-access mode all the time. In that mode the sandbox, the approval prompts and the reviewer agent do nothing, yet they make up a large share of the code. I wanted to see what Codex looks like without them.

## What's left

- A `codex` binary (`codex-rs/cli`), headless, for one-shot prompts or an interactive session.
- The loop: threads, turns, streaming, compaction, AGENTS.md. Threads live in memory and disappear when the process exits.
- Tools: shell (unified exec), `apply_patch`, MCP servers declared in `config.toml`, sub-agents, `update_plan`, `request_user_input`, `view_image`, tool search, web search, image generation, skills and hooks.

## What's gone

Security: sandboxing (Seatbelt, Landlock/bubblewrap, Windows), approval policies and prompts, guardian auto-review, exec-policy rules, the network proxy and its destination allowlists, project trust, permission profiles, managed requirements and cloud config, process hardening, the shell-snapshot credential broker, dangerous-command detection.

Everything around the loop: the TUI, app-server and its SDKs, cloud tasks, realtime voice, code mode, plugins and marketplaces, ChatGPT apps, session storage (rollouts, SQLite state, resume, memories), remote execution environments, telemetry (OpenTelemetry, metrics, analytics), non-OpenAI auth (Bedrock, workload and agent identity), feedback upload, rollout tracing, TUI and desktop settings.

All tests are gone too. Rust went from 2.05M lines (1.06M production, 0.99M tests) to about 172k, and the workspace from about 160 crates to about 60.

## Running it

```shell
cd codex-rs
cargo build -p codex-cli
mkdir -p ~/.codex-loop && cp ~/.codex/auth.json ~/.codex-loop/   # ChatGPT login, or export CODEX_API_KEY
CODEX_HOME=~/.codex-loop ./target/debug/codex "make the failing test pass"
```

With no prompt and a terminal on stdin, `codex` opens an interactive session. Piped stdin is read as the prompt.

| Flag | |
|---|---|
| `-m, --model` | model to use |
| `-C, --cd` | working directory |
| `-c key=value` | override a `config.toml` value (parsed as TOML) |
| `--json` | print raw events as JSON lines |
| `--reasoning` | show reasoning summaries on stderr |

Ctrl-C interrupts the current turn. The assistant's text goes to stdout and tool activity to stderr, so `codex "..." > answer.md` keeps only the answer.

## Caveats

- The model can run anything you can run. Point it at a machine or a directory where that's fine.
- Keep a separate `CODEX_HOME`. This build stores nothing about your sessions, but it reads `config.toml` from there (and warns about keys it no longer knows, like project trust levels, TUI settings and plugins) and rewrites the model cache and bundled system skills. It never touches the official daemon's `/tmp/codex-daemon-$UID` directory.
- MCP elicitations that only ask for confirmation are accepted automatically. Forms are asked in the terminal, and declined when there is no terminal.
- I've only run it end to end on macOS. Linux and Windows builds are untested.
- The Bazel files, GitHub workflows, `docs/` and the npm wrapper come from upstream and aren't maintained here. Build with cargo.
- There are no tests. Every change is checked by building and running real turns.

Apache-2.0, same as upstream.

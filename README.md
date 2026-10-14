# crab

`crab` is a coding agent for the terminal, written in Rust. It is inspired by
**pi**: it borrows the concept model (an agent loop, a session log, tool
descriptions that live next to the tool), but it is its own thing — its own
format and its own extension mechanism, with no attempt at compatibility.

It is a small single binary that starts fast, so you can put it in a pipe, wire
it into a script, or just open it and type.

Design decisions (and the reasoning behind them) live in [DESIGN.md](DESIGN.md).
This file is the user-facing manual. For how crab was built — including the fact that
much of it was written by crab itself — see [STORY.md](STORY.md).

## Install

Build from source with Cargo:

```sh
cargo install --path crates/crab-cli
```

This installs a binary named `crab`. You need a Rust toolchain — the workspace
pins one in `rust-toolchain.toml` (rustfmt + clippy included).

## Quickstart

Point `crab` at a model with an API key:

```sh
export CRAB_API_KEY=sk-...
```

`OPENAI_API_KEY` also works if `CRAB_API_KEY` is unset. With no config file at
all, `crab` talks to an OpenAI-compatible endpoint (`https://api.openai.com/v1`,
model `gpt-4o-mini`); override with `CRAB_BASE_URL` / `CRAB_MODEL`.

Run once and print the final response:

```sh
crab --print "explain what this repo does"
```

Or start the interactive session:

```sh
crab
```

The prompt is `crab> `. Type `/quit` (or `/exit`) to leave, or press Ctrl-D.
While the agent is running, **Ctrl-C aborts the current run** (it does not exit
`crab` — the text and tool results produced so far are kept on disk). A Ctrl-C
at an idle prompt just clears the input line.

`--print` is non-interactive: there is nobody to answer a confirmation, so tool
calls are auto-approved (see [Permissions](#permissions)).

## Configuration

`crab` reads `~/.crab/config.toml`. A missing file is fine — it just means "use
environment variables only". The shape:

```toml
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
api_key = "sk-..."          # optional if CRAB_API_KEY / OPENAI_API_KEY is set
context_window = 128000

[providers.local]
base_url = "http://localhost:8080/v1"
model = "qwen"
```

Provider fields:

| field | type | notes |
| --- | --- | --- |
| `base_url` | string | OpenAI-compatible base URL (required) |
| `model` | string | model name sent to the API (required) |
| `api_key` | string, optional | falls back to `CRAB_API_KEY` / `OPENAI_API_KEY` |
| `context_window` | integer, optional | shown in the status line; enables "running out of window" warnings |

Resolution order: the `--provider <name>` flag wins, otherwise
`default_provider`, otherwise `openai`. For each field, environment variables
beat the profile:

| env var | overrides |
| --- | --- |
| `CRAB_API_KEY` / `OPENAI_API_KEY` | `api_key` |
| `CRAB_BASE_URL` / `OPENAI_BASE_URL` | `base_url` |
| `CRAB_MODEL` | `model` |
| `CRAB_CONTEXT_WINDOW` | `context_window` |

Local endpoints (`localhost` / `127.0.0.1`) are allowed to have an empty API
key; anything else without a key is an error. To switch providers per run:

```sh
crab --provider deepseek
```

`--provider mock` is a built-in stub provider that returns a canned reply — handy
for debugging the loop without spending tokens.

`crab` also reads `AGENTS.md` from the current working directory and appends it
to the system prompt as project instructions. `--system "<text>"` appends extra
instructions after that.

## Tools

`crab` ships seven built-in tools:

| tool | what it does |
| --- | --- |
| `read` | Read a UTF-8 text file (truncated past 100 KB). Marks the file as read for the freshness gate. |
| `write` | Write a file (create or overwrite). Overwriting requires the file to have been read this session and unchanged since. Atomic. |
| `edit` | Exact-match string edits; each `old` must match exactly once, edits apply all-or-nothing against the original content. |
| `bash` | Run a shell command. 120 s timeout; long output is truncated (head + tail kept). |
| `glob` | Find files by glob pattern (e.g. `**/*.rs`). |
| `grep` | Regex search file contents; respects `.gitignore` / `.ignore`, skips hidden files. |
| `web_fetch` | Fetch a URL and return its extracted main article text (readability); strips nav/ads/scripts. |

A few invariants worth knowing:

- **Freshness gate.** `edit` and `write` refuse to touch a file you have not
  read this session, and re-check that the file's mtime has not changed since.
  This is what stops the agent from silently clobbering the edits you made in
  your own editor.
- **`edit` is literal, never fuzzy.** "0 matches" or "more than one match" is a
  loud error, by design. Fuzzy matching would replace a visible failure with a
  silent wrong edit.

## MCP servers

External tools are attached over the [Model Context
Protocol](https://modelcontextprotocol.io/). Add servers under `[mcp.<name>]` in
`~/.crab/config.toml`:

```toml
[mcp.brave]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-brave-search"]
enabled = true
env = { BRAVE_API_KEY = "..." }
```

MCP server fields:

| field | type | notes |
| --- | --- | --- |
| `command` | string | program to spawn (required) |
| `args` | array of strings | arguments (default: empty) |
| `enabled` | boolean, optional | omitted means enabled; `false` skips the server entirely |
| `env` | table of string→string | extra environment variables for the child process, layered **on top of** the inherited environment |

Those `env` values are added to the child's environment rather than replacing
it, so `PATH` and everything else still work. Putting a credential here (instead
of baking it into `args`) also keeps it out of `ps` output and out of the
permission prompt.

Every tool a server exposes is registered as **`<server>__<tool>`** — two
servers may both expose `search`, and the model needs to tell them apart. A
server named `brave` running `brave_web_search` shows up to the model as
`brave__brave_web_search`. Several tools from one server share a single child
process.

Server startup is best-effort: a command that fails to spawn or whose
`tools/list` fails prints a warning on stderr and is **skipped**. One broken MCP
server never prevents `crab` from starting.

### Node-based servers behind a proxy

`npx`-style MCP servers inherit your shell environment, including
`HTTP_PROXY` / `HTTPS_PROXY`. But Node's `fetch` (undici) **ignores those
variables by default**, so a Node server behind a corporate proxy may fail to
reach the network. The fix is to turn on Node's env-proxy support for the child:

```toml
[mcp.brave]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-brave-search"]
env = { BRAVE_API_KEY = "..." , NODE_OPTIONS = "--use-env-proxy" }
```

`--use-env-proxy` (equivalently `NODE_USE_ENV_PROXY=1`) makes Node's built-in
`http`/`https` and `fetch` honor `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY`. It is
opt-in and requires a recent Node (the `fetch` path is v22.21.0 / v24.0.0+).

## Permissions

Tool calls go through a permission gate, layered as follows:

- **Read-only operations pass through.** `read`, `glob`, `grep`, and `bash`
  commands classified as read-only (e.g. `ls`, `cat`, `git status`) run without
  asking.
- **Everything that writes, runs, or touches the network asks first.** `write`,
  `edit`, `web_fetch`, non-read-only `bash`, and every MCP tool call prompt:

  ```
  ⚠ bash — run: rm -rf build
  allow? [y]es/[n]o/[a]ll:
  ```

  `a` ("all") approves that class for the rest of the session.
- **`--yolo` disables the prompts** and allows everything.
- **`--print` auto-approves**, since there is nobody at the keyboard to answer.

> **This is not a security boundary.** The gate is a seatbelt against a
> typo or a confused model, nothing more. Any allowlist built on top of command
> strings is trivially bypassed — `bash -c`, `eval`, `$(...)`, writing a script
> and running it, `find -exec`. The only real isolation is a container. See
> [DESIGN.md §6](DESIGN.md) for the full rationale.

## Sessions

Each run writes an append-only JSONL session under `.crab/sessions/` in the
current directory (you can `tail -f` it, or `git diff` it). To resume the most
recent one:

```sh
crab --continue
```

Interrupted runs are still resumable: text produced and tool results completed
before a Ctrl-C abort are persisted.

## License

Apache-2.0. See [LICENSE](LICENSE).

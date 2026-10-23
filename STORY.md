# crab: a coding agent that built itself

`crab` is a coding agent for the terminal, written in Rust, inspired by pi. This
document is the story of how it was built — and the unusual part: **a large share of
it was built by crab itself.**

## What it is

A single ~12 MiB binary that starts in milliseconds and drives a model through a real
tool loop:

- **7 built-in tools**: `read`, `write`, `edit`, `bash`, `glob`, `grep`, `web_fetch`
- **MCP client**: attach external tool servers over stdio (Brave Search verified)
- **Multi-provider config**: `~/.crab/config.toml` profiles (DeepSeek, Kimi, OpenAI,
  local) with per-field env overrides
- **Fullscreen TUI** with a live status line (model · state · tokens · context usage ·
  cwd), a spinner with elapsed timer, and `/yolo` / `/model` slash commands
- **Persistent sessions** (append-only JSONL, `--continue` resumes with full history)
- **Permission gate** on anything that writes, runs, or touches the network
- **Skills**: Agent Skills directories are discovered and advertised in the
  system prompt; `/skill:<name>` injects one on demand
- **Local observability**: versioned session log, `crab trace` (span tree),
  `crab metrics` (tokens, cost, latency) with versioned per-model pricing

356 tests. Apache-2.0. [Releases](https://github.com/jinpeng/crab-agent/releases) ship
precompiled binaries for macOS (arm64) and Linux (x86_64, arm64).

## The self-hosting arc

The acceptance criterion set at design time (Q2) was deliberately un-gameable: **a
commit whose diff was produced entirely by crab itself.** Not a benchmark, not a vibe —
a real commit in the history.

That was reached at commit `f3fec8e` ("dogfood"), where crab added `Session::len` + a
test. From there it snowballed. Of ~194 commits to date, **21 were authored by crab**
(via DeepSeek), each one produced by handing crab a task spec in its own repo and reviewing
the result:

- The entire TUI layer: semantic color theme, ANSI-aware width helpers, the status
  line, colored structured tool blocks
- Token-usage capture through the SSE pipeline
- The `web_fetch` tool (readability extraction, size caps, gate) — *including applying
  its own code-review feedback*
- `grep` respecting `.gitignore`
- The MCP client: `Tool::name → Cow` refactor, config sections, the stdio client,
  tool wrapping, startup wiring
- This README and the original (cargo-based) `install.sh` — since replaced by the
  managed binary installer
- The built-in pricing table's legacy DeepSeek "effective since" dates — verified
  with cited sources, and it *overturned the premise*: the "stale" prices were
  the current cards, mislabeled (see the dogfood log)

The dogfood log is in [docs/dogfood-log.md](docs/dogfood-log.md).

## What crab was good at — and where it needed a human

The interesting part isn't that it wrote code; it's *how* it behaved on the hard parts.

**It caught real problems on its own:**
- Spotted that a task spec said "don't touch other files" while its test couldn't run
  without a `mod` declaration — and instead of claiming a false pass, verified the file
  out-of-tree and reported the conflict.
- Found that a flaky test was *pre-existing*, and proved it by stashing its own changes
  and re-running the original.
- Discovered the agent emits no "waiting for confirmation" event, so it added a shared
  flag to coordinate the renderer with the prompt.
- Wrote a self-contained in-test MCP server (hand-rolled JSON-RPC over stdio) so the
  client could be tested without a real subprocess.
- Refused to guess at the one genuinely hard problem (a persistent status line fighting
  reedline for the bottom row) — explored it, concluded the scroll-region approach
  conflicted, and said so rather than writing broken code.

**Where a human still had to step in:**
- rmcp's handshake has no timeout; a non-MCP subprocess hung forever. (Design-level fix:
  a 10s handshake timeout.)
- A timing-sensitive self-spawn round-trip test needed `#[ignore]` + serialization.
- The status-line / reedline layout conflict (the part crab declined) was done by hand.

The pattern: crab is strong on well-specified, testable units and honest about the
fuzzy ones. The human's job shifted from writing to *specifying crisply and reviewing*.

## Hard-won findings (the kind you only get by doing it)

- **Node's `fetch` ignores `HTTP(S)_PROXY`.** Any Node-based MCP server behind a proxy
  needs `NODE_OPTIONS=--use-env-proxy` (Node 22.21+/24+). crab's per-server `env` config
  carries this.
- **DeepSeek sends `usage` in the same SSE chunk as `finish_reason`.** Break the stream
  on `StreamEnd` without first capturing usage and you lose it.
- **Provider tool-call streaming is fragile** (index-less fragments, bare-string
  `arguments`). A tolerant accumulator beats a strict parser.
- **`edit` must be literal, never fuzzy** — a loud "no match" beats a silent wrong edit.
- **The permission gate is a seatbelt, not a boundary.** Command-string allowlists are
  trivially bypassed; real isolation is a container.
- **Terminals measure width in cells, not chars.** CJK characters take 2 cells,
  combining characters 0; any `chars().count()` in cursor math or truncation silently
  misaligns. Use `unicode-width` everywhere width matters.

## Numbers

| | |
|---|---|
| Lines of Rust | ~16,700 |
| Tests | 356 (3 ignored: two timing-sensitive self-spawn round trips, one subprocess helper) |
| Binary (release) | ~12 MiB |
| Cold start | sub-second (vs ~150 ms for the Node-based pi it was measured against) |
| Commits | 194 |
| Commits authored by crab | 21 |

## Design

The full decision record — what was chosen, and *why*, including the things explicitly
rejected (WASM/JS plugins, SQLite sessions, OS sandboxing, tiktoken, alt-screen TUI) —
is in [DESIGN.md](DESIGN.md).

## Status

v0.4.0. It works, it self-builds, and it's published. The honest label is "early but
real": the core loop, tools, config, MCP, and TUI are solid and tested. Since v0.3.0
it gained **context compaction** (deterministic tool-result shedding plus on-demand
LLM summary, replayable from the log — the session file is never rewritten), a
**meter** that seeds exact context occupancy and token counters at `--continue`,
a **pi-parity status line** (↑↓ input/output, R/W cache tokens, CH cache-hit rate,
`$` cumulative cost, one-decimal context percent), **pi-style copy** (drag to
select, release to copy) with proper spacing between messages, and **slash-command
hints** that complete commands *and their arguments* (`/model ` lists profiles,
`/skill:` lists skills). It also ships a faithful pre-wrap of the transcript that
fixed a ratatui rendering bug — over-wide rows were clipped and a character could
silently disappear off screen. And it installs like pi: a managed installer
(`curl … install.sh | sh`) lays out versioned releases under
`~/.crab/install/releases/`, a launcher resolves the current one at every start,
and `crab update` / `/update` self-updates with checksum-verified downloads.
Still deliberately future work: themes, session fork/branch, and a real extension
API.

---

*Built with crab. Reviewed by a human.*

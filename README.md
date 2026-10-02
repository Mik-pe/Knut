# Knut

![Knut — Your code, untangled.](assets/knut-cover.webp)

Knut is an experimental AI agent for your terminal, written in Rust. Give it a
task in plain language: it can inspect files, propose and apply edits, run
commands with your approval, and check the result. It also provides an embeddable
harness for applications with their own tools, context and completion rules.

Coding is the first built-in profile. You can also use Knut to summarize local
documents, look up information through host-provided tools, or have a
conversation without a repository. A model provider is needed for real tasks;
the optional decision model helps prioritize work.

## What can I use it for?

- **Understand a project:** ask where a feature lives or how a module works. Knut
  can inspect the relevant files and follow up with tools.
- **Change code:** ask it to fix a failing test or make a focused refactor. You
  review file changes and command approvals in the terminal; coding tasks use
  checks bound to the resulting source revision.
- **Work with local documents:** select the general profile to summarize notes
  or edit text without requiring a build system.
- **Build an agent into another application:** register your tools and connect
  a context provider and completion monitor to the same runtime.

Knut is alpha software. Start in a project you can review with version control.
The built-in coding checks cover Rust and a basic TypeScript/Node setup; custom
workflows can supply their own completion contract.

## Quick start

Install a current stable Rust toolchain (with `rustfmt` and `clippy`). Command
execution also needs Bubblewrap on Linux or `sandbox-exec` on macOS. On Debian
or Ubuntu, Bubblewrap is available as the `bubblewrap` package; the host must
permit its user namespaces. Install your project's dependencies before running
checks, because sandboxed commands deny network access by default.

```sh
git clone https://github.com/Mik-pe/Knut.git
cd Knut
cargo install --path . --locked

# Open Knut in the project or folder you want it to work on.
cd /path/to/your/project
knut
```

Press **F2** to open Settings, choose **Continue with ChatGPT**, complete browser
sign-in, then select a model. The choice is saved for the next start. You can
also configure an API provider; see [connections and configuration](docs/configuration.md)
for API-key setup, compatible endpoints, account management and all environment
variables. A saved connection takes priority over environment configuration;
Settings includes **Use environment configuration** to switch explicitly.

Try a task such as:

```text
Explain how this project starts, and point me to the main entry point.
Fix the failing test while preserving the public API and existing tests.
```

For document work inside a code repository:

```sh
KNUT_PROFILE=general knut run "Summarize notes.txt"
```

Run `knut doctor` for an offline setup check. `knut doctor --live` makes a real
request to each configured provider and may consume usage. Real tasks also
consume the configured provider's usage; local fixture tests use no paid models.

For development without installing, use `cargo build --locked` and
`cargo run --locked -- tui`. Cargo's configured target directory may differ
from `target/`; the scripts discover the binary through `cargo metadata`.

## How a task runs

One shared runtime serves the terminal, CLI and headless clients:

```text
Your task + project instructions + tools + context
                         |
                         v
               model -> gated tool calls
                 ^             |
                 +-- results --+
                         |
             final answer + required checks
```

The model chooses tools and receives their results with the original call IDs.
Every call passes through `ExecutionGate`, which enforces permissions, exact
approvals and replay rules in Rust. An optional System One decision model
prioritizes context and capabilities and helps classify failures; it cannot
grant permissions or waive required evidence. Knut works with the reasoner
alone when that decision model is absent.

File edits use content hashes to reject stale changes. Commands run through an
OS sandbox; an unavailable backend refuses execution. The coding profile checks
changed project content before reporting verified completion, and gives the
model bounded opportunities to repair failures. A question that leaves the
project unchanged can finish without a build. Explicit task requirements still
apply. Tasks have limits of 32 runtime turns, 16 tool calls per response and two
completion-repair attempts.

## Useful commands

| Command | Purpose |
| --- | --- |
| `knut` / `knut tui` | Open the interactive session |
| `knut run "task"` | Run a task and print its outcome |
| `knut doctor` / `knut doctor --live` | Inspect setup / test configured providers |
| `knut models` | List the configured provider's available models |
| `knut verify --json` | Run discovered project checks and report evidence |
| `knut jsonl` | Accept commands on stdin and emit structured events on stdout |
| `knut sessions list` / `knut sessions show <id>` | List sessions / replay a stored transcript |
| `knut bench` | Run the offline pilot benchmark |
| `knut help` | Show the complete command reference |

`knut run` stops when user input or approval is needed. Use the TUI or JSONL
for tasks with interactive command approvals. `--yes` pre-approves file edits;
commands still need their exact approval.

`route`, `repl`, `demo-tree` and `eval` are an offline playground with a mock
router and canned tools. Their results measure the fixture, not coding quality.

## Interactive CLI

![Knut's woven knot and terminal workspace](assets/knut-terminal.png)

The trefoil knot has a moving highlight on opening and during active work.
It settles after the introduction and stays still while waiting for approval.
[Watch the opening animation](assets/knut-terminal.gif).

Run `knut` in a terminal (or `knut tui`) to open a coding session. The main
screen is one conversation, a compact status line, and an input that grows with
your draft. Jobs, routing details, and changes open only when requested.

Type a task and press Enter. During work, Enter queues a separate task. Alt+S
switches the current draft to steering, which adds a correction to the active
goal. Successful completion starts the next queued task; failure or cancellation
holds the queue. Open jobs to edit, remove or explicitly start held requests.
Queue acknowledgements arrive at the next runtime boundary.

Approvals open a scrollable preview of the exact action, replacements, read hash
and complete arguments. Alt+V toggles the preview; PgUp/PgDn scrolls it. Enter
while approval is pending preserves the draft unless steering was explicitly
selected.

| Key | Action |
| --- | --- |
| Enter | Send a task, answer a question, or follow up |
| Shift+Enter | New line (Ctrl+J fallback for terminals without enhanced keyboard support) |
| Up / Down | Move through input lines; recall history at its edges |
| Ctrl+A / Ctrl+E | Start / end of input line |
| Ctrl+Z / Ctrl+Y | Undo / redo, including an idle draft cleared with Ctrl+C |
| / at empty input, Ctrl+P, Ctrl+K | Search working commands |
| Up / Down, Enter in commands | Select and run a command |
| Ctrl+R | Open/close recorded changes and checks |
| Ctrl+O | Open/close jobs and queued requests |
| Alt+S | Switch the current draft between queue and steer during work |
| Up / Down in jobs | Select a queued request |
| Alt+E / Alt+X in jobs | Edit / remove the selected queued request |
| Alt+R in jobs | Start the selected queued request when idle |
| Ctrl+B | Open/close decision details |
| PgUp / PgDn | Scroll the conversation or current detail view |
| Tab / Shift+Tab | Switch input and conversation navigation |
| Esc | Close a detail view or return to the latest output |
| Alt+A / Alt+D | Allow / deny the exact pending approval |
| Alt+V | Open/close the full pending-action preview |
| Ctrl+Left / Right or Alt+B / F | Move by word |
| Ctrl+W or Alt+Backspace | Delete the previous word |
| Ctrl+C | Close an overlay; otherwise stop work, clear an idle draft, or exit |
| Ctrl+D | Exit when idle with an empty draft |
| Ctrl+Q | Save the draft and exit when idle |
| ? / F1 | Quick shortcut modal (`?` with empty input; F1 anytime) |

In changes, Left/Right selects a file, Up/Down selects a hunk, and PgUp/PgDn
scrolls it. The review is read-only; it does not pretend to revert applied edits.
Use the command menu for pause, resume, cancel, setup diagnostics, and checks.

The theme uses warm text, teal accents, and explicit state labels. It
adapts to truecolor, 256 colors, 16 colors, and `NO_COLOR`; configure
`KNUT_TUI_COLORS=truecolor|256|16|none` to override detection, including
`NO_COLOR`. Use `/motion` to toggle animation, or `KNUT_TUI_MOTION=off` to
start with reduced motion. `TERM=dumb` uses a static ASCII mark.
Bracketed paste is one undoable edit, normalizes Windows newlines, and reports
truncation. Browsing prompt history preserves the unfinished draft and cursor.
The terminal is restored on exit.

Drafts, cursor positions and recent prompt history survive restarts in the same
workspace. Input is saved locally in the background every 400 ms and flushed on
normal exit; Ctrl+Q keeps the draft for next time. Up/Down recalls saved history.
Storage uses `$XDG_DATA_HOME/knut/sessions.db` (normally
`~/.local/share/knut/sessions.db`), or `KNUT_SESSION_STORE`, with owner-only file
permissions. History keeps up to 100 prompts within a one-million-character budget.
Storage errors and conflicting saves from another terminal are reported.
This restores input only; running tasks, queue edits and approvals are not resumed.

See [CLI_UX.md](CLI_UX.md) for the design and remaining usability work.

## Workspace layout

| Crate | Responsibility |
| --- | --- |
| `knut` | CLI entry point and compatibility exports |
| `knut-runtime` | Sessions, providers, tools, checks, persistence and headless protocols |
| `knut-terminal` | Terminal UI, connection screens and event replay |
| `knut-auth` | Account storage, authentication and token refresh |
| `knut-editor` | Composer state and edit memory, independent of the runtime/UI |

The terminal depends on the runtime. Runtime and terminal share auth/editor
primitives; the runtime does not depend on the terminal. All adapters keep using
one session engine. `cargo test --workspace` validates every crate.

The main implementation lives under `crates/`; `src/main.rs` is the CLI adapter,
`tests/` covers cross-crate behavior, and `scripts/` contains process and terminal
smoke checks. Start with [the harness guide](docs/harness.md) for runtime
contracts, embedding, JSONL task options and evaluation. [CLI_UX.md](CLI_UX.md)
describes the terminal UX; [REPO_READINESS.md](REPO_READINESS.md) records the
readiness goals and outstanding evaluation work.

## Developing and checking changes

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --locked
```

Sandbox tests need a working OS backend. CI provisions Bubblewrap on Linux and
runs offline tests; these checks do not establish real provider quality or
successful browser sign-in.

For a local process regression using a deliberately fallible provider:

```sh
cargo build
node scripts/smoke-harness.mjs
```

It uses an isolated Rust fixture, makes no paid provider calls, and verifies
tool-argument repair, failed-check recovery, exact approvals and queued task execution.
This is not evidence of improved live-model success rates.

For terminal regression checks, install the optional `tuistory` package where
Node can resolve it, then run `node scripts/smoke-tui.mjs --screenshots`.
For an existing installation, set `KNUT_TUISTORY_MODULE` to its absolute
`dist/index.js` path. This uses the local fixture provider, exercises four
terminal sizes, editing, help, approvals and repair, and saves snapshots in a
temporary directory. It also checks truecolor overrides and ASCII/reduced motion.

## Status and limitations

- TUI, `run`, and JSONL drive the same native conversation runtime. It performs
  real reads/edits, resumes exact approved calls, and runs repository checks
  against the edited revision. Failed checks and tool failures have a bounded
  repair budget. The native loop has offline regression coverage.
- Check discovery currently covers Rust and a basic TypeScript/Node profile;
  project-specific scripts and other package managers need richer setup support.
- Typed artifact references support JSON-pointer projections, including a
  search hit's path and a read's content hash. Unsupported or missing selected
  values fail explicitly.
- `bench` is a pilot: six fixture tasks, all offline. It is not the
  30-task held-out evaluation the roadmap calls for.
- Language servers are reported but never started implicitly; LSP
  navigation is fixture-tested, not live-verified here.
- Command isolation uses Bubblewrap on Linux and Seatbelt (`sandbox-exec`) on
  macOS. Both start with a clean environment and deny network by default.
  macOS commands use a private temporary directory and declared writable paths;
  the backend is reported with every command result. Windows is unsupported.
  A missing backend refuses execution.
- TUI performance was measured on a synthetic fixture (p95 under 5 ms for
  reducer plus render), not against real provider latency.

Knut is MIT licensed. See [LICENSE](LICENSE).

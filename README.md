# Knut

Knut is an experimental agentic harness in Rust.

The core idea is to spend expensive generative reasoning only when it is useful. Fast, bounded System One decisions route work between clarification, retrieval, tools, fast generation, and deeper reasoning.

```text
user prompt
    |
    v
System 0        deterministic rules / cache
    |
    v
System 1        Jev-style bounded decisions
    |
    +---- ask user
    +---- retrieve
    +---- tool
    +---- fast model
    `---- reasoner
              |
              v
        validated plan/tree
              |
              v
      System 1 chooses next edge
```

## Prototype goals

- Keep control flow explicit and testable.
- Treat System One as a router and edge selector, not a text generator.
- Batch independent routing judgments when possible.
- Escalate on uncertainty instead of trusting a weak route.
- Keep permissions and side effects outside model judgment.
- Make every routing decision observable and evaluable.
- Stay model-provider agnostic above thin adapters.

## Current scope

The core contains:

- typed routing decisions,
- a `SystemOne` trait,
- deterministic System 0 fast paths (named rules, routing cache),
- batched ingress judgments in one System One call,
- confidence-gated escalation,
- a capability-oriented tool registry with bounded candidate discovery,
- a model tier abstraction with a bounded compute cascade,
- a validated behavior-tree core with cooperative cancellation,
- an edge selector so System One re-routes after node results,
- a policy layer: permission and side-effect approval are Rust, not judgment,
- a System Two planner that produces validated plans only,
- eval traces, shadow routing, replay, and offline benchmarks,
- one event-driven session runtime shared by the TUI, headless and editor
  clients,
- real workspace tools, reviewable patches and sandboxed command execution,
- revision-bound check evidence, persistent sessions and calibrated routing,
- optional language intelligence, bounded subagents and trusted MCP tools.

## Invariants

These hold across every client (TUI, headless JSONL, ACP) and every phase:

- **one engine.** TUI, JSONL and editor clients submit commands to one
  `SessionRuntime` and consume its events. No client implements its own
  execution loop.
- **a mandatory gate.** Every tool invocation passes through
  `ExecutionGate`: policy, approval and replay semantics live in Rust, and
  nothing a model controls can bypass them.
- **quality mode by default.** Substantive code reasoning stays on the
  configured reasoner; cheaper-generation routing is opt-in and must earn
  promotion through measured results.
- **real evidence, not assertions.** Completion requires checks that
  actually ran against the current revision. Valid JSON is not correct
  code, and a router's confidence is not a probability that code is right.
- **honest unknowns.** Unreported usage is unknown, not zero; a stale
  revision says so; an unavailable capability is reported, never silently
  substituted.

## Getting started

```console
$ cargo build --release
$ ./target/release/knut doctor          # what is configured, offline
$ ./target/release/knut doctor --live   # one real call per configured provider
$ ./target/release/knut tui             # the workbench shell
$ ./target/release/knut run "fix the failing test"   # one real coding task
```

Choose a reasoner provider independently from the optional decision model.

For OpenAI API models, including Codex models:

```sh
export KNUT_PROVIDER=openai
export OPENAI_API_KEY="your-api-key"
export KNUT_PROVIDER_MODEL=gpt-6.1-sol
./target/release/knut models
./target/release/knut doctor --live
./target/release/knut tui
```

For your ChatGPT plan, sign in through the system browser:

```sh
./target/release/knut login openai-codex
export KNUT_PROVIDER=openai-codex
./target/release/knut models
export KNUT_PROVIDER_MODEL=gpt-6.1-sol # choose an ID from the account's catalog
./target/release/knut tui
```

The OpenAI profiles use the native Responses API with `store: false` and
streaming. They preserve encrypted reasoning items and full input history,
validate completed responses, and report interrupted streams or usage-limit
failures. `gpt-5-codex` and other account-available Responses models can also be
selected with `KNUT_PROVIDER_MODEL`. Access depends on your API account or the
ChatGPT account's returned catalog.

ChatGPT sign-in uses OpenAI's documented public-client OAuth flow with PKCE,
state, nonce and signed ID-token validation. Knut stores separate registrations
under `~/.config/knut/` (`XDG_CONFIG_HOME` or `KNUT_CONFIG_DIR` can override the
location), with owner-only permissions and atomic token rotation. Refreshes
are serialized across processes. `knut accounts` lists saved registrations;
`knut accounts <client-id>` selects one; `knut login openai-codex --new` adds an
account. `knut logout openai-codex` revokes the selected session and clears its
local tokens. If remote revocation fails, it reports that explicitly. Sign-in
does not read Codex's credentials or grant access to ChatGPT conversation history.
Review or disconnect Knut under [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage).

See OpenAI's [models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference)
and [sign-in contract](https://developers.openai.com/siwc/token-sharing-open-source/sign-in).
API-key usage is metered; ChatGPT plan usage stays on the selected plan route.
Knut never falls back from plan usage to API billing.

Z.ai remains the default. Other Chat Completions providers can use
`KNUT_PROVIDER=chat-completions` with an explicit base URL and model.

| variable | purpose |
| --- | --- |
| `KNUT_PROVIDER` | `zai` (default), `openai`, `openai-codex`, or `chat-completions` |
| `KNUT_PROVIDER_API_KEY` | explicit API credential; otherwise `OPENAI_API_KEY` for OpenAI or `ZAI_API_KEY` for Z.ai/compatible endpoints |
| `KNUT_PROVIDER_BASE_URL` | endpoint prefix; defaults to Z.ai coding or `https://api.openai.com/v1` for OpenAI; ChatGPT tokens are restricted to the OpenAI origin |
| `KNUT_PROVIDER_MODEL` | model ID; defaults to `glm-5.3-flash` or `gpt-6.1-sol` for OpenAI |
| `KNUT_PROVIDER_REASONING_EFFORT` | explicit effort; GLM-5.3 uses `low`, `high`, `max`; GPT-6.1 Sol accepts `low`, `medium`, `high`, `xhigh`, `max` |
| `KNUT_PROVIDER_TIMEOUT_SECONDS` | positive request timeout; default 120 seconds |
| `TYPESAFE_API_KEY` | optional Jev credential; without it, the reasoner plans directly |
| `KNUT_MODE` | `quality` (default) or `adaptive` |

A decision model is optional. With only the reasoner configured, System Zero
hands tasks to the same validated planner, which chooses from the complete file
and command tool catalog. Tool policy, exact approvals, bounded repair and
revision-bound checks stay in the runtime. An unusable Jev configuration emits
a warning and uses this path; failures of a configured live Jev call remain
explicit errors.

`knut doctor` reports what is missing with actionable guidance, and makes
no paid request unless you pass `--live`.

To inspect generator calls during a real coding run:

```sh
./target/debug/knut run "fix the failing test" --census /tmp/knut-census.json
```

The output path must be new. The report records model/tier, call purpose,
buffered/streaming mode, outcome, latency, and reported token usage—even when the
run fails. It excludes prompt/response bodies and error text. Unknown usage stays
unknown. Jev calls, cached-token breakdowns and costs are not yet included.
This runs the normal coding task and can incur provider charges; `--census`
does not grant write approval. `--yes` pre-approves file edits; commands still
require their exact approval through TUI or JSONL.

Small edits can use `files/edit`: a read hash plus a JSON-encoded array of exact
`{old,new}` replacements. Ambiguous matches and stale revisions fail without
writing. A run gets at most three plan/execute/check attempts; a blocked approval
ends the scripted run, and successful existing tests cannot hide failed edits.

For a live repository-edit comparison with Ante, see
[the comparison runner](scripts/compare-harnesses.mjs). Prepare an empty temporary
directory, then run each harness against its isolated snapshot:

```sh
trial_dir=$(mktemp -d /var/tmp/knut-comparison.XXXXXX)
node scripts/compare-harnesses.mjs prepare "$trial_dir"
node scripts/compare-harnesses.mjs run "$trial_dir" knut high
node scripts/compare-harnesses.mjs run "$trial_dir" ante high
node scripts/compare-harnesses.mjs verify "$trial_dir" knut high
node scripts/compare-harnesses.mjs verify "$trial_dir" ante high
```

Requires Linux Bubblewrap, Node, Ante and a built `target/debug/knut`.
Live runs use credentials from the environment or local `.env` and may incur
provider charges. Both use GLM-5.3 Flash on the coding-plan endpoint with the
requested native effort. Prompts/tools differ by harness; this is a development
pilot, not proof of Jev savings. Logs and diffs can contain repository content.
The source repository is mounted read-only and masked inside the test sandbox;
edits happen only in the temporary copies. Compiler caches remain writable.

## Interactive CLI

Run `knut` in a terminal (or `knut tui`) to open a coding session. The main
screen is one conversation, a compact status line, and an input that grows with
your draft. Jobs, routing details, and changes open only when requested.

Type a task and press Enter. During a task, the next message goes through the
runtime's steering/queue handling. Approvals are shown above the input; sending
a message while an approval is pending preserves the draft.

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
| Ctrl+B | Open/close decision details |
| PgUp / PgDn | Scroll the conversation or current detail view |
| Tab / Shift+Tab | Switch input and conversation navigation |
| Esc | Close a detail view or return to the latest output |
| Alt+A / Alt+D | Allow / deny the exact pending approval |
| Ctrl+C | Close an overlay; otherwise stop work, clear an idle draft, or exit |
| Ctrl+D | Exit when idle with an empty draft |
| ? / F1 | Quick shortcut modal (`?` with empty input; F1 anytime) |

In changes, Left/Right selects a file, Up/Down selects a hunk, and PgUp/PgDn
scrolls it. The review is read-only; it does not pretend to revert applied edits.
Use the command menu for pause, resume, cancel, setup diagnostics, and checks.

The theme uses warm text, muted teal accents, and explicit state labels. It
adapts to truecolor, 256 colors, 16 colors, and `NO_COLOR`; configure
`KNUT_TUI_COLORS=truecolor|256|16|none` to override detection. Bracketed paste
does not submit text, and the terminal is restored on exit.

See [CLI_UX.md](CLI_UX.md) for the design and remaining usability work.

## Commands

```console
$ knut doctor [--live]      configuration, capabilities, and live checks
$ knut tui                  Ratatui workbench
$ knut run <prompt>         real provider + tools + sandboxed checks
$ knut jsonl [prompt]       headless: commands on stdin, events on stdout
$ knut verify [--json]      the workspace's real checks for this revision
$ knut bench                pilot benchmark with an inspectable report
$ knut sessions list|show|export|plan    stored sessions
$ knut lsp                  language-server availability
$ knut release              versioned artifact and checksum instructions
$ knut route|repl|demo-tree|eval         offline playground (mock, demo-only)
```

The playground commands are explicitly demo-only: they use a deterministic
mock router and canned tools, and their numbers are not measurements of
coding quality. `run`, `verify`, `bench` and `tui` use the real engine.

## Architecture

```
commands/events ──▶ SessionRuntime ──▶ events to TUI · JSONL · ACP
                        │
        ┌───────────────┼────────────────┬──────────────┐
        ▼               ▼                ▼              ▼
   System 0/1      Planner (S2)    ExecutionGate   CheckRunner
   (bounded)       validated plans  policy/approve  revision-bound
        │               │                │          evidence
        └───────────────┴────────────────┴──────────────┘
                        │
              provider adapters (GLM, DeepSeek) · workspace tools
              sandboxed processes · patches · SQLite session store
```

See the [provider compatibility matrix](src/matrix.rs) for what each
endpoint actually supports, with the evidence behind every claim.

## Alpha status and limitations

**Verified on 2026-09-21 at commit `9b5b3f9`** (plus the release work in
this commit), on Linux:

| check | evidence |
| --- | --- |
| `knut doctor` (offline) | reports missing configuration with actionable guidance |
| `knut doctor --live` | one real call each to Jev and the configured reasoner; both succeeded |
| provider adapters | GLM (`glm-5.3-flash`, coding endpoint) and DeepSeek (`deepseek-v4.1-flash`, Ollama Cloud) streamed, returned tool calls, reported usage and preserved reasoning parts |
| `knut verify` | ran build, 246 tests and clippy inside the OS sandbox and reported the real result |
| `knut bench` | wrote an inspectable report showing 1/6 tasks verified offline, with limitations stated |
| `knut tui` | launched, accepted input, streamed events, and restored the terminal on exit |
| `knut jsonl` | stdout stayed parseable JSONL with malformed input piped in |
| sandbox | bubblewrap denied a read outside the workspace and unreachable network |
| session store | created owner-only (`0600`) and refused a corrupt file without resetting it |

**Known limitations, stated rather than implied:**

- TUI, `run`, and JSONL now drive the same session runtime. It performs
  real reads/edits, resumes exact approved plans, and runs repository checks
  against the edited revision. Failed checks and tool failures have a bounded
  repair budget. This integration is regression-tested; the earlier measured
  Ante pilot used the previous standalone CLI loop.
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

## Example

```rust
use knut::{
    Action, Decision, ModelTier, Risk, Route, StaticSystemOne, Knut,
};

# async fn demo() -> Result<(), knut::KnutError> {
let system_one = StaticSystemOne::new(Decision {
    route: Route::Generate,
    confidence: 0.94,
    retrieval: None,
    capability: None,
    model_tier: ModelTier::Fast,
    risk: Risk::Low,
    parallelizable: false,
});

let knut = Knut::new(system_one).with_confidence_floor(0.75);
let action = knut.next("Explain this error", vec![]).await?;

assert_eq!(action, Action::Generate(ModelTier::Fast));
# Ok(())
# }
```

## Design rule

If Knut can write the valid `match` arms before asking the model, System One is a candidate. If the output space cannot be bounded ahead of time, hand the work to a generative model.

The interesting experiment is not merely `Jev -> LLM`. It is a loop:

```text
System One -> cheap action -> System One verifies/routes
                              |
                              +-- confident -> continue
                              `-- uncertain -> stronger model
```

That makes fast judgment part of the runtime rather than a one-time front door.

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

Configuration is environment-based and nothing is written to the
repository:

| variable | purpose |
| --- | --- |
| `KNUT_PROVIDER_API_KEY` | reasoner credential (required for `run`) |
| `KNUT_PROVIDER_BASE_URL` | endpoint; defaults to the Z.ai coding endpoint |
| `KNUT_PROVIDER_MODEL` | model id; defaults to `glm-5.3-flash` |
| `TYPESAFE_API_KEY` | Jev credential (optional; decisions fall back to deterministic rules) |
| `KNUT_MODE` | `quality` (default) or `adaptive` |

`knut doctor` reports what is missing with actionable guidance, and makes
no paid request unless you pass `--live`.

## The workbench (`knut tui`)

`knut tui` opens the shell over the same `SessionRuntime` that `knut run`
uses: the real provider, the real workspace tools, the same approval gate
and the same revision-bound checks. Nothing in the UI is a simulation of
work; the shell renders what the runtime published and nothing else.

```text
 ──────────────────────────────────────────────────────────────
  ⟠ knut  ~/Work/Knut  ▏ main                        01:24
  ▸ running ▏ quality  ▏ glm-5.3-flash  ▏ api.z.ai  ▏ 3 checks
 ╭─ transcript ─────────────────────────╮╭─ jobs ────────────────╮
 │ ❯ fix the failing test in src/lib.rs ││ active                │
 │ ▏ ▸ reading src/lib.rs               ││  ⠹ run cargo test     │
 │   ✔ read: ok {"lines": 42}           ││ recent                │
 │ ◆ system one [recovery v1]: verify   ││  ✔ read       120ms   │
 ╰──────────────────────────────────────╯╰───────────────────────╯
 ╭─ enter submit | ctrl+j newline | : commands ─────────────────╮
 │ ❯ █                                                          │
 ╰──────────────────────────────────────────────────────────────╯
```

What is deliberate:

- **Degrade, never lie.** Colour is detected (`COLORTERM`, `TERM`,
  `NO_COLOR`, or an explicit `KNUT_TUI_COLORS=truecolor|256|16|none`).
  A 256-colour terminal gets the nearest cube entry, a 16-colour terminal
  gets the nearest classic colour, and a terminal with no colour gets
  weight and glyph contrast instead. Status is always carried by a word
  *and* a glyph, so nothing becomes unreadable when the palette collapses.
- **The transcript is a rail, not a wall.** Each entry kind has its own
  glyph and colour, and a streaming turn shows a live cursor block, so a
  stalled stream and a live one look different.
- **Jobs are honest.** A card that stopped says *how*: cancelled, timed
  out and failed are distinct words, and a narrow pane drops the summary
  before it drops the state.
- **Nothing runs on the keystroke path.** Checks run on a worker and come
  back as messages; a slow provider cannot block typing, scrolling or the
  decision inspector.

| key | action |
| --- | --- |
| `Enter` | submit the composed task |
| `Ctrl+J` | newline in the composer |
| `Tab` | cycle focus (composer → transcript → state) |
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | scroll the transcript |
| `a` / `d` | approve or deny a gated action |
| `c` / `p` / `r` | cancel / pause / resume the running task |
| `v` | review: recorded changes and check evidence |
| `V` | run the workspace's real checks now |
| `i` | decision inspector (System 0/1 provenance) |
| `:` | command palette (every entry states whether it exists) |
| `1` `2` `3` | transcript / state / jobs on narrow terminals |
| `F1` / `?` | help |
| `q` / `Ctrl+C` | quit, restoring the terminal |

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

- `knut run` wires the real provider, workspace tools, gate, sandbox and
  check runner, and executes a validated plan. On the fixture used here,
  the model's plan asked to pass a *search result* as a `read` node's
  `path`, which the plan language cannot express: `$ref` substitutes a
  whole value, and a search result is not a path string. The node failed
  and the task honestly reported "not verified" instead of claiming
  success. Extracting a typed field out of a referenced artifact is
  future work; until then, `run` is a real harness that may not
  autonomously complete every task, and never pretends otherwise.
- `bench` is a pilot: six fixture tasks, all offline. It is not the
  30-task held-out evaluation the roadmap calls for.
- Language servers are reported but never started implicitly; LSP
  navigation is fixture-tested, not live-verified here.
- macOS and Windows are not verified. Do not treat command isolation as
  working there.
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

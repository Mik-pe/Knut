# Harness architecture and evaluation

[Back to the README](../README.md)

## Prototype goals

- Keep control flow explicit and testable.
- Use System One for bounded selection and failure triage.
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
- a native model/tool conversation loop with cooperative cancellation,
- task-specific tools, instructions, context and completion requirements,
- a policy layer: permission and side-effect approval are Rust, not judgment,
- generic context providers and revision-bound completion monitors,
- validated planner/tree utilities for the offline playground,
- eval traces, shadow routing, replay, and offline benchmarks,
- one event-driven session runtime shared by the TUI, headless and editor
  clients,
- real workspace tools, reviewable patches and sandboxed command execution,
- revision-bound check evidence, persistent sessions and calibrated routing,
- optional language intelligence, bounded subagents and trusted MCP tools.

## Invariants

These hold across the TUI and headless clients, and host integrations:

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

## General tasks and embedding

The CLI opens the current directory for file and sandboxed command tools.
`auto` enables coding checks when a supported project manifest is present and
otherwise uses the general profile. Select `general` explicitly for document
work or conversation inside a repository; `coding` requires configured checks.
Coding checks run when project content changes. Questions that leave the project
unchanged can complete without a build; explicit task requirements still apply.

```sh
KNUT_PROFILE=general knut run "Summarize notes.txt"
KNUT_PROFILE=general knut tui
```

Hosts can set `KNUT_LOAD_ENV=0` to skip loading the workspace `.env` file and
supply an isolated environment to the child process.

JSONL protocol version 2 uses explicit `queue`, `steer`, `update_queued`,
`remove_queued` and `run_queued` commands. Queue events carry stable request IDs.
Clients must consume version 2. Submit, queue and update commands accept optional
task configuration:

```json
{"type":"submit","prompt":"Summarize the report","options":{"tools":["read"],"instructions":"Be concise.","context":[{"source":{"uri":"document:report","revision":"v1"},"description":"Report excerpt","content":{"text":"Quarterly results…"}}]}}
```

`tools` restricts registered tool IDs; an empty list permits no tools. Omit it to
use the configured catalog. Instructions and context stay with queued tasks.
`requirements` accepts an array of `{check, description, blocking}` records and
adds to the profile's completion contract. Required evidence cannot be waived
by the model. Task configuration is bounded to 128 KiB.

For embedding, `HarnessSetup` supplies tools, instructions, an optional
`ContextProvider` and an optional `CompletionMonitor`. `build_harness` builds the
same engine without opening a workspace. `SessionRuntime::new` also accepts an
injected model cascade for hosts that manage providers themselves. The CLI adds
an `AskUserTool`; its answer returns through the tool-result continuation.

Context records identify snapshots with a resource URI and revision. Supplied
URL/document records do not fetch or establish freshness of external resources;
that belongs to the configured provider. Workspace context performs gated reads
and retains their content hashes. Decision-model priorities reorder context and
tool schemas while retaining the full evidence/catalog. They are advisory; no
cost or quality improvement is claimed without measurements.

## Architecture

```text
commands/events -> SessionRuntime -> TUI / JSONL / editor events
                         |
                 model/tool conversation
                         |
          ExecutionGate + CompletionMonitor + ContextProvider
                         |
               configured tools and provider adapters

workspace profile: files / sandboxed commands / repository instructions
coding profile:    workspace tools + revision-bound build/test/lint evidence
custom setup:      application tools + context + completion contract
```

See the [provider compatibility matrix](../crates/knut-runtime/src/matrix.rs) for what each
endpoint actually supports, with the evidence behind every claim.

## Runtime optimizations

Tool function names are indexed when a registry is built. Dispatch looks up one
metadata entry in the task's restricted registry, then invokes the normal gate;
it does not rebuild or scan the full catalog for each call. Selecting task tools
rebuilds the index, so excluded tools remain unavailable.

Context reads still run on every model turn. A confident context priority is
reused only while the full ordered records (URI, revision, description and
content) are identical. Changed records, steering and new tasks invalidate it.
Errors and uncertain decisions are never cached. All context remains available
to the reasoner. The offline regression reduces three context-selection calls
to one for three unchanged turns; it does not measure live latency or model
quality.

## Inspecting real runs

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
writing. A task is bounded to 32 runtime turns and 16 tool calls per model response,
with two completion-repair attempts. A blocked approval
ends the scripted run, and successful existing tests cannot hide failed edits.

For a live repository-edit comparison with Ante, see
[the comparison runner](../scripts/compare-harnesses.mjs). Prepare an empty temporary
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

## Offline routing example

This demonstrates a single routing decision with a static backend. Real tasks
use `SessionRuntime` and the model/tool loop described above.

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

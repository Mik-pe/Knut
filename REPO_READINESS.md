# Repository readiness audit

Audited 2026-09-22 against the current dirty working tree. Existing changes were
preserved. The findings below record the initial source audit. Implementation progress
is recorded separately; historical pilot results are not a new benchmark.

## Goal and acceptance

Open an actual repository in Knut, submit a coding task, inspect real tool calls,
approve edits, obtain an independently verified patch, and continue working.
The TUI, `run`, and JSONL must use one execution path and permission gate.

Acceptance requires:

- Discover repository instructions and applicable checks; explain missing setup.
- Complete a single-file fix and a multi-file change using real reads and edits.
- Display progress during provider/tool activity; cancellation interrupts active
  work and terminates descendants, without dispatching more writes.
- Show exact approval scope, changed files, check results, and unresolved work.
- Recover from a failed check and reject stale edits; preserve unrelated changes.
- Pass independent task-specific checks against the final revision. Existing
  passing tests or a generated answer alone cannot establish task success.
- Exercise the same tasks through interactive and headless entry points.

## Findings, in implementation order

| Priority | Finding and evidence | Required change |
| --- | --- | --- |
| P0 | `src/main.rs:722`: JSONL's prompt path always emits a configuration failure, even when configured; stdin commands never drive the real engine. `coding_run` separately plans, executes, and checks. | Replace both paths with clients of the shared runtime; retain the working CLI repair/check behavior in that runtime. Remove the superseded loop. |
| P0 | `src/engine.rs:467`: the driver awaits the whole command before reading another. `src/session.rs:1784`: generation buffers stream events until the provider finishes. | Deliver events as they happen and accept cancellation while work is pending. Test with a deliberately stalled provider and a running child process. |
| P0 | `src/engine.rs:377`: event forwarding uses retained-log length as its cursor. `src/session.rs:417`: the bounded log evicts entries; once full, length stops increasing. | Separate live delivery from retained history or use stable event identities. Verify delivery through log rollover, including terminal and approval events. |
| P0 | `src/engine.rs:303`: without Jev, the interactive router always chooses text generation. Generation only surfaces tool-call proposals and then waits for check evidence. | Make repository tasks reach executable tools without requiring the optional router. Separate ordinary chat completion from coding completion. |
| P0 | `discover_instructions` has no production caller (`src/workspace.rs:899`). | Load applicable instructions into planning and generation, respecting directory scope and context limits. Test delivery, not just discovery. |
| P0 | Engine setup, on-demand checks, and `coding_run` use `CheckProfile::rust()`, while `verify` discovers profiles. Node discovery assumes TypeScript and npm from marker files. | Share repository check configuration across clients; inspect scripts/toolchains and report unsupported or missing checks explicitly. |
| P1 | `src/headless.rs:108` appends a newline to request queuing, but `src/session.rs:1894` trims it before classifying input. A short queued message can steer instead. | Represent queue and steer explicitly in the command contract and UI; do not encode intent in prompt formatting. |
| P1 | `src/composer.rs:95` excludes line separators from character counts; `insert_newline` does not enforce the character budget. | Land a reviewed fix with boundary/Unicode/undo tests. The successful pilot patches are still isolated artifacts, not a source-tree fix. |
| P1 | README claims a shared engine and still describes typed artifact projections as future work, despite current implementation and the pilot. | Align documentation with verified entry-point behavior after integration. |

Start with execution-path integration, then live events/cancellation and repo
context/checks. UI polish comes after a real interactive task passes these gates.
Existing visual work remains in [CLI_UX.md](CLI_UX.md).

## Next comparison protocol

1. Freeze source snapshots, harness binaries/versions, prompts, checks, provider,
   model, reasoning effort, permissions, and run budgets. Record actual request
   settings; equal tier labels alone are insufficient.
2. Run paired Knut/Ante tasks at each supported effort separately (`low`, `high`,
   `max` for this configured model). Keep each workspace and build target isolated.
   Match cache policy, alternate run order, and avoid concurrent builds.
3. Begin with the acceptance tasks above; then expand to at least 30 held-out
   tasks across repositories. Repeat pairs and retain all failures/timeouts.
   Keep hidden checks and expected patches outside model-visible snapshots.
4. Judge final diffs and independent checks: success, regressions, scope control,
   instruction compliance, and useful failure explanations. Record time to first
   useful action, time to verified patch, tools, retries, and available usage.
5. Capture cached tokens and Jev usage before reporting cost per success. Unknown
   values remain unknown. Report paired results and uncertainty, not only means.
6. Separately compare Knut with Jev enabled/disabled in the same engine. Ante
   comparisons assess the whole product; they cannot isolate Jev's contribution.

The existing runner is a one-task pilot. Extend it with task manifests and
per-task independent verifiers after the shared runtime works; do not present
repeated runs of the already inspected composer bug as held-out evidence.

## Integration pass

The standalone CLI execution loop was removed. `run`, JSONL, and the TUI now
submit commands to the engine driver. The session runtime owns repository
checks, bounded repair, and approval resumption with retained tool outputs.
Instructions reach planning and generation; checks use a shared workspace
profile. Verification rejects evidence when checks changed the source revision.

Live events now use a separate delivery channel, so retained-history eviction
cannot stall the UI. Streaming fragments reach clients during provider calls;
cancellation drops the pending call. Dropping a supervised command future also
signals its process group to stop.

Final working-tree validation passed: 607 library tests, 2 CLI tests, clippy
with warnings denied, formatting, and build. Regressions cover real
read/generate/write execution, exact approval previews and resumption,
instruction delivery, failed-check repair, stale check revisions, history
rollover, and stalled-stream cancellation. The obsolete headless dispatch stub
was replaced by the adapter used by the executable; outcomes come from the runtime.

Process smokes passed through the rebuilt TUI and JSONL with a controlled local
provider: two files changed, two exact approvals, native build/test/lint, and
independent tests. Evidence is under
`/var/tmp/knut-runtime-smoke-20260922-d`. A separate cancellation smoke observed
both the supervised check's shell and its child stop within the first 25 ms
observation, with a runtime cancellation event. Evidence is under
`/var/tmp/knut-cancel-check-CpymKy` and its driver is
`/var/tmp/knut-runtime-cancel-20260922.mjs`.

A real provider exposed a generation route that returned tool-shaped text
instead of acting. That failure is retained in the comparison ledger; repository
generation now executes a tool plan and has a regression test.
The corrected shared-runtime live run succeeded, and independent verification
passed 607 library tests, 2 CLI tests, and 4 hidden regressions.
The verified Knut-generated composer patch is now
applied to this working tree.

That pass completed the end-to-end repository milestone. The next pass below
addresses queue/steer semantics and source grounding; richer project setup,
complete usage accounting and held-out evaluation remain open.

## Harness and interaction pass

Planning now receives bounded source excerpts for up to three explicitly named
files, obtained through the execution gate. Literal tool arguments and typed
references are checked before execution. A rejected plan gets one repair with
the actual rejected output and precise errors. Generated nodes receive the
original task, repository instructions and latest repair evidence directly.

Recovery retains diagnostic locations, causes and tails instead of only log
prefixes. Jev receives actual failure evidence and batches classification with
diagnostic prioritization. Priority is advisory: all failures remain available,
and code still enforces permissions, retry budgets and fresh passing checks.
The default model is pinned to `jev-1.13.0`; recovery's confidence floor remains
an uncalibrated abstention heuristic.

The opt-in live recovery smoke also passed against `jev-1.13.0`: one request
classified the compiler/test failures as verification failures and prioritized
the compiler diagnostic over the downstream test failure. Both answers validated
against the submitted question pack. This establishes API compatibility for one
example, not calibration. Output: `/var/tmp/knut-jev-recovery-live.log`.

Session/JSONL protocol version 2 removes implicit steer-or-queue classification.
The runtime owns queued requests, editing, removal and dispatch. Completion
advances the queue; cancellation and failure hold it. Steering preserves the
original goal and interrupts a stalled provider. TUI queue editing preserves
the previous draft through acknowledgement. Approval previews retain complete
arguments, show replacement text before the identity and support scrolling.
File/check summaries and Unicode header truncation were corrected.

Validation: 625 library tests and 2 CLI tests, formatting, clippy with warnings
denied, and build pass. Tests use `--test-threads=2`: the initial unrestricted
run hit the existing synthetic latency assertion on the busy host; the assertion
was not relaxed. `scripts/smoke-harness.mjs` passed through JSONL with deliberate
invalid arguments, a failing first patch, real repair/checks, two exact approvals,
queue editing/removal and two completed tasks. Independent tests preserved the
original assertion. Evidence: `/var/tmp/knut-harness-smoke-whNmxQ`.

Direct tuistory terminal sessions passed at 60x18, 80x24 and 140x40: submit,
scroll/resize exact approvals, repair, complete and exit while preserving the
unfinished draft. Snapshots: `/var/tmp/knut-ui-review-J2UnP7`; workspace:
`/var/tmp/knut-harness-smoke-qOnUL0`.

### Retained live development trials

All three used GLM-5.3 Flash at low effort, a 45-second provider request timeout,
pinned Jev, and an isolated tiny Rust workspace. The task was to make `value()`
return seven while preserving the existing test and public API. Writes were
pre-approved in these disposable fixtures. Every attempt is retained.

| Attempt | Outcome | Evidence |
| --- | --- | --- |
| Before named-source reads | Failed: guessed replacement text did not match; subsequent planning/repair omitted required fields. No edit; independent test failed. | `/var/tmp/knut-live-harness-tf0jklmv` |
| With named-source reads, flawed trial logging | Correct edit and passing checks, but no verified completion. The trial wrote its live log inside the checked workspace, changing the revision during verification. Excluded from quality comparisons. | `/var/tmp/knut-live-harness-_wldkivp` |
| With logs outside the workspace | Verified completion; exact expected edit, unchanged test and independent test passed. One generator call, 1,653 input / 334 output tokens; 12.035 seconds for the run. | `/var/tmp/knut-live-review-x7rrs7cb` |

These are iterative development probes of one inspected task, not a paired or
held-out benchmark. The census excludes Jev usage, cached-token breakdown and
cost. Neither a success-rate improvement nor Jev's contribution is established.

Next validation should compare matched engine policies over held-out tasks.
Full log inspection, file attachments, durable draft/session resume and
richer completion summaries remain UX work. Queue acknowledgements now
preempt a quiet, abandon-safe tick; completion summaries carry check
evidence for the exact revision. See
[CLI_UX.md](CLI_UX.md) and the local `TASKS.md` for the updated backlog.

## Knot identity and editor polish

The unused static knot module was replaced with a bounded, depth-shaded trefoil
renderer, shared by the welcome and header. Its travelling highlight has a
2.4-second opening sequence and follows active work. Waiting, pause and terminal
states are static; reduced motion is available from `/motion` or
`KNUT_TUI_MOTION=off`. Animation follows elapsed time rather than input frequency,
and settled idle screens no longer redraw continuously. Explicit color overrides
now reach Crossterm even when `NO_COLOR` is inherited.

The composer now keeps a multiline paste as one undoable operation, preserves
graphemes at the size boundary, normalizes CRLF, and reports truncation. History
browsing restores the unfinished draft and cursor. Word navigation/deletion,
scrollable help and narrow-screen hints are connected to the live shell.

Final validation passed 621 library tests and 2 CLI tests, formatting, clippy
with warnings denied, and build. Obsolete static-art fixtures were removed with
the old renderer. `scripts/smoke-tui.mjs --screenshots` passed 30 terminal
observations with a local provider, including 30x12 through 140x40, truecolor
with inherited `NO_COLOR`, ASCII/reduced motion, word editing, help scrolling,
two approvals, a failed first patch, repair and draft-preserving completion.
Evidence: `/tmp/knut-tui-smoke-jYWom3`.

The screenshot and animated opening capture in `assets/knut-terminal.*` come
from the actual rebuilt terminal UI. The capture contains 25 distinct rendered
frames. The preview GIF loops; the live welcome settles after its introduction.

## Workspace input memory

The TUI now restores drafts, grapheme cursor positions and bounded prompt
history per canonical workspace. The existing SQLite store migrates to schema 2;
background saves coalesce every 400 ms and normal exit flushes the latest input.
Ctrl+Q keeps the draft when leaving an idle session. History browsing and queue
editing preserve the underlying draft. Conflicting terminal writes and unreadable
storage report errors. This does not resume tasks, queue edits or approvals.

Validation passed: 632 library tests, 2 CLI tests, formatting, clippy with warnings
denied and build. Regressions cover Unicode cursors, bounded history, workspace
isolation and aliases, conflicting saves, corrupt input, schema migration and
flushing the latest edit. The terminal smoke passed 35 observations, including
restart, restored-cursor insertion and prompt-history recall. Evidence:
`/tmp/knut-tui-smoke-Zm9z44`. The smoke uses a local provider fixture.

## Integrated connection and interaction changes

Resolved the autostash conflicts against the connection-settings work. Settings,
ChatGPT plan labels and model-change guards remain available alongside explicit
queue/steer commands, recovery evidence, exact approvals and workspace input
memory. Queue requests made before a provider stalls now receive acknowledgements
after the quiet-period deadline without requiring another keystroke. The
regression sends the request immediately after the first streamed fragment.

Validation passed: formatting, clippy with warnings denied, 665 library tests,
2 CLI tests, 2 provider-settings integration tests and build. The JSONL smoke
passed two exact approvals and two completed tasks with independent checks.
The terminal smoke passed 38 observations, including F2 settings with draft
preservation, restart recovery and repair. Both smoke scripts isolate saved
connection settings so they always use their local fixture provider.

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
Existing visual/backlog work remains in [CLI_UX.md](CLI_UX.md) and [TASKS.md](TASKS.md).

## Comparison baseline already available

Inspected saved `result.json`, `verification.json`, verification logs, and Knut's
census under `/var/tmp/knut-comparison.sWxwYR`. These are historical results;
this audit did not rerun the harnesses. The detailed ledger is
[HARNESS_COMPARISON.md](HARNESS_COMPARISON.md).

| Recorded measurement | Knut | Ante |
| --- | ---: | ---: |
| Model / reasoning effort | GLM-5.3 Flash / low | GLM-5.3 Flash / low |
| Independent verification | Passed, including 4 hidden tests | Passed, including 4 hidden tests |
| Changed scope | `src/composer.rs` | `src/composer.rs` |
| Native elapsed time | 493.117 s | 594.048 s |
| Reported input / output tokens | 18,635 / 3,569 | 174,156 / 2,566 (pilot report) |

Both produced working patches. The report records broader regression coverage
in Ante's patch. Cache accounting, Jev usage, build-cache conditions, and native
sampling settings differ or are incomplete; these results establish neither a
cost advantage nor a general quality/latency advantage. They also do not validate
the TUI or prove Jev adds value.

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
passed 607 library tests, 2 CLI tests, and 4 hidden regressions. The matched Ante
run timed out; its production fix passed the hidden cases but an added Unicode
test failed independently. The verified Knut-generated composer patch is now
applied to this working tree. See HARNESS_COMPARISON.md for exact versions,
accounting, and limitations.

The end-to-end repository milestone is complete. Queue/steer semantics, richer
project setup, eliminating path-only model calls, complete usage accounting,
and held-out multi-repository evaluation remain follow-up work.

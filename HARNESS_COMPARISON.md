# Live repository comparison

Goal: Knut must make real, tool-mediated repository edits that pass independent
regression checks, then be compared with Ante at the same model and native
reasoning effort. A passing pre-existing suite alone does not establish success.

## Latest matched run — 2026-09-22

Both arms used the same frozen Knut repository snapshot, prompt, GLM-5.3 Flash
model, coding-plan endpoint, and native `low` reasoning effort. Runs were
sequential, with separate workspaces and target directories and a 900-second
outer limit. This is a repeated development task, not a held-out evaluation.

| Measurement | Knut shared runtime | Ante 0.2.1 |
| --- | ---: | ---: |
| Native outcome | Verified on first execution attempt | Timed out |
| Native wall time | 461.394 s | 900.057 s (limit) |
| Same four hidden regressions | 4 passed | 4 passed |
| Additional verification | 607 library + 2 CLI tests passed | Added Unicode regression failed: actual 11, expected 9 |
| Changed scope | `src/composer.rs` only | `src/composer.rs` only |
| Added tests | 5 | 4 |
| Reported input / output tokens | 29,377 / 2,142 | 93,532 / 1,729 |
| Reported cached input | Unknown | 78,080, included in input |
| Generator accounting | 5 census calls | 7 usage events |
| Tool calls | 7: 3 searches, 2 reads, 2 edits | 8: 1 read, 3 edits, 4 shell calls |

Both production fixes satisfy the hidden character-budget cases. Knut produced
an independently verified patch. Ante's added tests included two expensive
per-character 200,000-character setup loops; process inspection showed two test
runs active concurrently after the first tool wait expired. It did not finish
within the outer limit, and a separately executed new Unicode test failed.
No model-produced patch was manually repaired or copied between arms.

Knut's independent full-suite verification took 31.292 seconds. For Ante, the
same hidden tests passed in 49.950 seconds, then its exact Unicode regression
failed in 3.773 seconds. Its full suite was not rerun after the timeout: the
independent failing regression already rejects the patch. This is not a claim
that all remaining Ante tests were evaluated.

Knut spent 41.887 seconds in generator calls and 402.536 seconds in native
build/test/lint. Two generation calls only returned file paths, consuming 7,264
input tokens and 3.158 seconds. Typed path selection and verification setup are
concrete optimization targets. Cached-token and Jev accounting are incomplete;
these numbers do not establish lower cost. One task also cannot establish a
general quality or latency advantage, or a causal benefit from Jev.

Artifacts, frozen manifest, both patches, logs and verification records:
`/var/tmp/knut-shared-runtime-v2.8mT6H9`. Snapshot SHA-256:
`f3a7cae9afc852d7c2f0c2233c15fc37d5d7e87d6112d1f0148d25afb2599662`.
Ante's independent diagnostic driver is preserved as `verify-ante.py` there.
After verification, Knut's patch was applied to the working source. Later UI
changes improve approval previews and remove stale status indicators; the
comparison measures the frozen binary, not those subsequent presentation edits
or the final headless adapter cleanup. Final working-tree validation passed
607 library tests, 2 CLI tests, clippy, build, and TUI/JSONL process smokes;
see REPO_READINESS.md for the separate cancellation evidence.

## Earlier completed pilot — 2026-09-21

Both harnesses fixed the same bug using real tools, preserved existing tests,
and passed independent full-suite verification including four externally added
regressions. Both changed only `src/composer.rs`. Neither generated patch was
manually corrected or copied into the other arm.

| Measurement | Knut | Ante 0.2.1 |
| --- | ---: | ---: |
| Model / effort | GLM-5.3 Flash / low | GLM-5.3 Flash / low |
| Independent tests passed | 599 library + 2 CLI + 4 hidden | 601 library + 2 CLI + 4 hidden |
| Reported input tokens | 18,635 | 174,156 |
| Reported output tokens | 3,569 | 2,566 |
| Reported cached input | Unknown | 157,184 |
| Observed generator accounting | 3 census calls | 12 usage events |
| Tool calls | 5: 2 searches, 2 reads, 1 edit | 13: 1 read, 5 edits, 7 shell calls |
| Native run wall time | 493.117 s | 594.048 s |
| Independent verification wall time | 30.708 s | 23.931 s |

Knut's generator calls took 55.853 seconds: planning 15.023 s, planning repair
8.520 s, patch generation 32.310 s. It completed the first execution attempt;
the new execution/check repair budget was not needed in this successful run.
Ante's numbers sum its `UsageUpdate.usage` records; these are not a network-level
request census and cannot prove the absence of unreported retries. Cached input
is part of its reported input, not extra tokens to add again. Knut omits cache
breakdowns and Jev usage, so **these numbers do not establish lower cost**.

Output quality: both use the same non-allocating line-character sum plus separator
count, and reject a newline before changing the buffer at capacity. Knut added
four tests; Ante added six, including explicit rejected-typing/undo coverage.
No existing tests were removed or weakened. Knut generated more explanatory
comments. Ante investigated two flaky/environment-sensitive existing tests and
reported that limitation instead of changing unrelated code. Both patches passed
the corrected independent verifier after native runs finished.

The source snapshot SHA-256 was
`d55badeda150bb71e00662257083f9c1992fee07a82a6773abb3eb5c447938db`.
The source commit was `b823670f5c378726c28f59bba6f60df9cdc83f9b` plus the captured
dirty changes. Raw manifest, logs, patches, census and verification records are
under `/var/tmp/knut-comparison.sWxwYR/{knut-low,ante-low}`.

This establishes a working real-repository pilot, **not a general harness win or
a causal Jev benefit**. Compared with the earlier failed Knut full-file run,
exact edits used fewer reported tokens and less generation time, but this is an
unreplicated development observation across changed harness versions.

## Protocol

`scripts/compare-harnesses.mjs` captures the dirty working source once, hashes it,
and creates separate Git repositories from that snapshot. Both receive the same
composer character-budget task. The original working checkout is masked in the
sandbox. Hidden regression tests are injected only after each harness finishes.

- Provider: Z.ai coding-plan endpoint, `glm-5.3-flash`.
- Effort: explicitly `low`, `high`, or `max`; never equate different providers'
  similarly named levels. Ante's isolated catalog is checked before starting.
- Ante: installed 0.2.1, bare profile, skills and auto-memory disabled, native
  Read/Write/Edit/Glob/Grep/Bash tools; explicit write permission inside the copy.
- Knut: native `run --yes`, file tools, revision-bound Rust checks, Jev configured
  when its credential is available, generator census exported.
- Fifteen-minute outer limit per run; Knut request timeout 300 seconds.
- Record complete output, diff, exit status, wall time, available usage, and
  external check result. Missing usage is unknown, not zero.

This is a development pilot, not a held-out evaluation or a Jev ablation. Native
tools, prompts, checking strategies and sampling defaults differ. Ante explicitly
sends temperature 1, top_p 0.95 and max_tokens 131072; Knut currently leaves these
to provider defaults. Machine load and compiler caches affect wall time. Do not
infer a causal Jev benefit from this comparison. The earlier pilot used a separate CLI loop; the latest run above uses the shared
engine also exercised by TUI and JSONL process smokes.

## Attempts retained — 2026-09-21

1. Initial Knut `high` request timed out at the old 120-second limit, before any
   tool ran. Ante preflight fell back to the metered endpoint because the isolated
   catalog lacked the coding-plan provider; it was stopped and excluded.
2. Pinned-endpoint `high` pilot: Knut executed search, then failed to resolve a
   path because the plan declared a selected string as JSON. Its planner also
   guessed `contents` instead of the write tool's `content`: tool schemas were
   missing from planning context. Ante read files and made an initial composer
   edit. A per-user tmpfs quota interrupted logs/checks; the pilot was stopped.
   No verified success, complete latency or complete token total is claimed.

Local preserved artifacts:

- `/home/mikpe/.cache/knut-evals/preflight-QGJ1Z4`
- `/home/mikpe/.cache/knut-evals/incomplete-high-7MEXEQ`

These contain incomplete raw logs and must not be treated as finished results.
The runner now rejects tmpfs artifact roots; use disk-backed `/var/tmp`.

Fixes prompted by these attempts: include exact tool schemas in plans, document
typed field projections and bounded reads, expose leaf failures, make request
timeouts configurable, pin provider/effort explicitly, and require successful
execution as well as fresh checks before reporting success.

3. First disk-backed `low` pilot, `/var/tmp/knut-comparison.IQ5s3y`: Knut made a
   real source edit, but a second write reused the original hash and was safely
   refused. Native checks reported build/lint passing and 595 tests passing,
   2 failing. Its first external verification is invalid: the verifier shared a
   Cargo target directory with Ante and could reuse the other arm's binary.
   Three generator calls reported 33,664 input
   and 12,840 output tokens; generation took 159,627 ms and the run 611,925 ms.
   Jev returned `continue` despite incomplete execution/checks; it did not
   override the completion gate. Ante added a per-character 200,000-character
   fixture and timed out at the 900-second outer limit.

The improved matched `low` snapshot is `/var/tmp/knut-comparison.sWxwYR`. It
includes the exact-edit tool and bounded failure repair. Knut applied one exact
edit and added four tests. Native build/test/lint passed. A first external
verification was invalidated for the same shared-target problem: it reported
Ante's 601 library tests instead of Knut's 599. Raw invalid results are retained
as `verification.invalid-shared-target.*`. The corrected verifier uses each
workspace's own target directory. Knut then passed 599 library + 2 CLI + all 4
hidden tests; Ante passed 601 library + 2 CLI + all 4 hidden tests, both with
changes limited to `src/composer.rs`.

Concurrent cold builds caused a working-checkout render timing test to exceed
its 50 ms budget; it passes in isolated runs. Raw wall times from these pilots
are not controlled latency measurements. Native cache policies also differed:
Knut's nested check sandbox used its own target directory while Ante used the
pilot's shared build directory. The corrected external verifications ran
sequentially after both harnesses finished, with per-workspace target directories.
Future comparisons should use matched warm/cold cache policies and no concurrent
builds. No general quality, cost or latency advantage has been established.

Provider/harness references: [Z.ai thinking controls](https://docs.z.ai/guides/capabilities/thinking),
[Z.ai request contract](https://docs.z.ai/api-reference/llm/chat-completion),
[Ante headless mode](https://docs.antigma.ai/usage/headless/).

## Shared runtime integration — 2026-09-22

The TUI, JSONL and `run` now use one engine driver. Controlled-provider process
smokes under `/var/tmp/knut-runtime-smoke-20260922-c` exercised two real file
writes, two exact approvals, native build/test/lint and independent tests through
both TUI and JSONL. This isolates client/runtime behavior; it is not a live-model
quality benchmark. The provider asserted repository-instruction delivery.

The first live run of this integration is retained at
`/var/tmp/knut-shared-runtime-comparison.YEJw6z/knut-low`. At GLM-5.3 Flash / low,
Jev's generation route reached a text-only request. The model returned a textual
shell invocation; no tool executed and the run correctly exited unverified.
One generation call reported 259 input / 43 output tokens; native elapsed time
was 5.839 seconds. This is an unpaired development failure, not a completed
comparison. It exposed the need to route repository generation through executable
plans. A regression now covers that route as well as explicit tool routing.

The corrected integration snapshot is
`/var/tmp/knut-shared-runtime-v2.8mT6H9`, SHA-256
`f3a7cae9afc852d7c2f0c2233c15fc37d5d7e87d6112d1f0148d25afb2599662`.
Knut's native run passed build/test/lint on its first execution attempt in
461.394 seconds, changing only `src/composer.rs` and adding five tests. It used
seven file-tool calls (three searches, two reads, two edits). Five generator
calls reported 29,377 input and 2,142 output tokens, taking 41.887 seconds total.
Two of those calls emitted only file paths: 7,264 input tokens and 3.158 seconds.
Replacing those with typed artifact selection is a concrete follow-up experiment.
Jev and cache accounting remain incomplete, so no cost saving is established.
The paired result and independent verification are summarized at the top of
this report.

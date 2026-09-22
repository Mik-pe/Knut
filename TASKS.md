# Knut: coding CLI first, Jev throughout

Research date: 2026-09-21. This is a proposed backlog, not a claim that the
features or performance gains exist. Implementation is happening concurrently;
recheck the current code before taking a task. Only this document was added by
the research pass.

## Direction

Make Knut excellent at completing real repository changes from the terminal.
Optimize verified task success, time to a correct patch, and cost per successful
task. Jev should remove unnecessary generation and improve evidence selection.
The number of Jev calls is not a success metric.

The central bet: expose the repository as a changing menu of concrete actions.
Rust constructs and validates the menu; Jev chooses among its entries; the
reasoner supplies new code, explanations, and plans when needed. Jev can drive
bounded stretches of the coding loop this way. Whether it should control most
of the loop is an experiment, not an assumption.

Keep one SessionRuntime and one execution gate across CLI, TUI, and editor
clients. Extend cohesive modules; when replacing behavior, remove the old path.
Benchmark policies can share that engine without becoming duplicate runtimes.

## Research findings that shape the design

- Jev returns Choice, Score, and Noul decisions rather than generated code.
  Multiple independent questions can share one state and request. This suggests
  separating selection from generation. [TypeSafe introduction](https://docs.typesafe.ai/introduction)
- Current documented model: `jev-1.13.0`; input costs $0.042/M tokens, output is
  free. Limits are 64k tokens per request and 32k for state plus the longest
  question. Pin the version for calibration. Published rate limits can change.
  [Models](https://docs.typesafe.ai/models)
- Choice permits 2–255 options; Score has 2–10 levels. Question IDs are routing
  keys, not model-visible instructions: describe the referenced candidate in
  the question itself. [API contract](https://docs.typesafe.ai/api)
- Choice/Score confidence summarizes a distribution; it is not independently
  established correctness. Noul provides a yes-probability without a separate
  confidence field. Calibrate each decision family on coding outcomes.
  [Confidence](https://docs.typesafe.ai/confidence)
- Documented weaknesses include numeric precision, indirection, irrelevant
  context, adversarial state, and inconsistency between differently formulated
  questions. Use small, direct judgments; keep arithmetic and invariants in
  Rust. Do not try generating code through repeated token choices.
  [Jev 1.13 limitations](https://docs.typesafe.ai/model-jaggedness/jev-1.13)
- Browser Use demonstrates a dynamic action/target menu with speculative target
  questions in one call, and generation only for text entry. This is evidence
  that the architecture is workable, not evidence of coding ability. Its
  published timing comparison covers only three repeats per arm of one task.
  [Implementation and measurements](https://github.com/browser-use/jev-ultrafast)
- TypeSafe's autoresearch cookbook uses an LLM to propose questions and trains
  a classical predictor on Jev's answers. Adapting this to coding outcomes is
  promising but unvalidated. [Feature discovery](https://docs.typesafe.ai/cookbooks/autoresearch_feature_discovery)
- RouteLLM supports learning model selection from outcome/preference data;
  its results do not establish coding-task gains for Knut or Jev.
  [RouteLLM paper](https://arxiv.org/abs/2406.18665)

## Existing foundation to reuse

Inspection found a real TypeSafe adapter in `src/typesafe.rs`, bounded question
packs in `src/frame.rs`, candidate discovery and advisory failure classification
in `src/session.rs`, deterministic edge rules in `src/edge.rs`, context
projections in `src/context.rs`, and calibration/cache/breaker infrastructure in
`src/calibration.rs`. `src/bench.rs` already defines matched benchmark arms.

These are foundations, not proof of end-to-end integration. In the inspected
code, context excerpts are selected in supplied order; recovery receives failed
node names and a repairability flag; candidate discovery selects capabilities.
Those are concrete places to introduce richer evidence and finer actions.
The README also reports a search-result-to-path binding failure. Resolve that
basic coding-loop gap before attributing success or failure to Jev's policy.

## P0 — establish an honest, useful coding loop

### J01 — End-to-end coding benchmark and decision census

Active acceptance goal and live attempt ledger: [HARNESS_COMPARISON.md](HARNESS_COMPARISON.md).
The isolated comparison runner now exercises an actual composer bug with external
regression checks and matched model/effort. Initial failed/incomplete attempts
exposed missing tool schemas and tmpfs quota pressure; no savings claim yet.

Completed one real-repository pilot: both Knut and Ante fixed the composer bug
at GLM-5.3 Flash / low effort and passed independent full-suite + hidden checks.
Knut used real search/read/edit calls and reported 18,635 input / 3,569 output
tokens. See the report for Ante accounting, invalidated attempts and cache/load
limitations. J01 remains open: this is not a held-out evaluation or Jev ablation.

Pilot-driven follow-ups:

- [ ] Measure replacing the search/read/edit planning template with typed
  candidate selection. The successful trial still spent 23.5 seconds in planning
  plus plan repair before its 32.3-second patch generation; this is a concrete
  Jev experiment, not a claimed saving.
- [ ] Read only the relevant method ranges and a verified test insertion anchor
  for exact edits; full-file context is no longer intrinsically required.
- [ ] Capture cached input and Jev usage before comparing costs with Ante.
- [ ] Compare checks under matched warm/cold cache policies, without concurrent
  builds. Native build/test/lint time dominated the successful pilot.
- [ ] Make the existing cancellation regression namespace-aware: a PID printed inside
  Bubblewrap is not an identity in the test runner's parent PID namespace.
  Keep the descendant-termination requirement; do not waive the check.
- [x] Stop asking Jev to ratify deterministic green evidence. The shared runtime
  gates completion in Rust; Jev remains available for bounded failure diagnosis
  and candidate selection.

Initial runtime audit: `Engine::handle_with` published task submission only after
the first expensive tick; it now publishes before driving. `SessionRuntime::generate`
replayed an unchanged prompt whenever workspace check evidence was missing,
including greetings. It now pauses after delivering a response instead of spending
another generation call. Verification remains required; separate conversational
completion from verified coding completion is still needed.

Implemented the initial generator census: `knut run <prompt> --census <new.json>`
records each actual cascade call's purpose, model/tier, delivery mode, outcome,
latency and reported input/output tokens, including planning repairs and failures.
Reports exclude prompt/response bodies and error text; existing files are refused.
Unknown usage makes aggregate totals unknown. The session runtime uses the same
call-boundary instrumentation for its model counter and usage events, including
generation inside behavior trees. Jev calls, cached tokens, costs, first-action
timing and verified-success benchmarking are not covered yet. No live benchmark
or token-savings claim has been made.

The 2026-09-22 integration removes `coding_run`'s separate planner/executor loop
and the JSONL stub. Both now submit to the same engine driver as the TUI.
SessionRuntime owns real checks and bounded repair. See REPO_READINESS.md for
current validation; the earlier Ante pilot does not validate this new version.

- [ ] Trace real `knut run` tasks and identify every model round trip: what was
  generated, what decision it served, and whether code or Jev could replace it.
- [ ] Extend the pilot to at least 30 held-out tasks spanning local fixes,
  multi-file changes, diagnosis, ambiguous requests, stale edits, and hostile
  tool output. Split by repository/task family; keep hidden checks outside
  all model contexts. Repeat runs and retain every failed attempt.
- [ ] Compare the same reasoner, tools, effort, snapshots, and budgets with
  Jev enabled/disabled in the same engine. Test cheaper generation separately.
- [ ] Record verified success, regressions, p50/p95 wall time, time to first
  useful action, generator input/output/cached tokens, Jev tokens/calls,
  retries, tool time, and total cost per success. Unknown usage stays unknown.

Done when a reproducible report identifies the largest avoidable costs and
includes paired uncertainty estimates. Thirty tasks are a starting pilot, not
proof of a small quality difference. Use shadow judgments for selection
accuracy, but execute matched arms to measure downstream effects.

### J02 — Turn observed artifacts into executable candidates

Started: typed artifact references now support validated JSON pointers, allowing
search-hit paths and read hashes to flow directly into subsequent tools. Missing
fields and wrong selected types fail safely. This is binding infrastructure,
not yet Jev candidate selection.

Implemented a revision-bound `files/edit` tool for small exact replacements.
It validates every replacement before writing, refuses ambiguous matches and
stale hashes, preserves untouched bytes, and uses the same write gate. Plans now
receive full argument schemas. `run` has a three-attempt repair budget carrying
actual tool/check failures; blocked actions do not gain permission on retry.

- [ ] Enumerate search hits, symbols, diagnostics, existing check commands,
  patch artifacts, and supported code actions as typed candidate handles.
- [ ] Resolve handles to concrete arguments in Rust. Bind handles to content
  revisions and preserve source locations; never use a model-selected label
  as an invented path or shell command.
- [ ] Cover the README's search-result → selected file → read → edit → check
  workflow. If the other agent already fixed binding, build on its solution.
- [ ] Measure candidate recall independently of Jev selection accuracy. Include
  `none / need more evidence`; a confident choice cannot repair a missing option.

Done when this workflow completes on real fixtures without asking a generator
to restate a path already present in tool output. Start with the existing small
candidate bound; evaluate hierarchical retrieval before expanding menus.

### J03 — Give Jev a bounded next-action loop

Started: deterministic no-progress guard for text generation with unmet evidence;
regression tests require one model call even after repeated runtime drives.
This is a safety stop, not the full next-action policy described below.

- [ ] After meaningful observations, offer concrete operations such as read
  candidate, inspect references, run known check, apply prepared patch,
  request generation, diagnose, and request completion verification.
- [ ] Ask operation and compatible target questions together. Each target
  question must be answerable from the current state without seeing another
  answer. Rust consumes only the selected operation's target and rejects
  inconsistent combinations. [Speculative fan-out](https://docs.typesafe.ai/patterns/fan-out)
- [ ] Skip judgment when the next action is mechanically determined. Route
  uncertainty or missing candidates to the reasoner; ask the user only for
  information or authority that the system cannot obtain itself.
- [ ] Enforce progress budgets, repeated-action detection, cancellation,
  revision freshness, approval, and completion evidence in the existing runtime.

Done when a coding task includes multiple useful navigation/check actions
between generative calls, with fewer generator turns and no observed success
regression in the matched pilot. Completion remains a verified runtime state.

### J04 — Calibrate the decisions actually used in production

- [ ] Pin model, question pack, candidate construction, and state projection
  versions together. Verify existing calibration is wired into each live path.
- [ ] Label accepted decisions and abstentions against evidence, not the
  generator's agreement. Track selection accuracy versus coverage, candidate
  recall, Noul reliability/Brier score, and decision-specific failure costs.
- [ ] Audit recovery classification with actual bounded diagnostics; the
  inspected frame lacks the failure text needed for useful semantic diagnosis.
- [ ] Test outages, stale replies, ambiguous menus, missing correct choices,
  prompt injection in files/logs, and changed question wording. Preserve raw
  probabilities separately from policy-adjusted scores.

Done when promotion is justified per decision family and model version. A
single global confidence threshold or high-confidence answer is insufficient.

## P1 — save tokens and latency while improving the patch

### J05 — Select evidence before paying the reasoner to read it

- [ ] Retrieve candidates deterministically with search, imports, symbols, and
  diagnostics. Ask Jev atomic relevance questions over bounded excerpts, then
  pack the reasoner's context under its actual budget.
- [ ] Score usefulness separately from duplication. Preserve user constraints,
  acceptance requirements, declarations, and necessary caller/callee context.
  Add an expansion action when omitted evidence becomes necessary.
- [ ] Compare supplied-order packing, deterministic ranking, and Jev reranking
  at equal budgets. Include misleading near-matches and cross-file dependencies.

Done when generator input falls materially (initial experiment target: 25%)
without lost task success or more rereads. This is extractive selection: Jev
returns IDs; Rust copies exact excerpts. It does not ask Jev to summarize.

### J06 — Convert noisy tool output into a useful evidence packet

- [ ] Parse compiler/test output into diagnostic records; deduplicate exact
  repeats in code. Let Jev select root-cause candidates and relevant log spans.
- [ ] Keep exit status, failing test identity, full artifact references, and
  essential stack/cause chains intact. Preserve access to the raw output.
- [ ] On ambiguous failures, gather the selected evidence before requesting
  diagnosis. Known transport errors and retry eligibility remain deterministic.

Done when large-log tasks need fewer input tokens and repair turns. Measure
missed root causes, not just compression ratio. Never let a retry judgment
override idempotency or replay rules.

### J07 — Make decisions at useful boundaries and hide their latency

- [ ] Batch independent questions on the same observation; measure state and
  question token costs. Enforce both documented context limits rather than
  treating the adapter's byte cap as an exact token cap.
- [ ] Start likely-needed read-only retrieval while checks run when authorized
  and independent. Tag speculative results with revision and cancel stale work.
- [ ] Reuse the existing exact cache, coalescer, deadlines, and circuit breaker;
  verify keys include all decision inputs and question/model versions.
- [ ] Avoid making Jev a mandatory serial hop before obvious actions. Trace
  the critical path and include discarded speculation in cost accounting.

Done when end-to-end p95 improves. Lower per-call latency alone does not count;
extra network decisions can erase savings from reduced generation.

### J08 — Turn Jev into a focused reviewer

- [ ] Given a diff and its relevant contracts, ask separate questions about
  omitted error handling, API compatibility, missing call-site changes,
  weakened checks, and mismatch with the requested behavior.
- [ ] Select suspicious hunks or existing requirement IDs; send only supported
  concerns to the reasoner for diagnosis and repair. Avoid a vague quality score.
- [ ] Evaluate on known defects and clean patches, including regressions that
  compile and pass visible tests. Retain held-out checks that edits cannot weaken.

Done when confirmed defects caught per review token improve with a tolerable
false-positive rate. Jev triages review effort; it does not certify correctness.

### J09 — Choose the next test for information value

- [ ] Use code/dependency information to enumerate relevant checks. Jev judges
  which candidate best distinguishes the remaining diagnostic hypotheses.
- [ ] Rust combines measured duration, historical failures, and semantic
  relevance to order tests. Run cheap discriminating checks early.
- [ ] Preserve the required final verification suite; early prioritization
  must not silently redefine what counts as complete.

Done when time to the first actionable failure decreases without missed final
regressions. Start with test ordering before experimenting with test omission.

### J10 — Detect drift and repair loops before another expensive turn

- [ ] Combine exact repeated-action/error detection with Jev judgments about
  whether new evidence contradicts a plan or leaves a requirement unaddressed.
- [ ] Offer bounded responses: gather specified evidence, request a different
  diagnosis, resume the valid plan, or surface an actual blocker.
- [ ] Evaluate oscillating patches, repeated failed commands, partial success,
  and user corrections. Require fresh observations before another judgment.

Done when wasted retries fall without premature abandonment of solvable tasks.

## P2 — larger bets worth controlled experiments

### J11 — Compile reasoning into reusable local procedures

- [ ] Let the reasoner produce a short validated procedure with typed slots,
  preconditions, branches, and stop conditions for a class of coding work.
  Jev binds observed candidates and selects branches on later steps.
- [ ] Start with diagnose failing test → locate symbol → generate patch →
  verify. Reuse only when repository/tool/contract preconditions still hold.
- [ ] Treat procedures as artifacts executed by the existing plan machinery,
  not a second DSL interpreter or parallel agent framework.

Hypothesis: one planning call can support many useful actions. Measure plan
reuse, invalid bindings, generator turns saved, and end-to-end success. Stop
the experiment if maintaining procedures costs more than fresh planning.

### J12 — Rank prepared repairs, including zero-generation fixes

- [ ] Offer compiler fix-its, LSP code actions, and parser-backed codemods as
  concrete patch candidates. Jev judges applicability against the user's goal.
- [ ] In isolated benchmark workspaces, compare one generated patch against a
  small candidate set produced only when uncertainty warrants the extra cost.
  Use real checks to eliminate invalid candidates before semantic ranking.

Hypothesis: selection over existing transformations can beat regenerating code.
Measure regression rate and total cost including all discarded patches. Apply
through the current patch/gate machinery with revision checks.

### J13 — Learn when extra reasoning is worth its price

- [ ] Combine Jev features such as requirement ambiguity, cross-module impact,
  and missing evidence with measured outcomes to select compute budgets.
- [ ] Compare against simple rules. Keep substantive reasoning on the quality
  model by default until a specific task family earns cheaper routing.
- [ ] Learn from checks and accepted outcomes; separate training, tuning, and
  test repositories. Include retries in both cost and latency labels.

Hypothesis: spending more on difficult tasks and less on routine generation
outperforms prompt-length or generic difficulty routing. Keep this experiment
separate from measuring Jev's control-loop benefit.

### J14 — Optimize question packs offline

- [ ] Let a reasoner propose direct atomic questions from failed traces; Jev
  supplies features; fit a small interpretable predictor or calibrated policy.
- [ ] Use the TypeSafe feature-discovery pattern as inspiration, with coding
  outcomes as labels. Compare against hand-written questions and plain rules.
- [ ] Penalize extra questions and state size. Freeze packs before held-out
  evaluation; never self-modify the production policy during a user task.

Hypothesis: learned combinations of narrow signals predict useful next actions
better than asking one broad question. Reject gains caused by task leakage.

### J15 — Ask for the evidence that would change the decision

- [ ] When several diagnoses remain plausible, enumerate observations that
  discriminate between them: a caller, configuration value, reproduction,
  specific test, or earlier error in a log.
- [ ] Jev judges each observation's relevance to explicit hypotheses. Rust
  estimates value versus measured cost; the reasoner creates new hypotheses
  when the bounded set is inadequate.

Hypothesis: active evidence gathering avoids speculative editing and broad
repository reads. Compare against always reading top-ranked files. A model's
distribution is a heuristic here, not a guaranteed information-gain estimate.

### J16 — Build a dependency map for context and decision reuse

- [ ] Attach facts, excerpts, decisions, and check results to the files,
  symbols, settings, and tool versions that support them.
- [ ] Invalidate exact dependencies mechanically. Experiment with Jev only
  for semantic relevance of changed material, with conservative rereads on doubt.
- [ ] Use preserved evidence to build a small next-turn packet rather than
  repeatedly replaying the entire transcript. Preserve native provider
  continuation requirements and measure prompt-cache losses from repacking.

Hypothesis: incremental evidence reuse saves more tokens than aggressive
summarization. Jev may prioritize revalidation; it must not declare stale checks
fresh. Measure missed invalidations and total billed tokens over long tasks.

### J17 — Explore repo-specific semantic search without constant generation

- [ ] At indexing time, label bounded symbols/modules against a small set of
  useful concepts: parsing, authorization, persistence, cancellation, errors.
- [ ] At task time, combine lexical/symbol retrieval with those Jev-derived
  labels and reranking. Refresh changed symbols rather than the whole repo.
- [ ] Compare against lexical and existing semantic retrieval baselines,
  including indexing cost and cold-start latency.

Hypothesis: a reusable semantic index helps find code described in different
words from the request. Keep labels as retrieval hints, never source truth;
abandon the index if maintenance exceeds the downstream savings.

## Suggested execution order and promotion rules

Start J01–J04, then compare J05/J06 against J03 independently to discover whether
better evidence or fewer reasoning turns produces the larger gain. Add J07
after traces reveal the critical path. Pursue J08–J10 for measurable quality and
recovery improvements. Choose P2 experiments from actual bottlenecks.

For every experiment, record the hypothesis, baseline, exact versions, dataset,
budget, failure cases, quality delta, token/cost delta, latency delta, and decision
to keep or remove it. Require a predeclared quality margin and enough samples
to assess it before broad promotion; a small pilot with no failures is not a
reliability guarantee. Remove unsuccessful runtime paths rather than accumulating
optional systems.

At the documented price, 10k Jev input tokens cost $0.00042; 20 such calls cost
$0.0084 before any generator or tool work. Cheap classification still adds
latency and can increase total input tokens. Report generator tokens, Jev tokens,
combined tokens, actual cost, and verified success separately. Prefer one avoided
reasoner turn or one prevented bad patch over dozens of decorative judgments.

# Development

## Workspace

| Crate | Responsibility |
| --- | --- |
| `knut` | CLI entry point and compatibility exports |
| `knut-runtime` | Sessions, providers, tools, checks, persistence and protocols |
| `knut-terminal` | Terminal UI, connection screens and event replay |
| `knut-auth` | Account storage, authentication and token refresh |
| `knut-editor` | Composer state and input memory |

The terminal depends on the runtime. Both share auth/editor primitives; the
runtime does not depend on the terminal. New clients use the existing session
commands/events and execution gate.

## Checks

From the checkout:

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --locked
node scripts/smoke-harness.mjs
```

The process smoke uses an isolated Rust fixture and a deliberately fallible local
provider. It exercises argument repair, failed-check recovery, exact approvals
and queued tasks without paid model calls.

For terminal checks, install `tuistory` where Node can resolve it, then run:

```sh
node scripts/smoke-tui.mjs --screenshots
node scripts/smoke-settings.mjs
```

For an existing external installation, set `KNUT_TUISTORY_MODULE` to its absolute
`dist/index.js` path. The smoke uses a local provider and checks four terminal
sizes, editing, help, connection settings, approvals, repair, saved drafts and
color/motion fallbacks. Snapshots are written to a temporary directory.

The settings smoke uses a GET-only catalog fixture to check model search,
scrolling, failed refreshes, saved API selections, appearance preferences and
environment reset across restarts. It preserves an unfinished draft throughout.

Smoke scripts locate the debug binary through Cargo metadata. Set `KNUT_BINARY`
to use another built executable.

These checks validate runtime/client behavior. They do not measure live model
success rates. Run `knut doctor --live` separately to check configured providers.

## Model-call census

```sh
knut run "fix the failing test" --census /tmp/knut-census.json
```

The output path must be new. The report records actual generator calls: model,
tier, purpose, delivery mode, outcome, latency and reported input/output tokens,
including failed runs. It excludes prompt/response bodies and error text.
Missing usage is unknown. Jev usage, cached-token breakdowns and costs are not
included. This is a normal live task and may incur charges; census recording
does not grant write approval.

## Evaluation

`knut bench` runs an offline fixture pilot with real checks and no model calls,
then writes an inspectable report. Playground commands (`route`, `repl`,
`demo-tree`, `eval`) use mock/demo paths. Neither measures live model quality.

The [comparison runner](../scripts/compare-harnesses.mjs) creates isolated
snapshots for a live repository-edit pilot with Ante:

```sh
trial_dir=$(mktemp -d /var/tmp/knut-comparison.XXXXXX)
node scripts/compare-harnesses.mjs prepare "$trial_dir"
node scripts/compare-harnesses.mjs run "$trial_dir" knut high
node scripts/compare-harnesses.mjs run "$trial_dir" ante high
node scripts/compare-harnesses.mjs verify "$trial_dir" knut high
node scripts/compare-harnesses.mjs verify "$trial_dir" ante high
```

It requires Linux Bubblewrap, Node, Ante and a built debug Knut binary. Use an
empty disk-backed temporary directory. Live runs use configured credentials,
may incur charges, and produce logs/diffs containing repository content. Source
is read-only and masked in the sandbox; edits happen in temporary copies.

The pilot uses GLM-5.3 Flash on the coding-plan endpoint at the requested effort.
It repeats a development task with different prompts/tools across harnesses;
it is not held-out evidence or a Jev ablation.

For a broader comparison, freeze prompts, snapshots, binaries, models, actual
request settings, budgets and checks. Isolate build targets, match warm/cold
cache policy and run builds sequentially. Keep independent task checks outside
model-visible snapshots and retain every failure/timeout. Compare Jev enabled
and disabled in the same engine before attributing a result to routing. Unknown
usage remains unknown; do not infer cost from incomplete token accounting.

## Installation updates

Set `KNUT_UPDATE_TARGET` to a fixed absolute installed binary path and open
Knut's source checkout as the workspace. Credentials and service configuration
belong outside the checkout.

After implementing a change, `self/inspect_installation` reports the installed
binary hash and source revision. `self/install_update` requires those exact
identities and write approval. It runs offline sandboxed formatting checks,
workspace tests, Clippy and a release build, checks the identities again, then
atomically activates the new executable. Missing dependencies, an unavailable
sandbox, failed checks or changed inputs prevent activation.

Existing sessions keep their original executable; new sessions use the update.
The previous binary is retained as `knut.previous` beside the installation.
Installing does not restart services, publish source or restore approvals.

# Runtime and integration

The terminal, `run` and JSONL submit commands to one `SessionRuntime` and consume
its events. Tools pass through `ExecutionGate`, which enforces policy, exact
approvals and replay rules. An optional decision model prioritizes context/tool
schemas and classifies failures; Rust owns permissions and completion evidence.

## Workspace profiles

The CLI opens the current directory for file tools and sandboxed commands.
Commands start with a clean environment and deny network access by default.
`KNUT_PROFILE=auto` discovers coding checks when a supported manifest is present;
otherwise it uses the general profile. `coding` requires configured checks.
`general` supports document work or conversation inside a repository:

```sh
KNUT_PROFILE=general knut run "Summarize notes.txt"
KNUT_PROFILE=general knut tui
```

Coding checks run when project content changes. Questions that leave the project
unchanged can complete without a build; explicit task requirements still apply.
Rust checks cover every workspace member, including members omitted from Cargo's
defaults. Node/TypeScript discovery is basic; unsupported projects need custom
completion setup.

Verification hashes the full contents of included files, including large sources.
Unreadable inputs prevent verification. Check evidence belongs to a revision;
changes during manual checks mark it stale and fail verification. Rerun checks
against the updated source. `knut verify --json` emits one report for the workspace.

Exact file edits require a read hash and JSON-encoded `{old,new}` replacements.
Stale hashes and ambiguous matches fail before writing. Tasks are bounded to 32
runtime turns, 16 tool calls per response and two completion-repair attempts.

## JSONL

Start `knut jsonl`. Write one JSON command per stdin line and consume JSONL
from stdout; diagnostics go to stderr. Protocol version **2** has explicit
queue and steer operations. Queue events carry stable request IDs.

`knut jsonl <prompt>` is a one-shot invocation: it does not read stdin and ends
when the task completes, fails, is cancelled or needs user input. Use the
command-driven form for approval and question responses.

| Command | Fields |
| --- | --- |
| `submit`, `queue` | `prompt`, optional `options` |
| `steer` | `prompt` |
| `update_queued` | `id`, `prompt`, optional `options` |
| `remove_queued`, `run_queued` | `id` |
| `answer` | `value` |
| `approve`, `deny` | Exact pending `approval_key` |
| `pause`, `resume`, `cancel`, `close` | None |

Examples:

```json
{"type":"submit","prompt":"Explain the failing test"}
{"type":"queue","prompt":"Then inspect the documentation"}
{"type":"steer","prompt":"Preserve the public API"}
```

Events use `ready`, `event`, `outcome` or `error` envelopes with
`protocol_version`. Terminal outcomes are `completed`, `failed` and `cancelled`.
An approval must echo the pending action's key; missing approval UI keeps the
action blocked. A line is limited to 1 MiB. See the
[command/event definitions](../crates/knut-runtime/src/headless.rs) for the full schema.

### Task options

Submit, queue and update commands accept task-specific tools, instructions,
context and additional completion requirements:

```json
{
  "type": "submit",
  "prompt": "Summarize the report",
  "options": {
    "tools": ["read"],
    "instructions": "Be concise.",
    "context": [{
      "source": {"uri": "document:report", "revision": "v1"},
      "description": "Report excerpt",
      "content": {"text": "Quarterly results…"}
    }]
  }
}
```

`tools` restricts registered tool IDs; an empty array permits no tools and omission
uses the configured catalog. Instructions and context stay with queued tasks.
`requirements` accepts `{check, description, blocking}` records and adds to the
profile's completion contract. The model cannot waive required evidence. Options
are limited to 128 KiB and the runtime's context-record limit.

## Stored sessions

```sh
knut sessions list
knut sessions show <id>
knut sessions export <id>
knut sessions plan <id>
```

`show` replays stored state without executing tools. `export` redacts paths;
add `--raw` to include them. `plan` reports whether the stored session can
continue against the current workspace; it does not resume work. Saved drafts
are covered in the [terminal guide](terminal.md#drafts-and-history).

## Embedding

`HarnessSetup` supplies a tool registry, instructions, an optional
`ContextProvider` and an optional `CompletionMonitor`. `build_harness` creates
the same engine without opening a workspace. Hosts managing their own providers
can construct `SessionRuntime::new` with an injected model cascade.
See the [setup types](../crates/knut-runtime/src/harness.rs) and
[engine builders](../crates/knut-runtime/src/engine.rs).

Context records identify snapshots with a resource URI and revision. Supplied
URL/document records do not fetch resources or establish external freshness;
the host's context provider owns that. Workspace reads use the gate and retain
content hashes. Prioritization keeps the full evidence and tool catalog.

The CLI registers `AskUserTool`; answers return through the tool-result
continuation. Headless/editor adapters use the same session semantics. Language
servers are reported by `knut lsp` and are never started implicitly.

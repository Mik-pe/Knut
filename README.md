# Knut

![Knut — Your code, untangled.](assets/knut-cover.webp)

Knut is an experimental agent harness for your terminal, written in Rust. Open
it in a project, connect a model, and ask it to inspect code or make a change.
You review proposed edits and commands before they run; Knut checks changed
code before reporting completion. It also supports general tasks and custom
tools through the same runtime.

## Start

Build from this checkout with Rust and Cargo:

```sh
cargo build --release --locked
./target/release/knut
```

Press **F2** to open Settings, choose **Continue with ChatGPT**, complete sign-in
in your browser, and select a model. The selection is saved for future starts.
Model access is checked when a task runs.

For an API connection, configure the environment before starting:

```sh
export KNUT_PROVIDER=openai
export OPENAI_API_KEY="your-api-key"
./target/release/knut doctor
./target/release/knut
```

The OpenAI adapter defaults to `gpt-6.1-sol`; set `KNUT_PROVIDER_MODEL` to change
it. A saved ChatGPT model takes priority over environment settings. Choose
**Use environment configuration** in Settings to switch back. Z.ai and other
Chat Completions endpoints are also supported; see
[connection and configuration](docs/configuration.md).

Command execution requires **Bubblewrap on Linux** or **sandbox-exec on macOS**.
An unavailable sandbox blocks commands. Windows command execution is unsupported.
`doctor` inspects setup offline; `doctor --live` makes real provider calls.

## Work in a project

Add the built binary to your `PATH`, then start it from the directory you want
to work in:

```sh
export PATH="$PWD/target/release:$PATH"
cd /path/to/project
knut
```

Type a task and press **Enter**. Use **Alt+A** to approve an exact pending action
or **Alt+D** to deny it. **Ctrl+P** searches commands, **F2** opens settings,
**F4** searches models, **Ctrl+R** opens changes and checks, and **F1** shows help.
During work, Enter queues another task and **Alt+S** switches the draft to
steering the current task. **Ctrl+C** stops work or closes the current overlay.
See the [terminal guide](docs/terminal.md) for shortcuts and saved drafts.

For a single task or an independent check:

```sh
knut run "explain why this test fails"
knut run "fix the failing test" --yes
knut verify
knut verify --json
```

`--yes` approves file writes for that run. Commands still need exact approval
through the terminal or a JSONL client; a blocked approval ends a scripted run.
The default profile discovers Rust and basic Node/TypeScript checks. Use
`KNUT_PROFILE=general` for document work or conversation without coding checks.

Verification covers every Rust workspace member and requires evidence for the
current source revision. `verify --json` emits one workspace report. See
[workspace profiles and verification](docs/runtime.md#workspace-profiles) for details.

## More

- [Connection and configuration](docs/configuration.md): providers, accounts and environment variables.
- [Terminal guide](docs/terminal.md): approvals, queueing, shortcuts and input memory.
- [Runtime and integration](docs/runtime.md): JSONL, task options and embedding.
- [Development](docs/development.md): workspace layout, checks, smoke runs and installation updates.

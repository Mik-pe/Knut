# CLI usability

## Design

Knut is a coding conversation inside a repository. The normal screen should
answer three questions: what did I ask, what is happening, and what can I do next?
Engine internals are useful on demand. They do not need permanent screen space.

Use one conversation at every terminal size. Keep the input available while
work runs. Show the model, task state, current action, and queue count compactly.
Open changes, jobs, or decision details explicitly and use Escape to return.
Keep ordinary letters as text, not hidden session commands. Show shortcuts where
they apply, preserve drafts through navigation, and make interruption predictable.

The visual direction is charcoal, warm text, muted teal, and minimal borders.
Reserve warning/error colors for actionable states. Do not depend on color alone.
Use the same runtime and approval gate in every view.

## Implemented in this redesign

- [x] Bare `knut` launches the session in a terminal; noninteractive invocation
  prints help. Main help leads with coding commands instead of the playground.
- [x] Replace the three-column dashboard and narrow-screen tabs with one
  conversation and explicit full-width detail views.
- [x] Remove the large splash and infrastructure inventory from the welcome.
- [x] Make commands accessible from input with `/`, Ctrl+P, or Ctrl+K;
  provide filtering, visible selection, arrow navigation, and Escape.
- [x] Only offer implemented commands. Stop advertising unwired attachments.
- [x] Add global shortcuts for changes, jobs, decisions, and help.
- [x] Compact contextual `?` shortcut modal with empty input; F1 during editing.
  Enable enhanced keyboard events for Shift+Enter, retaining Ctrl+J fallback.
- [x] Publish submitted prompts before the first model call. Suppress echoed
  generation summaries already present in the transcript.
- [x] Stop repeated text generation when workspace verification is outstanding.
  This pauses for input; separate chat completion from coding verification next.
- [x] Ctrl+C closes overlays, cancels active work, clears an idle draft, or
  exits an empty idle session. Keep prompt history and make clearing undoable.
- [x] Show approvals beside the input with explicit Alt+A/Alt+D keys;
  Enter while waiting for approval keeps the draft.
- [x] Grow and scroll multiline input; position the caret using grapheme
  boundaries and terminal cell widths. Handle bracketed paste in the live loop.
- [x] Follow the end of long responses; allow line-based page scrolling and
  preserve indentation. Keep routing chatter in decision details.
- [x] Make review use the width for the diff; navigate files/hunks and scroll.
  Remove misleading in-memory reject/keep controls from the live UI.
- [x] Handle real terminal resizing and retain monochrome/ASCII fallbacks.

## Next work, in order

### P0 — trust and task completion

- [ ] Wire `@file` and line-range attachments into the actual model request,
  with a preview, stale-content handling, bounded payloads, and errors before
  submission. Restore the command only when an end-to-end test proves delivery.
- [ ] Make queue versus steering an explicit user choice. Preserve the current
  draft when switching; show pending requests and support editing/removing them.
  Done when a mid-task message cannot unexpectedly change the running task.
- [ ] Persist drafts and prompt history across restarts; offer actual session
  resume with clear workspace identity and stale evidence handling.
- [ ] Present the exact proposed patch/command, scope, and revision in every
  approval. Support long permission descriptions without losing information.
  Keep approval choices tied to the execution gate's exact identity.
- [ ] Completion should show changed files, checks that passed/failed, and
  unresolved work with direct access to relevant output. Test successful,
  blocked, cancelled, and partially completed tasks in real sessions.

### P1 — everyday editing and navigation

- [ ] Render Markdown/code blocks and links with readable indentation and
  wrapping. Add transcript search and copying of code, commands, and diffs.
- [ ] Expand individual tool output inline or in a focused detail view;
  support full logs, long diffs, and scroll position preservation as work arrives.
- [ ] Add word navigation/deletion and an external-editor shortcut. Keep paste
  as one undo operation and surface truncation immediately.
- [ ] Build a setup flow that validates credentials without displaying them,
  chooses configured models, and explains recovery after provider failure.
- [ ] Make help and keyboard hints adapt to very short terminals; ensure every
  command remains discoverable without memorizing modifier keys.

### P2 — polish after the interaction contracts hold

- [ ] Offer terminal-default/light themes and configurable bindings. Preserve
  semantic colors and avoid assuming a dark terminal under `NO_COLOR`.
- [ ] Add optional mouse scrolling without breaking terminal text selection.
- [ ] Support accessible/reduced-motion output and a plain streaming mode for
  screen readers and terminals that cannot use the full-screen interface.
- [ ] Evaluate an inline terminal mode with native scrollback, using the same
  state/runtime. Adopt it only if interruption, approvals, resize, and long
  output work as reliably; replace the presentation path rather than maintain
  two separate execution systems.

## Acceptance checks

Exercise a new user opening Knut, submitting a task, approving/denying an action,
reading a long answer, inspecting a failed check, steering, cancelling, and
returning after interruption. Repeat at 60x18, 80x24, and 140x40, plus Unicode,
multiline paste, monochrome, terminal resize, and provider failure. Measure
input latency during streaming. No interaction should discard a draft, submit
a paste, invent success, or require navigating an unrelated pane.

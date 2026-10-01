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

The visual direction is charcoal, warm text, luminous teal, and minimal borders.
Reserve warning/error colors for actionable states. Do not depend on color alone.
Use the same runtime and approval gate in every view.
The native trefoil knot uses depth shading and a travelling highlight. The
welcome mark adapts to the available room; a small version identifies the
session header. Its 2.4-second introduction never delays typing. Active work
animates; approval, pause and terminal states settle. `/motion` and
`KNUT_TUI_MOTION=off` provide reduced motion without removing status labels.

## Implemented in this redesign

- [x] Bare `knut` launches the session in a terminal; noninteractive invocation
  prints help. Main help leads with coding commands instead of the playground.
- [x] Replace the three-column dashboard and narrow-screen tabs with one
  conversation and explicit full-width detail views.
- [x] Keep the welcome nonblocking, with a responsive knot mark and a clear
  first action. Hide infrastructure inventory behind setup diagnostics.
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
- [x] Show concise file/check summaries, keep reused approval-resume results from
  appearing as new work, and display proposed replacements before long approval
  identities. Fix Unicode truncation in the workspace header.
- [x] Replace unused static knot art with the native trefoil renderer. Use
  terminal cell widths for paths and wrapped text; keep the editor usable down
  to 30x12. Stop repainting settled idle screens. Honor explicit color overrides
  in the terminal backend as well as the theme.
- [x] Preserve the unfinished draft and cursor while browsing prompt history.
  Make a multiline paste one undoable edit, normalize CRLF, keep grapheme
  boundaries and report truncation in the live shell.

## Next work, in order

### P0 — trust and task completion

- [ ] Wire `@file` and line-range attachments into the actual model request,
  with a preview, stale-content handling, bounded payloads, and errors before
  submission. Restore the command only when an end-to-end test proves delivery.
- [x] Make queue versus steering an explicit user choice. Preserve the current
  draft when switching; show pending requests and support editing/removing them.
  Enter queues during work; Alt+S switches to steering. Jobs provides selection,
  Alt+E edit, Alt+X remove and Alt+R to start a held request. Editing keeps the
  previous draft until acknowledgement; Escape restores it. The runtime owns
  dispatch, and failure/cancellation holds the queue. Queue acknowledgements
  preempt a quiet, abandon-safe tick (stuck provider) while ticks holding
  supervised work or streaming deltas keep their turn; covered by
  `queue_acknowledgements_do_not_wait_for_the_active_tick`.
- [x] Persist drafts, Unicode cursor positions and bounded prompt history per
  canonical workspace. Autosave in the background; Ctrl+Q saves and exits when
  idle. Preserve the unfinished draft during history browsing and queue editing.
  Report corrupt storage and refuse conflicting writes from another terminal.
- [ ] Offer actual session resume with clear workspace identity and stale
  evidence handling.
- [x] Present the exact proposed patch/command, scope, and revision in every
  approval. Support long permission descriptions without losing information.
  The automatic scrollable preview retains complete arguments and the gate's
  exact identity. Alt+V reopens it; PgUp/PgDn scrolls it. Exact edits show old/new
  text and read hashes. Tests cover previews beyond the transcript's size limit.
- [ ] Completion should show changed files, checks that passed/failed, and
  unresolved work with direct access to relevant output. Test successful,
  blocked, cancelled, and partially completed tasks in real sessions.

### P1 — everyday editing and navigation

- [ ] Render Markdown/code blocks and links with readable indentation and
  wrapping. Add transcript search and copying of code, commands, and diffs.
- [ ] Expand individual tool output inline or in a focused detail view;
  support full logs, long diffs, and scroll position preservation as work arrives.
- [x] Add word navigation/deletion: Ctrl+Left/Right, Alt+B/F, Ctrl+W and
  Alt+Backspace. Preserve Unicode graphemes and atomic undo.
- [ ] Add an external-editor shortcut.
- [x] F2 Settings opens ChatGPT consent in the system browser, validates OAuth,
  chooses account-visible models, saves the choice ahead of environment defaults,
  and exposes usage recovery.
  Model access is confirmed by real inference; API-key setup still uses env.
- [x] Make help scroll with Up/Down and PgUp/PgDn. Adapt keyboard hints and the
  placeholder to narrow terminals; keep F1 discoverable and Escape visible.

### P2 — polish after the interaction contracts hold

- [ ] Offer terminal-default/light themes and configurable bindings. Preserve
  semantic colors and avoid assuming a dark terminal under `NO_COLOR`.
- [ ] Add optional mouse scrolling without breaking terminal text selection.
- [x] Support reduced motion and static ASCII output. Keep ordinary monochrome
  independent of animation preferences.
- [ ] Add a plain streaming mode for screen readers and terminals that cannot
  use the full-screen interface.
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

`scripts/smoke-tui.mjs --screenshots` automates the local-provider terminal flow,
including 30x12, 60x18, 80x24 and 140x40, scrollable help, word editing, exact
approvals, failed-check repair, draft/cursor/history recovery across restarts,
and truecolor/ASCII modes.
Set `KNUT_TUISTORY_MODULE` when tuistory is installed outside Node's search path.

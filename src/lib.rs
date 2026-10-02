//! Public entry points for the runtime and terminal client.
pub use knut_runtime::*;
pub use knut_terminal::ConnectionAction;
pub use knut_terminal::logo_lines;
pub use knut_terminal::{AdapterEquivalence, replay_state};
pub use knut_terminal::{ColorLevel, Glyphs, Palette, Rgb, Theme};
pub use knut_terminal::{
    ContextInspection, DecisionInspector, DecisionProvenance, DecisionRecord, LatencyBreakdown,
    LatencyPhase, OutstandingWork, RequirementStatus, SelectedContext, TaskNode, UsageView,
};
pub use knut_terminal::{
    Focus, MAX_ENTRY_CHARS, MAX_TIMELINE, PendingPrompt, TimelineEntry, TimelineKind,
    WorkbenchState, WorkbenchStats,
};
pub use knut_terminal::{
    LayoutPlan, Tab, plan_layout, render, render_review, render_review_themed, render_themed,
};
pub use knut_terminal::{PaletteCommand, command_catalog, filter_catalog};
pub use knut_terminal::{ShellAction, TerminalGuard, handle_key, run_shell};

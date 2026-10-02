pub use knut_runtime::*;
mod commands;
mod connection;
mod equivalence;
mod inspector;
mod knot;
mod preferences;
mod replay;
mod terminal;
mod theme;
mod tui;
mod tui_render;
mod tui_state;

pub use commands::{PaletteCommand, command_catalog, filter_catalog};
pub use connection::ConnectionAction;
pub use equivalence::AdapterEquivalence;
pub use inspector::{
    ContextInspection, DecisionInspector, DecisionProvenance, DecisionRecord, LatencyBreakdown,
    LatencyPhase, OutstandingWork, RequirementStatus, SelectedContext, TaskNode, UsageView,
};
pub use knot::logo_lines;
pub use preferences::TerminalPreferences;
pub use replay::replay_state;
pub use terminal::TerminalGuard;
pub use theme::{ColorLevel, Glyphs, Palette, Rgb, Theme};
pub use tui::{ShellAction, handle_key, run_shell};
pub use tui_render::{
    LayoutPlan, Tab, plan_layout, render, render_review, render_review_themed, render_themed,
};
pub use tui_state::{
    Focus, MAX_ENTRY_CHARS, MAX_TIMELINE, PendingPrompt, TimelineEntry, TimelineKind,
    WorkbenchState, WorkbenchStats,
};

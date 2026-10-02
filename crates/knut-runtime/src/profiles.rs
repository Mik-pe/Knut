use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    ArtifactRevision, CheckProfile, CheckRunner, CompletionMonitor, CompletionRequirements,
    ContextProvider, ContextRead, ContextRecord, Evidence, HarnessSetup, KnutError, ResourceRef,
    Supervisor, Workspace, register_command_tools, register_workspace_tools,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspaceProfile {
    #[default]
    Auto,
    General,
    Coding,
}

impl WorkspaceProfile {
    pub fn from_env() -> Result<Self, KnutError> {
        match std::env::var("KNUT_PROFILE").as_deref() {
            Ok("auto") | Err(_) => Ok(Self::Auto),
            Ok("general") => Ok(Self::General),
            Ok("coding") => Ok(Self::Coding),
            Ok(other) => Err(KnutError::Tool(format!(
                "unknown KNUT_PROFILE {other:?}; expected auto, general or coding"
            ))),
        }
    }
}

pub fn workspace_setup(
    workspace: Workspace,
    profile: WorkspaceProfile,
) -> Result<HarnessSetup, KnutError> {
    let coding = match profile {
        WorkspaceProfile::Auto => !crate::discover_profiles(&workspace).is_empty(),
        WorkspaceProfile::General => false,
        WorkspaceProfile::Coding => true,
    };
    let mut setup = HarnessSetup::default();
    register_workspace_tools(&mut setup.tools, workspace.clone())?;
    crate::register_self_update_tools(&mut setup.tools, workspace.clone())?;
    let supervisor = Arc::new(Supervisor::new(workspace.clone()));
    register_command_tools(&mut setup.tools, supervisor.clone())?;
    setup.context = Some(Arc::new(WorkspaceContext(workspace.clone())));
    if coding {
        let profile = CheckProfile::for_workspace(&workspace)?;
        setup.completion = Some(Arc::new(CheckRunner::new(workspace, supervisor, profile)));
        setup.instructions = "Answer questions by inspecting relevant files. When the user requests a change, implement it using tools. Use exact edits for small changes; read missing ranges before replacing truncated files. Read fresh hashes after edits. Do not weaken or delete tests to make checks pass.".to_owned();
    }
    Ok(setup)
}

struct WorkspaceContext(Workspace);

impl ContextProvider for WorkspaceContext {
    fn instructions(&self) -> Result<String, KnutError> {
        crate::workspace::instruction_context(&self.0)
    }

    fn reads(&self, prompt: &str) -> Vec<ContextRead> {
        crate::context::named_source_paths(&self.0, prompt)
            .into_iter()
            .map(|path| ContextRead {
                capability: "files".to_owned(),
                tool: "read".to_owned(),
                input: json!({"path":path,"start_line":1,"end_line":120}),
            })
            .collect()
    }

    fn record(&self, _read: &ContextRead, output: &Value) -> Option<ContextRecord> {
        let source = crate::context::source_from_read(output)?;
        Some(ContextRecord {
            source: ResourceRef {
                uri: format!("file:{}", source.path),
                revision: source.content_hash.clone(),
            },
            description: format!(
                "{} lines {}–{}",
                source.path, source.start_line, source.end_line
            ),
            content: serde_json::to_value(source).ok()?,
        })
    }
}

#[async_trait]
impl CompletionMonitor for CheckRunner {
    fn requirements(&self) -> CompletionRequirements {
        CheckRunner::requirements(self)
    }

    fn verify_unchanged(&self) -> bool {
        false
    }

    fn current_revision(&self) -> Result<ArtifactRevision, KnutError> {
        CheckRunner::current_revision(self, "workspace")
    }

    async fn verify(&self, revision: &ArtifactRevision) -> Vec<Evidence> {
        self.run_all(revision)
            .await
            .iter()
            .map(crate::CheckEvidence::to_evidence)
            .collect()
    }
}

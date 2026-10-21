use crate::openai_auth::{self, AccountInfo};
use crate::theme::Theme;
use crate::{ConnectionChange, ConnectionRequest, KnutError, ProviderConfig, ProviderModel};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionAction {
    Login(bool),
    Models,
    EnvironmentModels,
    SelectAccount(String),
    SelectModel(String),
    SelectEnvironmentModel(String),
    Logout,
    ManageUsage,
    Acknowledge,
    OpenAccount,
    Environment,
    ToggleMotion,
    ToggleBackground,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelSource {
    ChatGpt,
    Environment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionPage {
    Settings,
    Account,
    Models,
    Welcome,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConnectionPanel {
    pub page: ConnectionPage,
    pub accounts: Vec<AccountInfo>,
    pub models: Vec<(String, String)>,
    pub query: String,
    pub current_model: Option<String>,
    pub current_source: ModelSource,
    pub model_source: ModelSource,
    pub theme: Theme,
    pub selection: usize,
    pub busy: bool,
    pub cancellable: bool,
    pub status: String,
    pub error: Option<String>,
    pub return_to_conversation: bool,
    pub home: ConnectionPage,
}

impl ConnectionPanel {
    pub fn open() -> Self {
        let accounts = openai_auth::account_list();
        Self {
            page: ConnectionPage::Account,
            accounts: accounts.as_ref().ok().cloned().unwrap_or_default(),
            models: Vec::new(),
            query: String::new(),
            current_model: None,
            current_source: ModelSource::ChatGpt,
            model_source: ModelSource::ChatGpt,
            theme: Theme::detect(),
            selection: 0,
            busy: false,
            cancellable: true,
            status: "Connect your ChatGPT plan to Knut's coding harness.".to_owned(),
            error: accounts.err().map(|error| error.to_string()),
            return_to_conversation: false,
            home: ConnectionPage::Account,
        }
    }

    pub fn settings(using_plan: bool, model: Option<&str>, theme: &Theme) -> Self {
        let mut panel = Self::open();
        panel.page = ConnectionPage::Settings;
        panel.home = ConnectionPage::Settings;
        panel.current_model = model.map(str::to_owned);
        panel.current_source = if using_plan {
            ModelSource::ChatGpt
        } else {
            ModelSource::Environment
        };
        panel.model_source = panel.current_source;
        panel.sync_from_theme(theme);
        panel.status = "Changes apply now and are saved on this device.".to_owned();
        if !using_plan && model.is_some() {
            panel.selection = panel
                .choices()
                .iter()
                .position(|(_, action)| *action == ConnectionAction::EnvironmentModels)
                .unwrap_or(0);
        }
        panel
    }

    pub fn sync_from_theme(&mut self, theme: &Theme) {
        self.theme = *theme;
    }

    pub fn current_model_for_source(&self) -> Option<&str> {
        (self.current_source == self.model_source)
            .then_some(self.current_model.as_deref())
            .flatten()
    }

    pub fn refresh_action(&self) -> ConnectionAction {
        match self.model_source {
            ModelSource::ChatGpt => ConnectionAction::Models,
            ModelSource::Environment => ConnectionAction::EnvironmentModels,
        }
    }

    pub fn begin_models(&mut self, source: ModelSource) {
        if self.model_source != source {
            self.models.clear();
            self.clear_query();
        }
        self.model_source = source;
        self.page = ConnectionPage::Models;
        self.clamp_selection();
    }

    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into().chars().take(128).collect();
        self.selection = 0;
    }

    pub fn push_query(&mut self, character: char) {
        if self.query.chars().count() < 128 {
            self.query.push(character);
            self.selection = 0;
        }
    }

    pub fn pop_query(&mut self) {
        if let Some((start, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(start);
            self.selection = 0;
        }
    }

    pub fn clear_query(&mut self) {
        self.query.clear();
        self.selection = 0;
    }

    pub fn clamp_selection(&mut self) {
        self.selection = self.selection.min(self.choices().len().saturating_sub(1));
    }

    pub fn move_selection(&mut self, delta: isize) {
        self.selection = self.selection.saturating_add_signed(delta);
        self.clamp_selection();
    }

    pub fn select_current(&mut self) {
        self.selection = self
            .current_model_for_source()
            .and_then(|current| {
                self.filtered_models()
                    .iter()
                    .position(|(id, _)| id == current)
            })
            .unwrap_or(0);
    }

    fn filtered_models(&self) -> Vec<&(String, String)> {
        let terms: Vec<_> = self
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.models
            .iter()
            .filter(|(id, name)| {
                let text = format!("{id} {name}").to_lowercase();
                terms.iter().all(|term| text.contains(term))
            })
            .collect()
    }

    pub fn choices(&self) -> Vec<(String, ConnectionAction)> {
        match self.page {
            ConnectionPage::Settings => {
                let mut choices = vec![if self
                    .accounts
                    .iter()
                    .any(|account| account.active && account.signed_in)
                {
                    (
                        "Use ChatGPT plan / choose model".to_owned(),
                        ConnectionAction::Models,
                    )
                } else {
                    (
                        "Continue with ChatGPT".to_owned(),
                        ConnectionAction::Login(false),
                    )
                }];
                choices.push((
                    "Manage ChatGPT account".to_owned(),
                    ConnectionAction::OpenAccount,
                ));
                choices.push((
                    "Choose API model".to_owned(),
                    ConnectionAction::EnvironmentModels,
                ));
                choices.push((
                    "Use environment configuration".to_owned(),
                    ConnectionAction::Environment,
                ));
                if self.current_source == ModelSource::ChatGpt
                    || self.accounts.iter().any(|account| account.signed_in)
                {
                    choices.push(("Manage usage".to_owned(), ConnectionAction::ManageUsage));
                }
                choices.push((
                    format!(
                        "Motion: {}",
                        if self.theme.reduced_motion {
                            "reduced"
                        } else {
                            "animated"
                        }
                    ),
                    ConnectionAction::ToggleMotion,
                ));
                choices.push((
                    format!(
                        "Background: {}",
                        if self.theme.paint_background {
                            "Knut"
                        } else {
                            "terminal"
                        }
                    ),
                    ConnectionAction::ToggleBackground,
                ));
                choices
            }
            ConnectionPage::Welcome => vec![("Got it".to_owned(), ConnectionAction::Acknowledge)],
            ConnectionPage::Models => self
                .filtered_models()
                .iter()
                .map(|(id, name)| {
                    (
                        if id == name {
                            id.clone()
                        } else {
                            format!("{id} — {name}")
                        },
                        match self.model_source {
                            ModelSource::ChatGpt => ConnectionAction::SelectModel(id.clone()),
                            ModelSource::Environment => {
                                ConnectionAction::SelectEnvironmentModel(id.clone())
                            }
                        },
                    )
                })
                .collect(),
            ConnectionPage::Account => {
                let mut choices = vec![(
                    "Continue with ChatGPT".to_owned(),
                    ConnectionAction::Login(false),
                )];
                if self.accounts.iter().any(|account| account.signed_in) {
                    choices.push(("Choose model".to_owned(), ConnectionAction::Models));
                }
                for account in self.accounts.iter().filter(|a| a.signed_in && !a.active) {
                    choices.push((
                        format!("Use {}", account.label),
                        ConnectionAction::SelectAccount(account.id.clone()),
                    ));
                }
                if !self.accounts.is_empty() {
                    choices.push((
                        "Add another account".to_owned(),
                        ConnectionAction::Login(true),
                    ));
                }
                if self.accounts.iter().any(|a| a.active && a.signed_in) {
                    choices.push(("Sign out".to_owned(), ConnectionAction::Logout));
                }
                choices.push(("Manage usage".to_owned(), ConnectionAction::ManageUsage));
                choices
            }
        }
    }
}

pub(crate) enum ConnectionOutcome {
    Models {
        source: ModelSource,
        models: Option<Vec<(String, String)>>,
        notice: bool,
        error: Option<String>,
    },
    Connected {
        model: String,
        endpoint: String,
        plan: bool,
        account: Option<String>,
    },
    LoggedOut(bool),
    Acknowledged,
    BrowserOpened,
}

fn model_options(catalog: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen = std::collections::BTreeSet::new();
    catalog
        .into_iter()
        .filter(|(id, _)| !id.trim().is_empty() && seen.insert(id.clone()))
        .collect()
}

async fn catalog(source: ModelSource) -> Result<ConnectionOutcome, KnutError> {
    let notice = source == ModelSource::ChatGpt && openai_auth::needs_plan_notice()?;
    let config = match source {
        // Catalog requests do not use a model ID, but the adapter requires one.
        ModelSource::ChatGpt => ProviderConfig::chatgpt("catalog")?,
        ModelSource::Environment => {
            let config = ProviderConfig::from_environment()?;
            if config.uses_chatgpt_plan() {
                return Err(KnutError::Model(
                    "The configured connection uses ChatGPT. Choose ChatGPT models instead."
                        .to_owned(),
                ));
            }
            config
        }
    };
    let result = ProviderModel::new(config)?.list_models().await;
    let (models, error) = match result {
        Ok(models) if !models.is_empty() => (Some(model_options(models)), None),
        Ok(_) => (
            Some(Vec::new()),
            Some("The provider returned no available models. Refresh to retry.".to_owned()),
        ),
        Err(error) => (
            None,
            Some(format!("Could not load models: {error}. Refresh to retry.")),
        ),
    };
    Ok(ConnectionOutcome::Models {
        source,
        notice: notice && models.as_ref().is_some_and(|models| !models.is_empty()),
        models,
        error,
    })
}

pub(crate) async fn execute(
    action: ConnectionAction,
    connections: tokio::sync::mpsc::UnboundedSender<ConnectionRequest>,
    using_plan: bool,
    progress: tokio::sync::mpsc::UnboundedSender<&'static str>,
) -> Result<ConnectionOutcome, KnutError> {
    if matches!(
        action,
        ConnectionAction::Login(_) | ConnectionAction::SelectAccount(_) | ConnectionAction::Logout
    ) {
        update_engine(&connections, ConnectionChange::CheckIdle).await?;
    }
    match action {
        ConnectionAction::Login(new) => {
            openai_auth::login_in_tui(new).await?;
            let _ = progress.send("Signed in. Loading available models…");
            catalog(ModelSource::ChatGpt).await
        }
        ConnectionAction::SelectAccount(id) => {
            openai_auth::select_account(&id).await?;
            let _ = progress.send("Account selected. Loading available models…");
            catalog(ModelSource::ChatGpt).await
        }
        ConnectionAction::Models => catalog(ModelSource::ChatGpt).await,
        ConnectionAction::EnvironmentModels => catalog(ModelSource::Environment).await,
        ConnectionAction::SelectModel(model) => {
            let config = ProviderConfig::chatgpt(model.clone())?;
            let account = config.chatgpt_client().and_then(openai_auth::account_label);
            update_engine(&connections, ConnectionChange::Use(Some(config))).await?;
            Ok(ConnectionOutcome::Connected {
                model,
                endpoint: "api.openai.com".to_owned(),
                plan: true,
                account,
            })
        }
        ConnectionAction::SelectEnvironmentModel(model) => {
            let config = ProviderConfig::from_environment()?.with_model(model.clone())?;
            if config.uses_chatgpt_plan() {
                return Err(KnutError::Model(
                    "Choose this model through the ChatGPT connection.".to_owned(),
                ));
            }
            let endpoint = reqwest::Url::parse(config.base_url())
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_default();
            update_engine(&connections, ConnectionChange::Use(Some(config))).await?;
            Ok(ConnectionOutcome::Connected {
                model,
                endpoint,
                plan: false,
                account: None,
            })
        }
        ConnectionAction::Environment => {
            let config = ProviderConfig::environment_default()?;
            let model = config.model().to_owned();
            let endpoint = reqwest::Url::parse(config.base_url())
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_default();
            let plan = config.uses_chatgpt_plan();
            let account = config.chatgpt_client().and_then(openai_auth::account_label);
            update_engine(&connections, ConnectionChange::Environment(config)).await?;
            Ok(ConnectionOutcome::Connected {
                model,
                endpoint,
                plan,
                account,
            })
        }
        ConnectionAction::OpenAccount
        | ConnectionAction::ToggleMotion
        | ConnectionAction::ToggleBackground => {
            unreachable!("local settings are handled by the shell")
        }
        ConnectionAction::Logout => {
            let revoked = openai_auth::logout().await?;
            if using_plan {
                update_engine(&connections, ConnectionChange::Use(None)).await?;
            }
            Ok(ConnectionOutcome::LoggedOut(revoked))
        }
        ConnectionAction::ManageUsage => {
            openai_auth::open_browser("https://chatgpt.com/settings/usage").await?;
            Ok(ConnectionOutcome::BrowserOpened)
        }
        ConnectionAction::Acknowledge => {
            openai_auth::acknowledge_plan_notice().await?;
            Ok(ConnectionOutcome::Acknowledged)
        }
    }
}

async fn update_engine(
    connections: &tokio::sync::mpsc::UnboundedSender<ConnectionRequest>,
    change: ConnectionChange,
) -> Result<(), KnutError> {
    let (reply, result) = tokio::sync::oneshot::channel();
    connections
        .send(ConnectionRequest { change, reply })
        .map_err(|_| KnutError::Model("The engine has stopped; restart Knut".to_owned()))?;
    result
        .await
        .map_err(|_| KnutError::Model("Connection update was interrupted".to_owned()))?
}

pub(crate) struct ConnectionJob {
    pub progress: tokio::sync::mpsc::UnboundedReceiver<&'static str>,
    pub handle: Option<tokio::task::JoinHandle<Result<ConnectionOutcome, KnutError>>>,
}
impl Drop for ConnectionJob {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_keeps_only_returned_models_and_deduplicates_ids() {
        let catalog = vec![
            ("alpha".to_owned(), "Provider Alpha".to_owned()),
            ("beta".to_owned(), "Provider Beta".to_owned()),
        ];
        let models = model_options(catalog.clone());
        assert_eq!(model_options(models.clone()), models);
        assert_eq!(models, catalog);
        assert_eq!(
            model_options(vec![catalog[0].clone(), catalog[0].clone()]),
            vec![catalog[0].clone()]
        );
        assert!(model_options(Vec::new()).is_empty());
    }

    #[test]
    fn model_search_matches_ids_and_names_without_selectable_fallback_actions() {
        let mut panel = ConnectionPanel::open();
        panel.page = ConnectionPage::Models;
        panel.models = vec![
            ("alpha".into(), "Fast Model".into()),
            ("beta".into(), "Deep Model".into()),
        ];
        panel.set_query("BETA deep");
        assert_eq!(
            panel.choices(),
            vec![(
                "beta — Deep Model".to_owned(),
                ConnectionAction::SelectModel("beta".to_owned())
            )]
        );
        panel.set_query("unavailable");
        assert!(panel.choices().is_empty());
        panel.move_selection(10);
        assert_eq!(panel.selection, 0);
        panel.set_query("e\u{301}");
        panel.pop_query();
        assert!(panel.query.is_empty());
    }

    #[test]
    fn model_current_marker_and_actions_follow_the_provider_source() {
        let mut panel = ConnectionPanel::settings(false, Some("beta"), &Theme::plain());
        panel.models = vec![
            ("alpha".into(), "Alpha".into()),
            ("beta".into(), "Beta".into()),
        ];
        panel.page = ConnectionPage::Models;
        panel.select_current();
        assert_eq!(panel.selection, 1);
        assert_eq!(panel.current_model_for_source(), Some("beta"));
        assert_eq!(
            panel.choices()[1].1,
            ConnectionAction::SelectEnvironmentModel("beta".into())
        );
        assert_eq!(panel.refresh_action(), ConnectionAction::EnvironmentModels);
        panel.move_selection(-100);
        assert_eq!(panel.selection, 0);
        panel.move_selection(100);
        assert_eq!(panel.selection, 1);
        panel.begin_models(ModelSource::ChatGpt);
        assert!(panel.models.is_empty());
        assert!(panel.current_model_for_source().is_none());
        assert_eq!(panel.refresh_action(), ConnectionAction::Models);
    }
}

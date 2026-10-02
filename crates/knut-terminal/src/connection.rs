use crate::openai_auth::{self, AccountInfo};
use crate::{ConnectionChange, ConnectionRequest, KnutError, ProviderConfig, ProviderModel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionAction {
    Login(bool),
    Models,
    SelectAccount(String),
    SelectModel(String),
    Logout,
    ManageUsage,
    Acknowledge,
    OpenAccount,
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
            selection: 0,
            busy: false,
            cancellable: true,
            status: "Connect your ChatGPT plan to Knut's coding harness.".to_owned(),
            error: accounts.err().map(|error| error.to_string()),
            return_to_conversation: false,
            home: ConnectionPage::Account,
        }
    }

    pub fn settings(using_plan: bool, model: Option<&str>) -> Self {
        let mut panel = Self::open();
        panel.page = ConnectionPage::Settings;
        panel.home = ConnectionPage::Settings;
        panel.status = format!(
            "Connection: {}. Model: {}. Changes are saved for the next start.",
            if using_plan {
                "ChatGPT plan"
            } else {
                "environment configuration"
            },
            model.unwrap_or("not connected")
        );
        panel
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
                    "Use environment configuration".to_owned(),
                    ConnectionAction::Environment,
                ));
                choices.push(("Manage usage".to_owned(), ConnectionAction::ManageUsage));
                choices
            }
            ConnectionPage::Welcome => vec![("Got it".to_owned(), ConnectionAction::Acknowledge)],
            ConnectionPage::Models => {
                let mut choices: Vec<_> = self
                    .models
                    .iter()
                    .map(|(id, name)| {
                        (
                            if id == name {
                                id.clone()
                            } else {
                                format!("{id} — {name}")
                            },
                            ConnectionAction::SelectModel(id.clone()),
                        )
                    })
                    .collect();
                choices.push((
                    "Refresh available models".to_owned(),
                    ConnectionAction::Models,
                ));
                choices.push(("Manage usage".to_owned(), ConnectionAction::ManageUsage));
                choices
            }
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
        models: Vec<(String, String)>,
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

fn model_options(mut catalog: Vec<(String, String)>) -> Vec<(String, String)> {
    // The account catalog can omit models that accept this same account's token.
    let mut models = Vec::new();
    for (id, name) in [
        ("gpt-6.1-sol", "GPT-6.1 Sol"),
        ("gpt-6-astra", "GPT-6 Astra"),
        ("gpt-6-luna", "GPT-6 Luna"),
    ] {
        let entry = match catalog.iter().position(|(slug, _)| slug == id) {
            Some(index) => catalog.remove(index),
            None => (id.to_owned(), name.to_owned()),
        };
        models.push(entry);
    }
    models.extend(catalog);
    models
}

async fn catalog() -> Result<ConnectionOutcome, KnutError> {
    let notice = openai_auth::needs_plan_notice()?;
    let result = ProviderModel::new(ProviderConfig::chatgpt("gpt-6.1-sol")?)?
        .list_models()
        .await;
    let (models, error) = match result {
        Ok(models) if !models.is_empty() => (model_options(models), None),
        Ok(_) => (
            Vec::new(),
            Some("No models are available for this account. Manage usage or retry.".to_owned()),
        ),
        Err(error) => (
            Vec::new(),
            Some(format!(
                "Could not load models: {error}. Retry or manage usage."
            )),
        ),
    };
    Ok(ConnectionOutcome::Models {
        models,
        notice,
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
            catalog().await
        }
        ConnectionAction::SelectAccount(id) => {
            openai_auth::select_account(&id).await?;
            let _ = progress.send("Account selected. Loading available models…");
            catalog().await
        }
        ConnectionAction::Models => catalog().await,
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
        ConnectionAction::Environment => {
            let config = ProviderConfig::from_environment()?;
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
        ConnectionAction::OpenAccount => unreachable!("navigation is handled by the shell"),
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
    fn incomplete_catalog_still_offers_current_models_without_duplicates() {
        let catalog = vec![
            ("gpt-6-astra".to_owned(), "Account Astra".to_owned()),
            ("gpt-5.6-sol".to_owned(), "GPT-5.6 Sol".to_owned()),
        ];
        let models = model_options(catalog.clone());
        assert_eq!(model_options(models.clone()), models);
        assert_eq!(models.len(), 4);
        assert_eq!(models[1], catalog[0]);
        assert_eq!(models[3], catalog[1]);

        let mut panel = ConnectionPanel::open();
        panel.page = ConnectionPage::Models;
        panel.models = models;
        let choices = panel.choices();
        assert_eq!(
            choices[0],
            (
                "gpt-6.1-sol — GPT-6.1 Sol".to_owned(),
                ConnectionAction::SelectModel("gpt-6.1-sol".to_owned()),
            )
        );
        assert_eq!(
            choices[2].1,
            ConnectionAction::SelectModel("gpt-6-luna".to_owned())
        );
        assert_eq!(choices[4].1, ConnectionAction::Models);
    }
}

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionPage {
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
        }
    }

    pub fn choices(&self) -> Vec<(String, ConnectionAction)> {
        match self.page {
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
        account: Option<String>,
    },
    LoggedOut(bool),
    Acknowledged,
    BrowserOpened,
}

pub(crate) fn account_label(client_id: &str) -> Option<String> {
    openai_auth::account_list()
        .ok()?
        .into_iter()
        .find(|account| account.id == client_id && account.signed_in)
        .map(|account| account.label)
}

async fn catalog() -> Result<ConnectionOutcome, KnutError> {
    let notice = openai_auth::needs_plan_notice()?;
    let result = ProviderModel::new(ProviderConfig::chatgpt("gpt-6.1-sol")?)?
        .list_models()
        .await;
    let (models, error) = match result {
        Ok(models) if !models.is_empty() => (models, None),
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
            let account = config.chatgpt_client().and_then(account_label);
            update_engine(&connections, ConnectionChange::Use(Some(config))).await?;
            Ok(ConnectionOutcome::Connected { model, account })
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

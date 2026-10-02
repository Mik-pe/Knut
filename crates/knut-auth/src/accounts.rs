use crate::oauth::PLAN_SCOPE;
use crate::store::{Store, lock_store};
use crate::{AuthError, auth_error};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub id: String,
    pub label: String,
    pub active: bool,
    pub signed_in: bool,
}

pub fn account_list() -> Result<Vec<AccountInfo>, AuthError> {
    let accounts = Store::configured()?.read()?;
    Ok(accounts
        .registrations
        .iter()
        .map(|r| AccountInfo {
            id: r.client_id.clone(),
            label: r.label().to_owned(),
            active: accounts.active.as_deref() == Some(&r.client_id),
            signed_in: r.tokens.is_some(),
        })
        .collect())
}

pub fn saved_model() -> Result<Option<String>, AuthError> {
    Ok(Store::configured()?.read()?.preferred_model)
}

pub async fn use_environment() -> Result<(), AuthError> {
    let mut store = lock_store().await?;
    store.accounts.preferred_model = None;
    store.save()
}

pub fn needs_plan_notice() -> Result<bool, AuthError> {
    Ok(!Store::configured()?.read()?.plan_notice_seen)
}

pub async fn acknowledge_plan_notice() -> Result<(), AuthError> {
    let mut store = lock_store().await?;
    store.accounts.plan_notice_seen = true;
    store.save()
}

pub async fn save_model(model: &str, client_id: &str) -> Result<(), AuthError> {
    let mut store = lock_store().await?;
    let registration = store
        .accounts
        .registrations
        .iter()
        .find(|r| r.client_id == client_id)
        .ok_or_else(|| auth_error("Sign in first"))?;
    let tokens = registration
        .tokens
        .as_ref()
        .ok_or_else(|| auth_error("Sign in first"))?;
    if !tokens.scopes.iter().any(|scope| scope == PLAN_SCOPE) {
        return Err(auth_error("ChatGPT plan usage is not authorized"));
    }
    store.accounts.active = Some(client_id.to_owned());
    store.accounts.preferred_model = Some(model.to_owned());
    store.save()
}

/// Saved account labels, with active and signed-in status. Credentials stay private.
pub fn accounts() -> Result<Vec<String>, AuthError> {
    Ok(account_list()?
        .into_iter()
        .map(|account| {
            format!(
                "{} {} — {}{}",
                if account.active { "*" } else { " " },
                account.id,
                account.label,
                if account.signed_in {
                    ""
                } else {
                    " (signed out)"
                },
            )
        })
        .collect())
}

/// Select a saved, signed-in registration without combining account credentials.
pub async fn select_account(client_id: &str) -> Result<(), AuthError> {
    let mut store = lock_store().await?;
    if !store
        .accounts
        .registrations
        .iter()
        .any(|r| r.client_id == client_id && r.tokens.is_some())
    {
        return Err(auth_error("No signed-in ChatGPT registration has that ID"));
    }
    store.accounts.active = Some(client_id.to_owned());
    store.save()
}

pub fn account_label(client_id: &str) -> Option<String> {
    account_list()
        .ok()?
        .into_iter()
        .find(|account| account.id == client_id && account.signed_in)
        .map(|account| account.label)
}

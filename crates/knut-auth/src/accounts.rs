use crate::oauth::PLAN_SCOPE;
use crate::store::{LockedStore, Store, lock_store};
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

pub fn saved_api_model(identity: &str) -> Result<Option<String>, AuthError> {
    saved_api_model_from(&Store::configured()?, identity)
}

fn saved_api_model_from(store: &Store, identity: &str) -> Result<Option<String>, AuthError> {
    validate_api_identity(identity)?;
    let model = store.read()?.api_models.get(identity).cloned();
    if let Some(model) = &model {
        validate_api_model(model)?;
    }
    Ok(model)
}

pub async fn save_api_model(identity: &str, model: &str) -> Result<(), AuthError> {
    let mut store = lock_store().await?;
    store.save_api_model(identity, model)
}

impl LockedStore {
    fn save_api_model(&mut self, identity: &str, model: &str) -> Result<(), AuthError> {
        validate_api_identity(identity)?;
        validate_api_model(model)?;
        self.accounts
            .api_models
            .insert(identity.to_owned(), model.to_owned());
        self.accounts.preferred_model = None;
        self.save()
    }

    fn use_environment(&mut self) -> Result<(), AuthError> {
        self.accounts.preferred_model = None;
        self.accounts.api_models.clear();
        self.save()
    }
}

fn validate_api_identity(identity: &str) -> Result<(), AuthError> {
    if identity.len() != 64
        || !identity
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(auth_error(
            "Provider preference identity must be a SHA-256 digest",
        ));
    }
    Ok(())
}

fn validate_api_model(model: &str) -> Result<(), AuthError> {
    if model.trim().is_empty() || model.len() > 1024 || model.chars().any(char::is_control) {
        return Err(auth_error("Invalid provider model ID"));
    }
    Ok(())
}

pub async fn use_environment() -> Result<(), AuthError> {
    let mut store = lock_store().await?;
    store.use_environment()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_model_selection_and_environment_reset_survive_restart_atomically() {
        let dir =
            std::env::temp_dir().join(format!("knut-models-{}", crate::random_value().unwrap()));
        let mut store = Store { dir: dir.clone() }.lock().unwrap();
        store.accounts.preferred_model = Some("chatgpt-model".to_owned());
        store.save().unwrap();
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        store.save_api_model(&first, "provider-model").unwrap();
        store.save_api_model(&second, "other-model").unwrap();
        let restarted = store.store.read().unwrap();
        assert!(restarted.preferred_model.is_none());
        assert_eq!(restarted.api_models.get(&first).unwrap(), "provider-model");
        assert_eq!(restarted.api_models.get(&second).unwrap(), "other-model");

        let original = std::fs::read(dir.join("openai-accounts.json")).unwrap();
        assert!(
            store
                .save_api_model("https://user:secret@example.com", "model")
                .is_err()
        );
        assert!(store.save_api_model(&first, "bad\nmodel").is_err());
        assert_eq!(
            std::fs::read(dir.join("openai-accounts.json")).unwrap(),
            original
        );

        store.accounts.preferred_model = Some("chatgpt-model".to_owned());
        store.use_environment().unwrap();
        let reset = store.store.read().unwrap();
        assert!(reset.preferred_model.is_none());
        assert!(reset.api_models.is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_account_records_have_no_api_override() {
        let accounts: crate::store::Accounts = serde_json::from_value(serde_json::json!({
            "host_id": "host", "active": null, "registrations": [], "preferred_model": "plan-model"
        }))
        .unwrap();
        assert!(accounts.api_models.is_empty());
        assert_eq!(accounts.preferred_model.as_deref(), Some("plan-model"));
    }

    #[test]
    fn malformed_saved_api_models_are_rejected_on_read() {
        let dir = std::env::temp_dir().join(format!(
            "knut-models-read-{}",
            crate::random_value().unwrap()
        ));
        let mut store = Store { dir: dir.clone() }.lock().unwrap();
        let identity = "a".repeat(64);
        for model in [String::new(), "bad\nmodel".to_owned(), "x".repeat(1025)] {
            store.accounts.api_models.insert(identity.clone(), model);
            store.save().unwrap();
            assert!(saved_api_model_from(&store.store, &identity).is_err());
        }
        assert!(saved_api_model_from(&store.store, "A".repeat(64).as_str()).is_err());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

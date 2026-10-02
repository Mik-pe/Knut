mod accounts;
mod oauth;
mod store;

use std::fs::File;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

pub use accounts::{
    AccountInfo, account_label, account_list, accounts, acknowledge_plan_notice, needs_plan_notice,
    save_model, saved_model, select_account, use_environment,
};
pub use oauth::{access_token, login, login_in_tui, logout, open_browser, signed_in_client};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("model provider authentication failed: {0}")]
    ModelAuth(String),
}

pub(crate) fn auth_error(message: &str) -> AuthError {
    AuthError::ModelAuth(message.to_owned())
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_bytes<const N: usize>() -> Result<[u8; N], AuthError> {
    let mut bytes = [0; N];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| auth_error("OS randomness unavailable"))?;
    Ok(bytes)
}

pub(crate) fn random_value() -> Result<String, AuthError> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?))
}

pub(crate) fn host_id() -> Result<String, AuthError> {
    let mut b = random_bytes::<16>()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex = b.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok(format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

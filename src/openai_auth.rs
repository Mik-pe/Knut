//! Sign in with ChatGPT for locally hosted, open-source inference.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::KnutError;

const ISSUER: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

#[derive(Clone, Serialize, Deserialize)]
struct Registration {
    client_id: String,
    subject: String,
    email: String,
    #[serde(default)]
    tokens: Option<Tokens>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_at: u64,
    scopes: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Accounts {
    host_id: String,
    active: Option<String>,
    registrations: Vec<Registration>,
    #[serde(default)]
    preferred_model: Option<String>,
    #[serde(default)]
    plan_notice_seen: bool,
}

impl Accounts {
    fn selected(&self) -> Result<&Registration, KnutError> {
        self.registrations
            .iter()
            .find(|r| Some(&r.client_id) == self.active.as_ref())
            .ok_or_else(|| auth_error("Run `knut login openai-codex` to sign in with ChatGPT"))
    }
}

struct Store {
    dir: PathBuf,
}
struct LockedStore {
    store: Store,
    _lock: File,
    accounts: Accounts,
}

fn auth_error(message: &str) -> KnutError {
    KnutError::ModelAuth(message.to_owned())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_bytes<const N: usize>() -> Result<[u8; N], KnutError> {
    let mut bytes = [0; N];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| auth_error("OS randomness unavailable"))?;
    Ok(bytes)
}

fn random_value() -> Result<String, KnutError> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?))
}

fn host_id() -> Result<String, KnutError> {
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

fn private_metadata(metadata: &std::fs::Metadata) -> bool {
    // SAFETY: geteuid has no preconditions.
    metadata.uid() == unsafe { libc::geteuid() } && metadata.permissions().mode() & 0o077 == 0
}

impl Store {
    fn configured() -> Result<Self, KnutError> {
        let dir = if let Some(dir) = std::env::var_os("KNUT_CONFIG_DIR") {
            PathBuf::from(dir)
        } else if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(dir).join("knut")
        } else {
            PathBuf::from(std::env::var_os("HOME").ok_or_else(|| auth_error("HOME is not set"))?)
                .join(".config/knut")
        };
        Ok(Self { dir })
    }

    fn prepare(&self) -> Result<(), KnutError> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|_| auth_error("Cannot create Knut credential directory"))?;
        let metadata = std::fs::symlink_metadata(&self.dir)
            .map_err(|_| auth_error("Cannot inspect Knut credential directory"))?;
        if !metadata.is_dir() || !private_metadata(&metadata) {
            return Err(auth_error(
                "Knut credential directory must be owned by you with permissions 0700",
            ));
        }
        Ok(())
    }

    fn read(&self) -> Result<Accounts, KnutError> {
        self.prepare()?;
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.dir.join("openai-accounts.json"))
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Accounts::default()),
            Err(_) => return Err(auth_error("Cannot read ChatGPT credentials")),
        };
        let metadata = file
            .metadata()
            .map_err(|_| auth_error("Cannot inspect ChatGPT credentials"))?;
        if !metadata.is_file() || !private_metadata(&metadata) || metadata.len() > 1024 * 1024 {
            return Err(auth_error(
                "ChatGPT credentials require an owner-only regular file",
            ));
        }
        serde_json::from_reader(file).map_err(|_| {
            auth_error("Invalid ChatGPT credential record; restore it or sign in again")
        })
    }

    fn lock(self) -> Result<LockedStore, KnutError> {
        self.prepare()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.dir.join("openai-auth.lock"))
            .map_err(|_| auth_error("Cannot lock ChatGPT credentials"))?;
        if !private_metadata(
            &lock
                .metadata()
                .map_err(|_| auth_error("Cannot inspect credential lock"))?,
        ) {
            return Err(auth_error("ChatGPT credential lock is not private"));
        }
        // SAFETY: the descriptor is valid and remains held by LockedStore.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(auth_error("Cannot lock ChatGPT credentials"));
        }
        let accounts = self.read()?;
        Ok(LockedStore {
            store: self,
            _lock: lock,
            accounts,
        })
    }
}

impl LockedStore {
    fn save(&self) -> Result<(), KnutError> {
        let path = self
            .store
            .dir
            .join(format!(".openai-accounts-{}", random_value()?));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|_| auth_error("Cannot save ChatGPT credentials"))?;
            serde_json::to_writer(&mut file, &self.accounts)
                .map_err(|_| auth_error("Cannot encode ChatGPT credentials"))?;
            file.flush()
                .and_then(|_| file.sync_all())
                .map_err(|_| auth_error("Cannot sync ChatGPT credentials"))?;
            std::fs::rename(&path, self.store.dir.join("openai-accounts.json"))
                .map_err(|_| auth_error("Cannot replace ChatGPT credentials"))?;
            File::open(&self.store.dir)
                .and_then(|f| f.sync_all())
                .map_err(|_| auth_error("Cannot sync credential directory"))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(path);
        }
        result
    }
}

async fn lock_store() -> Result<LockedStore, KnutError> {
    let store = Store::configured()?;
    tokio::task::spawn_blocking(move || store.lock())
        .await
        .map_err(|_| auth_error("Credential lock task failed"))?
}

fn client() -> Result<reqwest::Client, KnutError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| auth_error("Cannot build OpenAI sign-in client"))
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
}

impl TokenReply {
    fn tokens(self, previous: Option<&Tokens>) -> Result<Tokens, KnutError> {
        if !self.token_type.eq_ignore_ascii_case("bearer")
            || self.access_token.is_empty()
            || self.expires_in == 0
        {
            return Err(auth_error("OpenAI returned an invalid token set"));
        }
        let scopes = self
            .scope
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .or_else(|| previous.map(|p| p.scopes.clone()))
            .ok_or_else(|| auth_error("OpenAI did not grant scopes"))?;
        let mut tokens = Tokens {
            access_token: self.access_token,
            refresh_token: self
                .refresh_token
                .or_else(|| previous.map(|p| p.refresh_token.clone()))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| auth_error("OpenAI did not issue a renewable session"))?,
            id_token: self
                .id_token
                .or_else(|| previous.map(|p| p.id_token.clone()))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| auth_error("OpenAI did not issue an ID token"))?,
            expires_at: now()
                .checked_add(self.expires_in)
                .ok_or_else(|| auth_error("Invalid token expiration"))?,
            scopes,
        };
        if !tokens.scopes.iter().any(|s| s == PLAN_SCOPE) {
            tokens.access_token.clear();
            return Err(auth_error(
                "ChatGPT plan usage was not granted; run `knut login openai-codex` and approve plan usage",
            ));
        }
        Ok(tokens)
    }
}

async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenReply, KnutError> {
    let response = http
        .post(endpoint)
        .form(form)
        .send()
        .await
        .map_err(|_| auth_error("OpenAI token request failed; retry sign-in"))?;
    if !response.status().is_success() {
        return Err(auth_error(&format!(
            "OpenAI token request failed (HTTP {}); run `knut login openai-codex`",
            response.status().as_u16()
        )));
    }
    response
        .json()
        .await
        .map_err(|_| auth_error("Invalid OpenAI token response"))
}

/// Confirm an active account has consented to ChatGPT plan usage, without network requests.
pub(crate) fn signed_in_client() -> Result<String, KnutError> {
    let accounts = Store::configured()?.read()?;
    let tokens = accounts
        .selected()?
        .tokens
        .as_ref()
        .ok_or_else(|| auth_error("Run `knut login openai-codex` to sign in"))?;
    if tokens.access_token.is_empty() || !tokens.scopes.iter().any(|s| s == PLAN_SCOPE) {
        return Err(auth_error(
            "ChatGPT plan usage is not authorized; run `knut login openai-codex`",
        ));
    }
    Ok(accounts.selected()?.client_id.clone())
}

pub(crate) async fn access_token(client_id: &str) -> Result<String, KnutError> {
    let mut store = lock_store().await?;
    refresh_account(&mut store, &client()?, TOKEN, client_id).await
}

async fn refresh_account(
    store: &mut LockedStore,
    http: &reqwest::Client,
    endpoint: &str,
    client_id: &str,
) -> Result<String, KnutError> {
    let selected = store
        .accounts
        .registrations
        .iter()
        .find(|r| r.client_id == client_id)
        .cloned()
        .ok_or_else(|| {
            auth_error("The connected ChatGPT account is no longer registered; reconnect")
        })?;
    let tokens = selected
        .tokens
        .as_ref()
        .ok_or_else(|| auth_error("Run `knut login openai-codex`"))?;
    if !tokens.scopes.iter().any(|s| s == PLAN_SCOPE) {
        return Err(auth_error("ChatGPT plan usage is not authorized"));
    }
    if tokens.expires_at > now() + 60 {
        return Ok(tokens.access_token.clone());
    }
    let reply = token_request(
        http,
        endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &selected.client_id),
            ("refresh_token", &tokens.refresh_token),
            ("resource", RESOURCE),
        ],
    )
    .await?;
    if let Some(id_token) = &reply.id_token {
        let identity = validate_identity(
            id_token,
            &selected.client_id,
            None,
            &signing_keys(http).await?,
        )?;
        if identity.sub != selected.subject {
            return Err(auth_error("Refresh returned a different account identity"));
        }
    }
    let replacement = reply.tokens(Some(tokens))?;
    let access = replacement.access_token.clone();
    store
        .accounts
        .registrations
        .iter_mut()
        .find(|r| r.client_id == selected.client_id)
        .unwrap()
        .tokens = Some(replacement);
    store.save()?;
    Ok(access)
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
    revocation_endpoint: String,
}

async fn discovery(http: &reqwest::Client) -> Result<Discovery, KnutError> {
    let response = http
        .get(format!("{ISSUER}/.well-known/openid-configuration"))
        .send()
        .await
        .map_err(|_| auth_error("OpenAI discovery unavailable"))?;
    if !response.status().is_success() {
        return Err(auth_error("OpenAI discovery failed"));
    }
    let document: Discovery = response
        .json()
        .await
        .map_err(|_| auth_error("Invalid OpenAI discovery document"))?;
    if document.issuer != ISSUER {
        return Err(auth_error("Unexpected OpenAI issuer"));
    }
    for endpoint in [&document.jwks_uri, &document.revocation_endpoint] {
        let url = reqwest::Url::parse(endpoint)
            .map_err(|_| auth_error("Invalid OpenAI discovery endpoint"))?;
        if url.scheme() != "https"
            || url.host_str() != Some("auth.openai.com")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some_and(|p| p != 443)
        {
            return Err(auth_error("Untrusted OpenAI discovery endpoint"));
        }
    }
    Ok(document)
}

async fn signing_keys(http: &reqwest::Client) -> Result<JwkSet, KnutError> {
    let discovery = discovery(http).await?;
    let keys = http
        .get(discovery.jwks_uri)
        .send()
        .await
        .map_err(|_| auth_error("OpenAI signing keys unavailable"))?;
    if !keys.status().is_success() {
        return Err(auth_error("OpenAI signing key request failed"));
    }
    keys.json()
        .await
        .map_err(|_| auth_error("Invalid OpenAI signing keys"))
}

#[derive(Clone, Deserialize)]
struct Claims {
    sub: String,
    iat: u64,
    #[serde(default)]
    email: String,
    #[serde(default)]
    nonce: String,
}

fn validate_identity(
    token: &str,
    client_id: &str,
    nonce: Option<&str>,
    keys: &JwkSet,
) -> Result<Claims, KnutError> {
    let header = decode_header(token).map_err(|_| auth_error("Invalid OpenAI ID token"))?;
    if header.alg != Algorithm::RS256 {
        return Err(auth_error("Unsupported OpenAI ID token algorithm"));
    }
    let key = header
        .kid
        .as_ref()
        .and_then(|kid| keys.find(kid))
        .ok_or_else(|| auth_error("OpenAI ID token signing key missing"))?;
    let key = DecodingKey::from_jwk(key).map_err(|_| auth_error("Invalid OpenAI signing key"))?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["sub", "exp", "iat", "iss", "aud"]);
    validation.leeway = 5;
    let claims = decode::<Claims>(token, &key, &validation)
        .map_err(|_| auth_error("OpenAI ID token verification failed"))?
        .claims;
    if claims.sub.is_empty()
        || nonce.is_some_and(|nonce| claims.nonce != nonce)
        || claims.iat > now() + 5
    {
        return Err(auth_error("OpenAI ID token identity/nonce mismatch"));
    }
    Ok(claims)
}

struct Attempt {
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
    client_id: Option<String>,
}

impl Attempt {
    fn authorization_url(
        &self,
        host: &str,
        previous: Option<&Registration>,
    ) -> Result<reqwest::Url, KnutError> {
        let mut url = reqwest::Url::parse(AUTHORIZE).unwrap();
        let mut params = url.query_pairs_mut();
        params.extend_pairs([
            (
                "client_id",
                self.client_id.as_deref().unwrap_or("dynamic_agent_client"),
            ),
            ("ext_agent_host_id", host),
            ("response_type", "code"),
            ("redirect_uri", &self.redirect),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", &self.state),
            ("nonce", &self.nonce),
            ("code_challenge_method", "S256"),
            (
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes())),
            ),
        ]);
        if self.client_id.is_none() {
            params.append_pair("agent_name_hint", "Knut");
        }
        if let Some(tokens) = previous.and_then(|r| r.tokens.as_ref()) {
            params.append_pair("id_token_hint", &tokens.id_token);
        }
        drop(params);
        Ok(url)
    }

    fn callback(&self, target: &str) -> Result<(String, String), KnutError> {
        let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
            .map_err(|_| auth_error("Invalid sign-in callback"))?;
        if url.path() != "/auth/callback" || url.fragment().is_some() {
            return Err(auth_error("Invalid sign-in callback path"));
        }
        let pairs = url.query_pairs().collect::<Vec<_>>();
        let get = |name: &str| -> Result<Option<String>, KnutError> {
            let values = pairs.iter().filter(|(k, _)| k == name).collect::<Vec<_>>();
            if values.len() > 1 {
                return Err(auth_error("Duplicate sign-in callback field"));
            }
            Ok(values.first().map(|(_, v)| v.to_string()))
        };
        if get("state")?.as_deref() != Some(&self.state) {
            return Err(auth_error("Sign-in state mismatch"));
        }
        if get("error")?.is_some() {
            return Err(auth_error(
                "ChatGPT sign-in was declined; no credentials were changed",
            ));
        }
        let code = get("code")?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| auth_error("Sign-in callback has no code"))?;
        let client_id = match (&self.client_id, get("client_id")?) {
            (Some(expected), Some(actual)) if *expected != actual => {
                return Err(auth_error("Sign-in registration changed unexpectedly"));
            }
            (Some(expected), _) => expected.clone(),
            (None, Some(actual)) if actual != "dynamic_agent_client" && !actual.is_empty() => {
                actual
            }
            _ => return Err(auth_error("ChatGPT client registration is incomplete")),
        };
        Ok((code, client_id))
    }
}

async fn wait_callback(
    listener: tokio::net::TcpListener,
    attempt: &Attempt,
) -> Result<(String, String), KnutError> {
    tokio::time::timeout(Duration::from_secs(600),async {
        loop {
            let (mut socket,_)=listener.accept().await.map_err(|_|auth_error("Sign-in callback listener failed"))?;
            let request=tokio::time::timeout(Duration::from_secs(5),async {
                let mut bytes=Vec::new();
                let mut chunk=[0;1024];
                while bytes.len()<16*1024 {
                    let n=socket.read(&mut chunk).await.map_err(|_|auth_error("Cannot read sign-in callback"))?;
                    if n==0{break;}
                    bytes.extend_from_slice(&chunk[..n]);
                    if bytes.windows(4).any(|w|w==b"\r\n\r\n"){break;}
                }
                String::from_utf8(bytes).map_err(|_|auth_error("Invalid sign-in callback encoding"))
            }).await;
            let Ok(Ok(request))=request else {continue;};
            let mut first=request.lines().next().unwrap_or_default().split_whitespace();
            if first.next()!=Some("GET"){continue;}
            let target=first.next().unwrap_or_default();
            if !target.starts_with("/auth/callback?"){continue;}
            let result=attempt.callback(target);
            let body=if result.is_ok(){"Return to Knut to finish signing in."}else{"Sign-in failed. Return to Knut."};
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
            let _=socket.write_all(response.as_bytes()).await;
            return result;
        }
    }).await.map_err(|_|auth_error("ChatGPT sign-in timed out; run `knut login openai-codex` again"))?
}

/// Register or reauthorize ChatGPT plan usage in the system browser.
pub async fn login(new_account: bool) -> Result<String, KnutError> {
    login_with_output(new_account, true).await
}

pub(crate) async fn login_in_tui(new_account: bool) -> Result<String, KnutError> {
    login_with_output(new_account, false).await
}

async fn login_with_output(new_account: bool, console: bool) -> Result<String, KnutError> {
    let (host, previous) = {
        let mut store = lock_store().await?;
        if store.accounts.host_id.is_empty() {
            store.accounts.host_id = host_id()?;
            store.save()?;
        }
        let previous = if new_account {
            None
        } else {
            store.accounts.selected().ok().cloned()
        };
        (store.accounts.host_id.clone(), previous)
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| auth_error("Cannot start local sign-in callback"))?;
    let port = listener
        .local_addr()
        .map_err(|_| auth_error("Cannot inspect callback address"))?
        .port();
    let attempt = Attempt {
        state: random_value()?,
        nonce: random_value()?,
        verifier: random_value()?,
        redirect: format!("http://127.0.0.1:{port}/auth/callback"),
        client_id: previous.as_ref().map(|r| r.client_id.clone()),
    };
    let url = attempt.authorization_url(&host, previous.as_ref())?;
    let opened = open_browser(url.as_str()).await.is_ok();
    if !opened {
        if !console {
            return Err(auth_error(
                "Cannot open the browser. Run `knut login openai-codex` in a normal terminal, then retry here.",
            ));
        }
        if previous.as_ref().and_then(|r| r.tokens.as_ref()).is_some() {
            return Err(auth_error(
                "Cannot open sign-in browser; use `knut login openai-codex --new` to register in a browser manually",
            ));
        }
        println!("Open this URL in your browser to sign in:\n{url}");
    } else if console {
        println!("Continue signing in with ChatGPT in your browser.");
    }
    let (code, client_id) = wait_callback(listener, &attempt).await?;
    let http = client()?;
    let reply = token_request(
        &http,
        TOKEN,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("code", &code),
            ("code_verifier", &attempt.verifier),
            ("redirect_uri", &attempt.redirect),
            ("resource", RESOURCE),
        ],
    )
    .await?;
    let tokens = reply.tokens(None)?;
    let keys = signing_keys(&http).await?;
    let identity = validate_identity(&tokens.id_token, &client_id, Some(&attempt.nonce), &keys)?;
    if previous.as_ref().is_some_and(|r| r.subject != identity.sub) {
        return Err(auth_error(
            "Sign-in returned a different account; use --new to add it",
        ));
    }
    let email = identity.email.clone();
    let mut store = lock_store().await?;
    if store.accounts.host_id != host {
        return Err(auth_error(
            "Knut host identity changed during sign-in; retry",
        ));
    }
    let registration = Registration {
        client_id: client_id.clone(),
        subject: identity.sub,
        email: identity.email,
        tokens: Some(tokens),
    };
    if let Some(existing) = store
        .accounts
        .registrations
        .iter_mut()
        .find(|r| r.client_id == client_id)
    {
        if existing.subject != registration.subject {
            return Err(auth_error("Registration identity changed"));
        }
        *existing = registration;
    } else {
        store.accounts.registrations.push(registration);
    }
    store.accounts.active = Some(client_id);
    store.save()?;
    Ok(if email.is_empty() {
        "ChatGPT account".to_owned()
    } else {
        email
    })
}

pub(crate) async fn open_browser(url: &str) -> Result<(), KnutError> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = tokio::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|_| auth_error("Cannot open the system browser"))?;
    if status.success() {
        Ok(())
    } else {
        Err(auth_error("Cannot open the system browser"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountInfo {
    pub id: String,
    pub label: String,
    pub active: bool,
    pub signed_in: bool,
}

pub(crate) fn account_list() -> Result<Vec<AccountInfo>, KnutError> {
    let accounts = Store::configured()?.read()?;
    Ok(accounts
        .registrations
        .iter()
        .map(|r| AccountInfo {
            id: r.client_id.clone(),
            label: if r.email.is_empty() {
                "ChatGPT account".to_owned()
            } else {
                r.email.clone()
            },
            active: accounts.active.as_deref() == Some(&r.client_id),
            signed_in: r.tokens.is_some(),
        })
        .collect())
}

pub(crate) fn saved_model() -> Result<Option<String>, KnutError> {
    Ok(Store::configured()?.read()?.preferred_model)
}

pub(crate) async fn use_environment() -> Result<(), KnutError> {
    let mut store = lock_store().await?;
    store.accounts.preferred_model = None;
    store.save()
}

pub(crate) fn needs_plan_notice() -> Result<bool, KnutError> {
    Ok(!Store::configured()?.read()?.plan_notice_seen)
}

pub(crate) async fn acknowledge_plan_notice() -> Result<(), KnutError> {
    let mut store = lock_store().await?;
    store.accounts.plan_notice_seen = true;
    store.save()
}

pub(crate) async fn save_model(model: &str, client_id: &str) -> Result<(), KnutError> {
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
pub fn accounts() -> Result<Vec<String>, KnutError> {
    let accounts = Store::configured()?.read()?;
    Ok(accounts
        .registrations
        .iter()
        .map(|r| {
            format!(
                "{} {} — {}{}",
                if accounts.active.as_deref() == Some(&r.client_id) {
                    "*"
                } else {
                    " "
                },
                r.client_id,
                if r.email.is_empty() {
                    "ChatGPT account"
                } else {
                    &r.email
                },
                if r.tokens.is_none() {
                    " (signed out)"
                } else {
                    ""
                }
            )
        })
        .collect())
}

/// Select a saved, signed-in registration without combining account credentials.
pub async fn select_account(client_id: &str) -> Result<(), KnutError> {
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

/// Revoke the selected renewable session, then clear its local tokens.
pub async fn logout() -> Result<bool, KnutError> {
    let mut store = lock_store().await?;
    let selected = store.accounts.selected()?.clone();
    let mut revoked = selected.tokens.is_none();
    if let Some(tokens) = &selected.tokens {
        let http = client()?;
        if let Ok(discovery) = discovery(&http).await {
            for delay in [0, 1, 2] {
                if delay > 0 {
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
                if http
                    .post(&discovery.revocation_endpoint)
                    .form(&[
                        ("token", &tokens.refresh_token),
                        ("token_type_hint", &"refresh_token".to_owned()),
                        ("client_id", &selected.client_id),
                    ])
                    .send()
                    .await
                    .is_ok_and(|r| r.status() == reqwest::StatusCode::OK)
                {
                    revoked = true;
                    break;
                }
            }
        }
    }
    store
        .accounts
        .registrations
        .iter_mut()
        .find(|r| r.client_id == selected.client_id)
        .unwrap()
        .tokens = None;
    store.accounts.preferred_model = None;
    store.save()?;
    Ok(revoked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(client_id: Option<&str>) -> Attempt {
        Attempt {
            state: "state".into(),
            nonce: "nonce".into(),
            verifier: "verifier".into(),
            redirect: "http://127.0.0.1:1234/auth/callback".into(),
            client_id: client_id.map(str::to_owned),
        }
    }

    #[test]
    fn callbacks_bind_state_and_issued_registration() {
        let first = attempt(None);
        assert_eq!(
            first
                .callback("/auth/callback?state=state&code=code&client_id=oaiapp_new")
                .unwrap(),
            ("code".into(), "oaiapp_new".into())
        );
        for bad in [
            "/auth/callback?state=wrong&code=code&client_id=oaiapp_new",
            "/auth/callback?state=state&code=code",
            "/auth/callback?state=state&error=access_denied&code=code&client_id=oaiapp_new",
            "/auth/callback?state=state&state=state&code=code&client_id=oaiapp_new",
            "/callback?state=state&code=code&client_id=oaiapp_new",
        ] {
            assert!(first.callback(bad).is_err());
        }
        let returning = attempt(Some("saved"));
        assert!(
            returning
                .callback("/auth/callback?state=state&code=code&client_id=other")
                .is_err()
        );
        assert_eq!(
            returning
                .callback("/auth/callback?state=state&code=code")
                .unwrap()
                .1,
            "saved"
        );
    }

    #[test]
    fn authorization_uses_pkce_and_stable_host_and_never_registers_returning_client_again() {
        let first = attempt(None)
            .authorization_url("urn:uuid:host", None)
            .unwrap();
        let params = first
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["client_id"], "dynamic_agent_client");
        assert_eq!(params["agent_name_hint"], "Knut");
        assert_eq!(params["ext_agent_host_id"], "urn:uuid:host");
        let next = attempt(Some("saved"))
            .authorization_url("urn:uuid:host", None)
            .unwrap();
        assert!(!next.query_pairs().any(|(k, _)| k == "agent_name_hint"));
        assert!(host_id().unwrap().starts_with("urn:uuid:"));
        assert_ne!(random_value().unwrap(), random_value().unwrap());
    }

    #[test]
    fn token_rotation_is_atomic_and_cannot_silently_drop_consent() {
        let previous = Tokens {
            access_token: "old".into(),
            refresh_token: "old-refresh".into(),
            id_token: "old-id".into(),
            expires_at: 0,
            scopes: vec![PLAN_SCOPE.into()],
        };
        let response = |scope: Option<String>| TokenReply {
            access_token: "new".into(),
            refresh_token: Some("rotated".into()),
            id_token: None,
            token_type: "Bearer".into(),
            expires_in: 3600,
            scope,
        };
        let next = response(None).tokens(Some(&previous)).unwrap();
        assert_eq!(next.refresh_token, "rotated");
        assert_eq!(next.id_token, "old-id");
        assert!(
            response(Some("openid".into()))
                .tokens(Some(&previous))
                .is_err()
        );
    }

    #[tokio::test]
    async fn refresh_persists_rotated_tokens_and_reuses_them_after_restart() {
        let dir =
            std::env::temp_dir().join(format!("knut-auth-refresh-{}", random_value().unwrap()));
        let mut store = Store { dir: dir.clone() }.lock().unwrap();
        store.accounts.active = Some("selected-client".to_owned());
        store.accounts.registrations.push(Registration {
            client_id: "selected-client".into(),
            subject: "subject".into(),
            email: "fixture@example.invalid".into(),
            tokens: Some(Tokens {
                access_token: "expired".into(),
                refresh_token: "refresh-old".into(),
                id_token: "id-existing".into(),
                expires_at: 0,
                scopes: vec![PLAN_SCOPE.into()],
            }),
        });
        let mut other = store.accounts.registrations[0].clone();
        other.client_id = "other-client".to_owned();
        other.tokens.as_mut().unwrap().access_token = "other-account-token".to_owned();
        other.tokens.as_mut().unwrap().expires_at = now() + 3600;
        store.accounts.registrations.push(other);
        store.accounts.active = Some("other-client".to_owned());
        store.save().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    if body.len() >= length {
                        let parsed =
                            reqwest::Url::parse(&format!("http://localhost/?{body}")).unwrap();
                        let params = parsed
                            .query_pairs()
                            .collect::<std::collections::BTreeMap<_, _>>();
                        assert_eq!(params["client_id"], "selected-client");
                        assert_eq!(params["refresh_token"], "refresh-old");
                        assert_eq!(params["resource"], RESOURCE);
                        assert!(!params.contains_key("scope"));
                        break;
                    }
                }
            }
            let body = r#"{"access_token":"fresh","refresh_token":"rotated","token_type":"Bearer","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        assert_eq!(
            refresh_account(&mut store, &client().unwrap(), &endpoint, "selected-client")
                .await
                .unwrap(),
            "fresh"
        );
        server.await.unwrap();
        assert_eq!(store.accounts.active.as_deref(), Some("other-client"));
        drop(store);
        let mut restarted = Store { dir: dir.clone() }.lock().unwrap();
        assert_eq!(
            restarted
                .accounts
                .registrations
                .iter()
                .find(|r| r.client_id == "selected-client")
                .unwrap()
                .tokens
                .as_ref()
                .unwrap()
                .refresh_token,
            "rotated"
        );
        assert_eq!(
            refresh_account(
                &mut restarted,
                &client().unwrap(),
                "http://127.0.0.1:1/unreachable",
                "selected-client"
            )
            .await
            .unwrap(),
            "fresh"
        );
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn id_tokens_require_valid_signature_identity_expiry_and_nonce() {
        use jsonwebtoken::{EncodingKey, Header, encode};
        let key = EncodingKey::from_rsa_der(include_bytes!(
            "../tests/fixtures/openai-test-signing-key.der"
        ));
        let keys: JwkSet =
            serde_json::from_str(include_str!("../tests/fixtures/openai-test-jwks.json")).unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("fixture".to_owned());
        let claims = serde_json::json!({"sub":"subject", "email":"fixture@example.invalid", "iss":ISSUER, "aud":"client", "iat":now(), "exp":now()+300, "nonce":"nonce"});
        let sign = |claims: &serde_json::Value| encode(&header, claims, &key).unwrap();
        let valid = sign(&claims);
        assert_eq!(
            validate_identity(&valid, "client", Some("nonce"), &keys)
                .unwrap()
                .sub,
            "subject"
        );
        for (field, value) in [
            ("iss", serde_json::json!("https://attacker.invalid")),
            ("aud", serde_json::json!("other")),
            ("exp", serde_json::json!(now() - 60)),
            ("nonce", serde_json::json!("other")),
            ("sub", serde_json::json!("")),
        ] {
            let mut bad = claims.clone();
            bad[field] = value;
            assert!(
                validate_identity(&sign(&bad), "client", Some("nonce"), &keys).is_err(),
                "{field}"
            );
        }
        let mut corrupted = valid.into_bytes();
        let last = corrupted.len() - 8;
        corrupted[last] = if corrupted[last] == b'A' { b'B' } else { b'A' };
        assert!(
            validate_identity(
                std::str::from_utf8(&corrupted).unwrap(),
                "client",
                Some("nonce"),
                &keys
            )
            .is_err()
        );
        let mut missing_iat = claims;
        missing_iat.as_object_mut().unwrap().remove("iat");
        assert!(validate_identity(&sign(&missing_iat), "client", Some("nonce"), &keys).is_err());
    }

    #[test]
    fn credential_storage_preserves_host_and_rejects_insecure_files_and_symlinks() {
        let dir = std::env::temp_dir().join(format!("knut-auth-test-{}", random_value().unwrap()));
        let store = Store { dir: dir.clone() };
        let mut locked = store.lock().unwrap();
        locked.accounts.host_id = host_id().unwrap();
        locked.accounts.preferred_model = Some("chosen-model".to_owned());
        locked.accounts.plan_notice_seen = true;
        locked.save().unwrap();
        let host = locked.accounts.host_id.clone();
        drop(locked);
        let store = Store { dir: dir.clone() };
        let restarted = store.read().unwrap();
        assert_eq!(restarted.host_id, host);
        assert_eq!(restarted.preferred_model.as_deref(), Some("chosen-model"));
        assert!(restarted.plan_notice_seen);
        let path = dir.join("openai-accounts.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.read().is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
        assert!(store.read().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn cancelling_browser_sign_in_closes_the_callback_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { wait_callback(listener, &attempt(None)).await });
        tokio::task::yield_now().await;
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        assert!(tokio::net::TcpListener::bind(address).await.is_ok());
    }
}

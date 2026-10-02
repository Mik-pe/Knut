use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{AuthError, auth_error, random_value};

const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Registration {
    pub(crate) client_id: String,
    pub(crate) subject: String,
    pub(crate) email: String,
    #[serde(default)]
    pub(crate) tokens: Option<Tokens>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Tokens {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) id_token: String,
    pub(crate) expires_at: u64,
    pub(crate) scopes: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Accounts {
    pub(crate) host_id: String,
    pub(crate) active: Option<String>,
    pub(crate) registrations: Vec<Registration>,
    #[serde(default)]
    pub(crate) preferred_model: Option<String>,
    #[serde(default)]
    pub(crate) api_models: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) plan_notice_seen: bool,
}

impl Accounts {
    pub(crate) fn selected(&self) -> Result<&Registration, AuthError> {
        self.registrations
            .iter()
            .find(|r| Some(&r.client_id) == self.active.as_ref())
            .ok_or_else(|| auth_error("Run `knut login openai-codex` to sign in with ChatGPT"))
    }
}

pub(crate) struct Store {
    pub(crate) dir: PathBuf,
}
pub(crate) struct LockedStore {
    pub(crate) store: Store,
    _lock: File,
    pub(crate) accounts: Accounts,
}

impl Registration {
    pub(crate) fn label(&self) -> &str {
        if self.email.is_empty() {
            "ChatGPT account"
        } else {
            &self.email
        }
    }
}

fn private_metadata(metadata: &std::fs::Metadata) -> bool {
    // SAFETY: geteuid has no preconditions.
    metadata.uid() == unsafe { libc::geteuid() } && metadata.permissions().mode() & 0o077 == 0
}

impl Store {
    pub(crate) fn configured() -> Result<Self, AuthError> {
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

    fn prepare(&self) -> Result<(), AuthError> {
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

    pub(crate) fn read(&self) -> Result<Accounts, AuthError> {
        self.prepare()?;
        // A FIFO can block during open, before its metadata can be rejected.
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(self.dir.join("openai-accounts.json"))
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Accounts::default()),
            Err(_) => return Err(auth_error("Cannot read ChatGPT credentials")),
        };
        let metadata = file
            .metadata()
            .map_err(|_| auth_error("Cannot inspect ChatGPT credentials"))?;
        if !metadata.is_file() || !private_metadata(&metadata) {
            return Err(auth_error(
                "ChatGPT credentials require an owner-only regular file",
            ));
        }
        if metadata.len() > MAX_CREDENTIAL_BYTES as u64 {
            return Err(auth_error("ChatGPT credential record exceeds 1 MiB"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_CREDENTIAL_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| auth_error("Cannot read ChatGPT credentials"))?;
        if bytes.len() > MAX_CREDENTIAL_BYTES {
            return Err(auth_error("ChatGPT credential record exceeds 1 MiB"));
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            auth_error("Invalid ChatGPT credential record; restore it or sign in again")
        })
    }

    pub(crate) fn lock(self) -> Result<LockedStore, AuthError> {
        self.prepare()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(self.dir.join("openai-auth.lock"))
            .map_err(|_| auth_error("Cannot lock ChatGPT credentials"))?;
        let metadata = lock
            .metadata()
            .map_err(|_| auth_error("Cannot inspect credential lock"))?;
        if !metadata.is_file() || !private_metadata(&metadata) {
            return Err(auth_error(
                "ChatGPT credential lock requires an owner-only regular file",
            ));
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
    pub(crate) fn save(&self) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(&self.accounts)
            .map_err(|_| auth_error("Cannot encode ChatGPT credentials"))?;
        if bytes.len() > MAX_CREDENTIAL_BYTES {
            return Err(auth_error("ChatGPT credential record exceeds 1 MiB"));
        }
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
            file.write_all(&bytes)
                .map_err(|_| auth_error("Cannot save ChatGPT credentials"))?;
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

pub(crate) async fn lock_store() -> Result<LockedStore, AuthError> {
    let store = Store::configured()?;
    tokio::task::spawn_blocking(move || store.lock())
        .await
        .map_err(|_| auth_error("Credential lock task failed"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_id;

    #[test]
    fn fifo_credentials_and_locks_are_rejected_without_waiting_for_a_writer() {
        let dir = std::env::temp_dir().join(format!("knut-auth-fifo-{}", random_value().unwrap()));
        Store { dir: dir.clone() }.prepare().unwrap();
        for (filename, credential_lock) in
            [("openai-accounts.json", false), ("openai-auth.lock", true)]
        {
            let path = dir.join(filename);
            let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: fifo is a valid NUL-terminated path and remains alive for the call.
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            let (sender, receiver) = std::sync::mpsc::channel();
            let store = Store { dir: dir.clone() };
            let reader = std::thread::spawn(move || {
                let rejected = if credential_lock {
                    store.lock().is_err()
                } else {
                    store.read().is_err()
                };
                sender.send(rejected).unwrap();
            });
            assert!(
                receiver
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("opening a FIFO must not wait for a writer")
            );
            reader.join().unwrap();
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_saves_preserve_the_readable_credential_record() {
        let dir = std::env::temp_dir().join(format!("knut-auth-size-{}", random_value().unwrap()));
        let mut locked = Store { dir: dir.clone() }.lock().unwrap();
        locked.accounts.host_id = host_id().unwrap();
        locked.accounts.preferred_model = Some("original-model".to_owned());
        locked.save().unwrap();
        let path = dir.join("openai-accounts.json");
        let original = std::fs::read(&path).unwrap();

        locked.accounts.preferred_model = Some("x".repeat(MAX_CREDENTIAL_BYTES));
        assert!(locked.save().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            locked.store.read().unwrap().preferred_model.as_deref(),
            Some("original-model")
        );
        drop(locked);

        std::fs::write(&path, vec![b' '; MAX_CREDENTIAL_BYTES + 1]).unwrap();
        assert!(Store { dir: dir.clone() }.read().is_err());
        std::fs::remove_dir_all(dir).unwrap();
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
}

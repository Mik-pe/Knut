use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{OptionalExtension, params};
use tokio::sync::{mpsc, watch};

use super::SessionStore;
use crate::KnutError;
use crate::composer::{Composer, ComposerMemory};

const SAVE_INTERVAL: Duration = Duration::from_millis(400);
const MAX_MEMORY_BYTES: usize = 8 * 1024 * 1024;

struct EditorWriter {
    store: SessionStore,
    workspace: Vec<u8>,
    revision: i64,
}

impl EditorWriter {
    fn open(path: &Path, workspace: &Path) -> Result<(Self, ComposerMemory), KnutError> {
        let workspace = workspace
            .canonicalize()
            .map_err(|err| KnutError::Tool(format!("identifying draft workspace: {err}")))?
            .as_os_str()
            .as_encoded_bytes()
            .to_vec();
        let store = SessionStore::open(path)?;
        store
            .connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(|err| KnutError::Tool(format!("configuring draft storage: {err}")))?;
        let stored: Option<(i64, Option<String>)> = store
            .connection
            .query_row(
                "SELECT revision, CASE WHEN length(CAST(payload AS BLOB)) <= ?2 THEN payload END
             FROM editor_memory WHERE workspace = ?1",
                params![workspace, MAX_MEMORY_BYTES as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|err| KnutError::Tool(format!("loading saved input: {err}")))?;
        let (revision, memory) = match stored {
            None => (0, ComposerMemory::default()),
            Some((revision, Some(payload))) => {
                let memory: ComposerMemory = serde_json::from_str(&payload)
                    .map_err(|err| KnutError::Tool(format!("saved input is unreadable: {err}")))?;
                Composer::from_memory(memory.clone())
                    .map_err(|err| KnutError::Tool(err.to_owned()))?;
                (revision, memory)
            }
            Some((_, None)) => return Err(KnutError::Tool("saved input is too large".to_owned())),
        };
        Ok((
            Self {
                store,
                workspace,
                revision,
            },
            memory,
        ))
    }

    fn save(&mut self, memory: &ComposerMemory) -> Result<(), KnutError> {
        let payload = serde_json::to_string(memory)
            .map_err(|err| KnutError::Tool(format!("encoding draft: {err}")))?;
        let changed = self.store.connection.execute(
            "INSERT INTO editor_memory (workspace, revision, payload) VALUES (?1, 1, ?2)
             ON CONFLICT(workspace) DO UPDATE SET revision = revision + 1, payload = excluded.payload
             WHERE revision = ?3",
            params![self.workspace, payload, self.revision],
        ).map_err(|err| KnutError::Tool(format!("saving input: {err}")))?;
        if changed == 0 {
            return Err(KnutError::Tool(
                "another terminal saved this workspace's input; this terminal's draft is unsaved"
                    .to_owned(),
            ));
        }
        self.revision += 1;
        Ok(())
    }
}

pub(crate) struct EditorMemory {
    sender: watch::Sender<ComposerMemory>,
    worker: tokio::task::JoinHandle<()>,
    errors: mpsc::UnboundedReceiver<String>,
    last: ComposerMemory,
    next_save: Instant,
}

impl EditorMemory {
    pub(crate) fn open(path: &Path, workspace: &Path) -> Result<(Self, Composer), KnutError> {
        let (mut writer, last) = EditorWriter::open(path, workspace)?;
        let composer =
            Composer::from_memory(last.clone()).map_err(|err| KnutError::Tool(err.to_owned()))?;
        let (sender, mut receiver) = watch::channel(last.clone());
        let (error_tx, errors) = mpsc::unbounded_channel();
        let worker = tokio::spawn(async move {
            while receiver.changed().await.is_ok() {
                let memory = receiver.borrow_and_update().clone();
                match tokio::task::spawn_blocking(move || {
                    let result = writer.save(&memory);
                    (writer, result)
                })
                .await
                {
                    Ok((next, Ok(()))) => writer = next,
                    Ok((_, Err(err))) => {
                        let _ = error_tx.send(err.to_string());
                        break;
                    }
                    Err(err) => {
                        let _ = error_tx.send(format!("draft writer stopped: {err}"));
                        break;
                    }
                }
            }
        });
        Ok((
            Self {
                sender,
                worker,
                errors,
                last,
                next_save: Instant::now(),
            },
            composer,
        ))
    }

    pub(crate) fn checkpoint(&mut self, composer: &Composer, force: bool) {
        if self.worker.is_finished() || (!force && Instant::now() < self.next_save) {
            return;
        }
        self.next_save = Instant::now() + SAVE_INTERVAL;
        let memory = composer.memory();
        if memory != self.last {
            self.sender.send_replace(memory.clone());
            self.last = memory;
        }
    }

    pub(crate) fn error(&mut self) -> Option<String> {
        self.errors.try_recv().ok()
    }

    pub(crate) async fn finish(mut self) -> Option<String> {
        drop(self.sender);
        let result = self.worker.await;
        result
            .err()
            .map(|err| err.to_string())
            .or_else(|| self.errors.try_recv().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "knut-editor-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(root.join("workspace")).unwrap();
            Self(root)
        }

        fn path(&self) -> PathBuf {
            self.0.join("sessions.db")
        }
        fn workspace(&self) -> PathBuf {
            self.0.join("workspace")
        }
        fn open(&self) -> (EditorWriter, ComposerMemory) {
            EditorWriter::open(&self.path(), &self.workspace()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn draft(text: &str) -> Composer {
        let mut composer = Composer::new();
        composer.paste(text);
        composer
    }

    #[test]
    fn reopen_and_clear_are_durable_and_workspaces_are_isolated() {
        let fixture = Fixture::new();
        let composer = draft("unfinished\n👩‍💻");
        let (mut writer, _) = fixture.open();
        writer.save(&composer.memory()).unwrap();
        drop(writer);
        let (mut writer, restored) = fixture.open();
        assert_eq!(restored, composer.memory());
        let other = fixture.0.join("other");
        std::fs::create_dir(&other).unwrap();
        assert_eq!(
            EditorWriter::open(&fixture.path(), &other).unwrap().1,
            ComposerMemory::default()
        );
        writer.save(&ComposerMemory::default()).unwrap();
        assert_eq!(fixture.open().1, ComposerMemory::default());
    }

    #[cfg(unix)]
    #[test]
    fn a_workspace_alias_recovers_the_same_draft() {
        let fixture = Fixture::new();
        let alias = fixture.0.join("alias");
        std::os::unix::fs::symlink(fixture.workspace(), &alias).unwrap();
        let memory = draft("through a symlink").memory();
        fixture.open().0.save(&memory).unwrap();
        assert_eq!(
            EditorWriter::open(&fixture.path(), &alias).unwrap().1,
            memory
        );
    }

    #[test]
    fn stale_terminal_cannot_overwrite_a_newer_draft() {
        let fixture = Fixture::new();
        let (mut first, _) = fixture.open();
        let (mut second, _) = fixture.open();
        first.save(&draft("keep this").memory()).unwrap();
        let error = second.save(&draft("stale").memory()).unwrap_err();
        assert!(error.to_string().contains("another terminal"));
        assert_eq!(fixture.open().1, draft("keep this").memory());
    }

    #[test]
    fn unreadable_memory_is_reported_and_not_reset() {
        let fixture = Fixture::new();
        let (writer, _) = fixture.open();
        writer
            .store
            .connection
            .execute(
                "INSERT INTO editor_memory (workspace, revision, payload) VALUES (?1, 1, 'broken')",
                params![writer.workspace],
            )
            .unwrap();
        assert!(EditorWriter::open(&fixture.path(), &fixture.workspace()).is_err());
        let payload: String = writer
            .store
            .connection
            .query_row("SELECT payload FROM editor_memory", [], |row| row.get(0))
            .unwrap();
        assert_eq!(payload, "broken");
    }

    #[test]
    fn migration_preserves_existing_sessions() {
        let fixture = Fixture::new();
        let mut store = SessionStore::open(fixture.path()).unwrap();
        store
            .start_session("existing", "/ws", "rev", "policy", "completed", 1)
            .unwrap();
        store
            .connection
            .execute_batch("DROP TABLE editor_memory; PRAGMA user_version = 1;")
            .unwrap();
        drop(store);
        let (writer, _) = fixture.open();
        assert_eq!(writer.store.schema_version().unwrap(), 2);
        assert_eq!(writer.store.sessions().unwrap()[0].id, "existing");
    }

    #[tokio::test]
    async fn shutdown_flushes_the_latest_coalesced_edit() {
        let fixture = Fixture::new();
        let (mut memory, _) = EditorMemory::open(&fixture.path(), &fixture.workspace()).unwrap();
        for index in 0..20 {
            memory.checkpoint(&draft(&format!("draft {index}")), true);
        }
        assert_eq!(memory.finish().await, None);
        assert_eq!(fixture.open().1, draft("draft 19").memory());
    }
}

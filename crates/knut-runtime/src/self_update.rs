use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    CheckProfile, CheckRunner, CommandRequest, KnutError, SideEffect, Supervisor, Tool,
    ToolMetadata, ToolRegistry, Workspace,
};

const MAX_BINARY: u64 = 256 * 1024 * 1024;
struct Installation {
    workspace: Workspace,
    target: PathBuf,
}
struct Inspect(Arc<Installation>);
struct Install(Arc<Installation>);

fn failure(message: &str) -> KnutError {
    KnutError::Tool(message.into())
}
fn hash(path: &Path) -> Result<String, KnutError> {
    let mut file = File::open(path).map_err(|_| failure("Could not read installation binary"))?;
    if !file
        .metadata()
        .map_err(|_| failure("Could not inspect binary"))?
        .is_file()
    {
        return Err(failure("Binary must be a regular file"));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| failure("Could not hash binary"))?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > MAX_BINARY {
            return Err(failure("Installation binary exceeds 256 MiB"));
        }
        digest.update(&buffer[..count]);
    }
    if total == 0 {
        return Err(failure("Installation binary is empty"));
    }
    Ok(format!("{:x}", digest.finalize()))
}
impl Installation {
    fn revision(&self) -> Result<String, KnutError> {
        let runner = CheckRunner::new(
            self.workspace.clone(),
            Arc::new(Supervisor::new(self.workspace.clone())),
            CheckProfile::for_workspace(&self.workspace)?,
        );
        Ok(runner.current_revision("self-update")?.revision)
    }
    fn validate(&self, input: &Value) -> Result<(), KnutError> {
        if hash(&self.target)? != input["expect_installed_hash"] {
            return Err(failure(
                "The installed binary changed; inspect and approve again",
            ));
        }
        if self.revision()? != input["expect_source_revision"] {
            return Err(failure("The source changed; inspect and approve again"));
        }
        Ok(())
    }
    async fn build(&self) -> Result<PathBuf, KnutError> {
        // Every command remains inside the ordinary sandbox. No credential
        // inheritance, shell interpolation, network or unsandboxed fallback.
        let supervisor = Supervisor::new(self.workspace.clone());
        let commands: &[&[&str]] = &[
            &["fmt", "--all", "--check"],
            &["test", "--workspace", "--locked", "--offline"],
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--locked",
                "--offline",
                "--",
                "-D",
                "warnings",
            ],
            &["build", "--release", "--locked", "--offline", "-p", "knut"],
        ];
        let output = self
            .workspace
            .resolve(".knut-update")?
            .absolute()
            .to_owned();
        std::fs::create_dir_all(&output)
            .map_err(|_| failure("Could not create update build directory"))?;
        for args in commands {
            let mut request =
                CommandRequest::new("cargo", args.iter().map(|arg| (*arg).into()).collect())
                    .with_writable(vec![".".into()])
                    .with_timeout(Duration::from_secs(600));
            request.env.insert(
                "CARGO_TARGET_DIR".into(),
                output.to_string_lossy().into_owned(),
            );
            let outcome = supervisor.run(request).await?;
            if !outcome.status.is_success() {
                return Err(KnutError::Tool(format!(
                    "Update check failed; installed binary unchanged: {}",
                    outcome.model_summary
                )));
            }
        }
        Ok(output.join("release/knut"))
    }
}
#[async_trait]
impl Tool for Inspect {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata { id:"inspect_installation".into(),tool_version:"1".into(),capability:"self".into(),description:"Inspect the installed Knut hash and current workspace source revision. Pass both identities to install_update after implementing changes. Installation runs sandboxed formatting, tests, clippy and a release build before activation.".into(), input_schema:json!({"type":"object","properties":{},"additionalProperties":false}),side_effect:SideEffect::ReadOnly }
    }
    async fn call(&self, _input: Value) -> Result<Value, KnutError> {
        Ok(json!({"installed_hash":hash(&self.0.target)?, "source_revision":self.0.revision()?}))
    }
}
#[async_trait]
impl Tool for Install {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata { id:"install_update".into(),tool_version:"1".into(),capability:"self".into(),description:"Check, build and atomically install the approved source revision at the host-configured Knut installation. Requires exact approval and fresh installed hash/source revision from inspect_installation. Uses offline sandboxed cargo fmt, test, clippy and release build; refuses failures or changed inputs. Running sessions keep their executable. Retains the previous binary for rollback.".into(),input_schema:json!({"type":"object","properties":{"expect_installed_hash":{"type":"string"},"expect_source_revision":{"type":"string"}},"required":["expect_installed_hash","expect_source_revision"],"additionalProperties":false}),side_effect:SideEffect::NonIdempotentWrite }
    }
    async fn call(&self, input: Value) -> Result<Value, KnutError> {
        self.0.validate(&input)?;
        let candidate = self.0.build().await?;
        self.0.validate(&input)?;
        activate(&self.0, &candidate, &input)
    }
}

fn activate(
    installation: &Installation,
    candidate: &Path,
    input: &Value,
) -> Result<Value, KnutError> {
    let target = &installation.target;
    let parent = target
        .parent()
        .ok_or_else(|| failure("Invalid installation directory"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(parent.join(".knut-install.lock"))
        .map_err(|_| failure("Could not lock installation"))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(failure("Another installation is in progress"));
    }
    installation.validate(input)?;
    let candidate_hash = hash(candidate)?;
    let mut source = File::open(candidate).map_err(|_| failure("Could not open candidate"))?;
    let mut magic = [0; 4];
    source
        .read_exact(&mut magic)
        .map_err(|_| failure("Candidate is not an executable"))?;
    if magic != [0x7f, b'E', b'L', b'F']
        && magic != [0xcf, 0xfa, 0xed, 0xfe]
        && magic != [0xfe, 0xed, 0xfa, 0xcf]
    {
        return Err(failure(
            "Candidate must be a native ELF or Mach-O executable",
        ));
    }
    let temp = parent.join(format!(".knut-next-{}", std::process::id()));
    let previous = target.with_extension("previous");
    let result = (|| {
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&temp)
            .map_err(|_| failure("Could not stage update"))?;
        destination
            .write_all(&magic)
            .map_err(|_| failure("Could not stage executable"))?;
        std::io::copy(&mut source.take(MAX_BINARY), &mut destination)
            .map_err(|_| failure("Could not stage update"))?;
        destination
            .sync_all()
            .map_err(|_| failure("Could not sync update"))?;
        if hash(&temp)? != candidate_hash {
            return Err(failure("Candidate changed during installation"));
        }
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))
            .map_err(|_| failure("Could not set executable permissions"))?;
        let backup = parent.join(format!(".knut-previous-{}", std::process::id()));
        std::fs::hard_link(target, &backup)
            .map_err(|_| failure("Could not retain previous executable"))?;
        if std::fs::rename(&backup, &previous).is_err() {
            let _ = std::fs::remove_file(&backup);
            return Err(failure("Could not retain rollback executable"));
        }
        std::fs::rename(&temp, target).map_err(|_| failure("Could not activate update"))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| failure("Could not sync installation directory"))?;
        Ok(
            json!({"installed_hash":candidate_hash,"active_sessions":"continue on their original executable","new_sessions":"use the installed update"}),
        )
    })();
    let _ = std::fs::remove_file(&temp);
    result
}

pub fn register_self_update_tools(
    registry: &mut ToolRegistry,
    workspace: Workspace,
) -> Result<(), KnutError> {
    let Some(target) = std::env::var_os("KNUT_UPDATE_TARGET") else {
        return Ok(());
    };
    let target = PathBuf::from(target);
    if !target.is_absolute()
        || !std::fs::symlink_metadata(&target)
            .map_err(|_| failure("Self-update target must exist"))?
            .file_type()
            .is_file()
    {
        return Err(failure(
            "Self-update requires a fixed absolute regular installation path",
        ));
    }
    let installation = Arc::new(Installation { workspace, target });
    registry.register(Inspect(installation.clone()))?;
    registry.register(Install(installation))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        root: PathBuf,
        installation: Installation,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "knut-update-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let source = root.join("source");
            std::fs::create_dir_all(&source).unwrap();
            std::fs::write(
                source.join("Cargo.toml"),
                "[package]\nname=\"knut\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
            )
            .unwrap();
            std::fs::write(source.join(".gitignore"), ".knut-update/\n").unwrap();
            let target = root.join("knut");
            std::fs::write(&target, b"\x7fELFold version").unwrap();
            Self {
                installation: Installation {
                    workspace: Workspace::open(source).unwrap(),
                    target,
                },
                root,
            }
        }
        fn input(&self) -> Value {
            json!({"expect_installed_hash":hash(&self.installation.target).unwrap(),"expect_source_revision":self.installation.revision().unwrap()})
        }
        fn candidate(&self) -> PathBuf {
            let candidate = self.root.join("candidate");
            std::fs::write(&candidate, b"\x7fELFnew version").unwrap();
            candidate
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn atomic_activation_retains_old_inode_and_rollback() {
        let f = Fixture::new();
        let mut resident = File::open(&f.installation.target).unwrap();
        let candidate = f.candidate();
        activate(&f.installation, &candidate, &f.input()).unwrap();
        let mut original = Vec::new();
        resident.read_to_end(&mut original).unwrap();
        assert_eq!(original, b"\x7fELFold version");
        assert_eq!(
            std::fs::read(f.installation.target.with_extension("previous")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read(&f.installation.target).unwrap(),
            std::fs::read(candidate).unwrap()
        );
    }
    #[test]
    fn changed_source_or_installation_requires_fresh_approval() {
        let f = Fixture::new();
        let input = f.input();
        let candidate = f.candidate();
        std::fs::write(
            f.installation.workspace.root().join("changed.rs"),
            "changed",
        )
        .unwrap();
        assert!(activate(&f.installation, &candidate, &input).is_err());
        let input = f.input();
        std::fs::write(&f.installation.target, b"another installed version").unwrap();
        assert!(activate(&f.installation, &candidate, &input).is_err());
        assert_eq!(
            std::fs::read(&f.installation.target).unwrap(),
            b"another installed version"
        );
        assert!(!f.installation.target.with_extension("previous").exists());
    }
    #[tokio::test]
    async fn failed_build_keeps_installation_unchanged() {
        let f = Fixture::new();
        let before = std::fs::read(&f.installation.target).unwrap();
        // An invalid crate has no target: formatting/checks must refuse it.
        let input = f.input();
        let install = Install(Arc::new(Installation {
            workspace: f.installation.workspace.clone(),
            target: f.installation.target.clone(),
        }));
        assert!(install.call(input).await.is_err());
        assert_eq!(std::fs::read(&f.installation.target).unwrap(), before);
        assert!(!f.installation.target.with_extension("previous").exists());
    }
}

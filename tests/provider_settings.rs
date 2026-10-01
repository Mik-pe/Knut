use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new(model: Option<&str>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "knut-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let record = serde_json::json!({
            "host_id": "urn:uuid:fixture",
            "active": "fixture-client",
            "preferred_model": model,
            "registrations": [{
                "client_id": "fixture-client",
                "subject": "fixture-subject",
                "email": "fixture@example.invalid",
                "tokens": {
                    "access_token": "fixture-access-never-sent",
                    "refresh_token": "fixture-refresh-never-sent",
                    "id_token": "fixture-id-never-sent",
                    "expires_at": 4_000_000_000_u64,
                    "scopes": ["chatgpt.tokens.use.direct"]
                }
            }]
        });
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("openai-accounts.json"))
            .unwrap();
        serde_json::to_writer(file, &record).unwrap();
        Self(dir)
    }

    fn doctor(&self) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_knut"))
            .arg("doctor")
            .env("KNUT_CONFIG_DIR", &self.0)
            .env("KNUT_PROVIDER", "zai")
            .env("KNUT_PROVIDER_MODEL", "environment-model")
            .env("KNUT_PROVIDER_BASE_URL", "http://127.0.0.1:1/v1")
            .env("KNUT_PROVIDER_API_KEY", "fixture-api-never-sent")
            .env_remove("TYPESAFE_API_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn saved_chatgpt_settings_survive_restart_and_override_ambient_provider_variables() {
    let fixture = Fixture::new(Some("saved-model"));
    for _ in 0..2 {
        let output = fixture.doctor();
        assert!(output.contains("model saved-model"), "{output}");
        assert!(output.contains("https://api.openai.com/v1"));
        assert!(output.contains("ChatGPT sign-in"));
        assert!(!output.contains("fixture-access-never-sent"));
        assert!(!output.contains("environment-model"));
    }
}

#[test]
fn without_a_saved_chatgpt_model_environment_configuration_remains_available() {
    let fixture = Fixture::new(None);
    let output = fixture.doctor();
    assert!(output.contains("model environment-model"), "{output}");
    assert!(output.contains("http://127.0.0.1:1/v1"));
    assert!(!output.contains("fixture-api-never-sent"));
}

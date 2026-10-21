use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use knut::{CheckOutcome, EvidenceReport};

struct Fixture(PathBuf);

impl Fixture {
    fn new(source: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "knut-verification-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"verify_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), source).unwrap();
        fs::write(root.join("marker.txt"), "original").unwrap();
        let lockfile = Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            lockfile.status.success(),
            "{}",
            String::from_utf8_lossy(&lockfile.stderr)
        );
        Self(root)
    }

    fn verify(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_knut"))
            .args(["verify", "--json"])
            .current_dir(&self.0)
            .env("KNUT_LOAD_ENV", "0")
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn passing_verification_outputs_one_json_report() {
    let fixture = Fixture::new("#[test] fn passes() {}\n");
    let output = fixture.verify();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: EvidenceReport = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report.is_green());
    assert_eq!(report.checks.len(), 3);
    assert!(
        report
            .checks
            .iter()
            .all(|check| check.outcome == CheckOutcome::Passed)
    );
}

#[test]
fn source_changes_during_verification_are_reported_as_stale() {
    let fixture = Fixture::new(
        "#[test] fn changes_source() { std::fs::write(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/marker.txt\"), \"changed\").unwrap(); }\n",
    );
    let output = fixture.verify();
    assert!(
        !output.status.success(),
        "verification accepted changed source"
    );
    let report: EvidenceReport = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!report.is_green());
    assert!(!report.outstanding.is_empty());
    assert!(
        report
            .checks
            .iter()
            .all(|check| check.outcome == CheckOutcome::Stale)
    );
    assert_eq!(
        fs::read_to_string(fixture.0.join("marker.txt")).unwrap(),
        "changed"
    );
}

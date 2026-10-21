use std::process::Command;

#[test]
fn help_does_not_initialize_a_live_playground_backend() {
    let output = Command::new(env!("CARGO_BIN_EXE_knut"))
        .args(["--help", "--backend", "jev"])
        .env("KNUT_LOAD_ENV", "0")
        .env_remove("TYPESAFE_API_KEY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("USAGE:"));
}

#[test]
fn offline_route_still_emits_a_parseable_trace_after_cli_refactoring() {
    let output = Command::new(env!("CARGO_BIN_EXE_knut"))
        .args(["route", "Explain this error", "--json"])
        .env("KNUT_LOAD_ENV", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trace: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(trace["prompt"], "Explain this error");
    assert!(trace["action"].is_object());
}

#[test]
fn offline_bench_does_not_invent_model_or_routing_measurements() {
    struct ReportDirectory(std::path::PathBuf);
    impl Drop for ReportDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let directory = ReportDirectory(std::env::temp_dir().join(format!(
        "knut-cli-bench-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    )));
    let output = Command::new(env!("CARGO_BIN_EXE_knut"))
        .arg("bench")
        .env("KNUT_LOAD_ENV", "0")
        .env("KNUT_BENCH_OUT", &directory.0)
        .env("KNUT_PROVIDER_MODEL", "unused-model")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: knut::BenchReport = serde_json::from_slice(
        &std::fs::read(
            directory
                .0
                .join(format!("bench-report-v{}.json", knut::REPORT_VERSION)),
        )
        .unwrap(),
    )
    .unwrap();
    let tasks = knut::pilot_suite();
    assert_eq!(report.runs.len(), tasks.len());
    assert_eq!(
        report.held_out_tasks,
        tasks.iter().filter(|task| task.held_out).count()
    );
    assert!(report.comparisons.is_empty());
    assert_eq!(report.versions.model, "none (offline checks only)");
    assert!(
        report
            .runs
            .iter()
            .all(|run| run.control_decisions == 0 && run.cost.is_none())
    );
}

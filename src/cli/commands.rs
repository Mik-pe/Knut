use knut::KnutError;
use std::sync::Arc;

pub(super) async fn lsp_status() -> Result<(), KnutError> {
    println!("language intelligence (LSP {})", knut::LSP_PROTOCOL_VERSION);
    println!(
        "  supported features: {}",
        knut::SUPPORTED_FEATURES
            .iter()
            .map(|feature| feature.label())
            .collect::<Vec<_>>()
            .join(", ")
    );

    for config in [knut::ServerConfig::rust(), knut::ServerConfig::typescript()] {
        let availability = {
            let manager = knut::LanguageServerManager::new(config.clone());
            manager.availability()
        };
        match availability {
            Ok(()) => println!("  {}: ready ({})", config.language, config.program),
            Err(unavailable) => {
                println!("  {}: unavailable — {unavailable}", config.language);
            }
        }
    }

    println!(
        "  note: servers are not started implicitly; trust one with ServerConfig::trust(true) \
         and a verified executable"
    );
    Ok(())
}

pub(super) async fn release_artifacts() -> Result<(), KnutError> {
    let version = env!("CARGO_PKG_VERSION");
    println!("knut {version} — release artifacts");
    println!();
    println!("build (reproducible, no network after `cargo fetch`):");
    println!("  cargo build --release --locked");
    println!("  cargo test --all --locked          # offline fixtures");
    println!();
    println!("artifacts:");
    println!("  target/release/knut                # the binary");
    println!("  knut-{version}-x86_64-unknown-linux-gnu.tar.gz");
    println!("  knut-{version}-x86_64-unknown-linux-gnu.tar.gz.sha256");
    println!();
    println!("package:");
    println!("  tar -czf knut-{version}-$(uname -m)-unknown-linux-gnu.tar.gz \\");
    println!("      -C target/release knut");
    println!("  sha256sum knut-{version}-*.tar.gz > knut-{version}-*.tar.gz.sha256");
    println!();
    println!("platform support:");
    println!("  linux      fully verified: TUI, commands, sandbox (bubblewrap)");
    println!("  macos      TUI expected; command/sandbox support NOT verified");
    println!("  windows    not verified; do not treat isolation as working");
    println!();
    println!("paid or network-dependent checks stay opt-in:");
    println!(
        "  KNUT_LIVE_SMOKE=1 cargo test -p knut-runtime --lib provider::tests::live_glm_smoke_is_opt_in"
    );
    Ok(())
}

pub(super) async fn sessions(args: &[String], as_json: bool) -> Result<(), KnutError> {
    let store_path = knut::session_store_path();
    let store = knut::SessionStore::open(&store_path)?;
    let subcommand = args.first().map(String::as_str).unwrap_or("list");

    match subcommand {
        "list" | "" => {
            let sessions = store.sessions()?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&sessions)
                        .map_err(|err| KnutError::Tool(format!("serialize: {err}")))?
                );
            } else if sessions.is_empty() {
                println!("no stored sessions ({})", store_path.display());
            } else {
                println!("stored sessions ({})", store_path.display());
                for session in sessions {
                    println!(
                        "  {}  {}  {} events  {}",
                        session.id, session.task_state, session.event_count, session.workspace
                    );
                }
            }
            Ok(())
        }
        "show" | "replay" => {
            let Some(id) = args.get(1) else {
                return Err(KnutError::Tool(
                    "sessions show needs a session id".to_owned(),
                ));
            };
            // Replaying rebuilds state only: it never dispatches.
            let state = knut::replay_state(&store, id, "stored")?;
            println!("session {id} replayed (state only; nothing was executed)");
            println!("  task state: {:?}", state.task_state);
            println!("  timeline entries: {}", state.timeline.len());
            for entry in state.timeline.iter().rev().take(10).rev() {
                println!("  [{}] {}", entry.kind.label(), entry.text);
            }
            Ok(())
        }
        "export" => {
            let Some(id) = args.get(1) else {
                return Err(KnutError::Tool(
                    "sessions export needs a session id".to_owned(),
                ));
            };
            let include_raw = args.iter().any(|arg| arg == "--raw");
            let export = store.export(id, include_raw)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&export)
                    .map_err(|err| KnutError::Tool(format!("serialize: {err}")))?
            );
            if !include_raw {
                eprintln!("note: paths are redacted; pass --raw to include them explicitly");
            }
            Ok(())
        }
        "plan" | "resume" => {
            let Some(id) = args.get(1) else {
                return Err(KnutError::Tool(
                    "sessions plan needs a session id".to_owned(),
                ));
            };
            let workspace = knut::Workspace::open(".")?;
            let supervisor = std::sync::Arc::new(knut::Supervisor::new(workspace.clone()));
            let revision =
                knut::CheckRunner::new(workspace.clone(), supervisor, knut::CheckProfile::rust())
                    .current_revision("workspace")
                    .map(|revision| revision.revision)
                    .unwrap_or_else(|_| "unknown".to_owned());

            let plan = store.plan_resume(id, &revision, "side-effect-default")?;
            match plan {
                knut::ResumePlan::Continue => {
                    println!("session {id} can continue");
                }
                knut::ResumePlan::Reconcile(blockers) => {
                    println!("session {id} needs reconciliation before continuing:");
                    for blocker in blockers {
                        println!("  - {blocker}");
                    }
                }
                knut::ResumePlan::Refuse(blockers) => {
                    println!("session {id} cannot be resumed as-is:");
                    for blocker in blockers {
                        println!("  - {blocker}");
                    }
                }
            }
            Ok(())
        }
        other => Err(KnutError::Tool(format!(
            "unknown sessions subcommand {other:?}; expected list, show, export or plan"
        ))),
    }
}

pub(super) async fn verify_workspace(verbose: bool, as_json: bool) -> Result<(), KnutError> {
    let workspace = knut::Workspace::open(".")?;
    let profile = knut::CheckProfile::for_workspace(&workspace)?;
    let supervisor = Arc::new(knut::Supervisor::new(workspace.clone()));
    let runner = knut::CheckRunner::new(workspace, supervisor, profile);
    let report = runner.run_report().await?;

    if as_json {
        let serialized = serde_json::to_string_pretty(&report)
            .map_err(|err| KnutError::Tool(format!("serialize report: {err}")))?;
        println!("{serialized}");
    } else {
        print!("{}", report.summary());
        if verbose {
            for check in &report.checks {
                println!("  $ {}", check.command.join(" "));
                if !check.output.is_empty() {
                    for line in check.output.lines().take(12) {
                        println!("      {line}");
                    }
                }
            }
        }
    }

    if report.is_green() {
        if !as_json {
            println!("verified: all blocking checks passed for this revision");
        }
        Ok(())
    } else {
        Err(KnutError::Tool(
            "verification did not pass; see the evidence above".to_owned(),
        ))
    }
}

pub(super) async fn doctor(live: bool) -> Result<(), KnutError> {
    println!("knut doctor");

    match std::env::var("TYPESAFE_API_KEY") {
        Ok(key) if !key.trim().is_empty() => {
            let base = std::env::var("TYPESAFE_BASE_URL")
                .unwrap_or_else(|_| knut::DEFAULT_BASE_URL.to_owned());
            let model =
                std::env::var("TYPESAFE_MODEL").unwrap_or_else(|_| knut::DEFAULT_MODEL.to_owned());
            println!("  system one:  configured (base {base}, model {model})");
            if live {
                let system_one = knut::JevSystemOne::new(knut::TypeSafeConfig::from_env()?)?;
                let input = knut::DecisionInput::new(
                    "read the project notes".to_owned(),
                    vec!["files".to_owned()],
                );
                match knut::SystemOne::decide(&system_one, &input).await {
                    Ok(decision) => println!(
                        "    live:      ok (route {:?}, confidence {:.2})",
                        decision.route, decision.confidence
                    ),
                    Err(err) => println!("    live:      FAILED ({err})"),
                }
            }
        }
        _ => println!("  system one:  optional, not configured; reasoner plans directly"),
    }

    match knut::ProviderConfig::from_env() {
        Ok(config) => {
            let model = knut::ProviderModel::new(config)?;
            let summary = model.config().summary();
            println!(
                "  reasoner:    configured (model {}, tier {:?}, billing {:?}, {:?})",
                summary.model, summary.tier, summary.billing, summary.transport
            );
            println!(
                "               endpoint {} (timeout {:?}, credential {})",
                summary.base_url, summary.timeout, summary.api_key_source
            );
            let caps = knut::Model::capabilities(&model);
            println!(
                "               capabilities: streaming={} tools={} reasoning={} continuation={} usage={}",
                caps.streaming, caps.tools, caps.reasoning, caps.continuation, caps.usage
            );
            if live {
                let request = knut::ModelRequest::new(
                    "Reply with exactly the word pong.",
                    knut::ExpectedArtifact::Text,
                );
                let mut sink = knut::BufferedSink::new();
                match knut::Model::stream(&model, &request, &mut sink).await {
                    Ok(response) => {
                        let usage =
                            match (response.usage.input_tokens, response.usage.output_tokens) {
                                (Some(i), Some(o)) => format!("{i} in / {o} out"),
                                _ => "unknown".to_owned(),
                            };
                        println!(
                            "    live:      ok (model {}, usage {usage})",
                            response.identity.model
                        );
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        Err(error) => {
            println!("  reasoner:    unavailable ({error})");
            if live {
                return Err(error);
            }
        }
    }

    println!("  playground:  ok (static backend, offline)");

    if !live {
        println!("\nrun `knut doctor --live` to verify credentials with one real call");
    }
    Ok(())
}

pub(super) async fn openai_account_command(
    command: &str,
    args: &[String],
) -> Result<(), KnutError> {
    match command {
        "accounts" if args.is_empty() => {
            for label in knut::openai_auth::accounts()? {
                println!("{label}");
            }
        }
        "accounts" if args.len() == 1 => knut::openai_auth::select_account(&args[0]).await?,
        "login"
            if args.first().map(String::as_str) == Some("openai-codex")
                && (args.len() == 1 || args.len() == 2 && args[1] == "--new") =>
        {
            let account = knut::openai_auth::login(args.len() == 2).await?;
            println!(
                "Signed in: {account}\nOpen knut and press F2 to choose and save your ChatGPT model."
            );
        }
        "logout" if args.len() == 1 && args[0] == "openai-codex" => {
            let revoked = knut::openai_auth::logout().await?;
            println!("Signed out locally.");
            if !revoked {
                println!(
                    "Remote revocation was not confirmed. Disconnect Knut in ChatGPT Settings."
                );
            }
        }
        _ => {
            return Err(KnutError::Model(
                "Use login/logout openai-codex, or accounts [client-id]".to_owned(),
            ));
        }
    }
    Ok(())
}

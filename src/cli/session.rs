use knut::KnutError;

pub(super) async fn tui() -> Result<(), KnutError> {
    let workspace = knut::Workspace::open(".")?;
    let root = workspace.root().to_string_lossy().into_owned();

    let (engine, report) = knut::build_here();
    let mut state = knut::WorkbenchState::new(root).with_theme(knut::Theme::detect());
    state.model = report.model.clone();
    state.chatgpt_plan = report.chatgpt_plan;
    state.account = report.account.clone();
    state.endpoint = report.endpoint_label();
    state.unavailable = report.unavailable.clone();
    state.checks = report.checks.len();

    // Session work runs behind the display so a slow provider can never
    // block a keystroke.
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    let (connection_tx, connection_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(knut::run_engine_with_connections(
        engine,
        command_rx,
        event_tx,
        connection_rx,
    ));

    knut::run_shell(state, event_rx, command_tx, connection_tx)
        .await
        .map_err(|err| KnutError::Tool(format!("terminal: {err}")))
}

pub(super) async fn headless_jsonl(positionals: &[String]) -> Result<(), KnutError> {
    let mut adapter = knut::HeadlessAdapter::new(format!("headless-{}", std::process::id()));
    let (engine, _) = knut::build_here();
    let (event_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let (command_tx, commands) = tokio::sync::mpsc::unbounded_channel();
    let (input_tx, mut inputs) = tokio::sync::mpsc::unbounded_channel();
    let one_shot = !positionals.is_empty();
    if one_shot {
        let _ = command_tx.send(knut::SessionCommand::Submit {
            prompt: positionals.join(" "),
            options: Default::default(),
        });
    } else {
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                if input_tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    emit(&knut::HeadlessEvent::Ready {
        protocol_version: knut::JSONL_PROTOCOL_VERSION,
        session: adapter.session.id.clone(),
    });
    let client = async move {
        let mut command_tx = Some(command_tx);
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { break; };
                    emit(&adapter.translate(&event));
                    let terminal = adapter.outcome(&event);
                    if let Some(outcome) = &terminal { emit(outcome); }
                    if one_shot && (terminal.is_some() || matches!(event, knut::SessionEvent::WaitingForUser { .. })) {
                        command_tx.take();
                    }
                }
                line = inputs.recv(), if !one_shot && command_tx.is_some() => {
                    match line {
                        Some(Ok(line)) if line.trim().is_empty() => {}
                        Some(Ok(line)) => match adapter.handle_line(&line) {
                            Ok(None) => { command_tx.take(); }
                            Ok(Some(command)) => {
                                let _ = command_tx.as_ref().unwrap().send(command);
                            }
                            Err(error) => emit(&knut::HeadlessEvent::Error { protocol_version: knut::JSONL_PROTOCOL_VERSION, error }),
                        },
                        Some(Err(error)) => {
                            eprintln!("reading stdin: {error}");
                            command_tx.take();
                        }
                        None => { command_tx.take(); }
                    }
                }
            }
        }
    };
    tokio::join!(knut::run_engine(engine, commands, event_tx), client);
    Ok(())
}

fn emit(event: &knut::HeadlessEvent) {
    match knut::to_jsonl(event) {
        Ok(line) => println!("{line}"),
        Err(err) => eprintln!("{}", knut::diagnostic(&format!("{err:?}"))),
    }
}

pub(super) async fn task_run_with_census(
    prompts: &[String],
    verbose: bool,
    yes: bool,
    census_path: Option<&str>,
) -> Result<(), KnutError> {
    let Some(path) = census_path else {
        return task_run(prompts, verbose, yes).await;
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|err| KnutError::Tool(format!("cannot create census {path:?}: {err}")))?;
    let started = std::time::Instant::now();
    let (result, calls) = knut::capture_model_calls(task_run(prompts, verbose, yes)).await;
    let report = knut::ModelCallReport::new(
        calls,
        result.is_ok(),
        started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    );
    serde_json::to_writer_pretty(&mut file, &report)
        .map_err(|err| KnutError::Tool(format!("cannot write census {path:?}: {err}")))?;
    file.sync_all()
        .map_err(|err| KnutError::Tool(format!("cannot persist census {path:?}: {err}")))?;
    eprintln!("generator census: {} calls → {path}", report.calls.len());
    result
}

async fn task_run(prompts: &[String], verbose: bool, yes: bool) -> Result<(), KnutError> {
    let prompt = prompts.join(" ");
    if prompt.trim().is_empty() {
        return Err(KnutError::Tool("knut run needs a prompt".to_owned()));
    }
    let workspace = knut::Workspace::open(".")?;
    let (engine, report) = knut::build_with_write_approval(workspace, yes);
    if let Some(warning) = &report.routing_warning {
        eprintln!("  routing: {warning}");
    }
    if let Some(reason) = report.unavailable {
        return Err(KnutError::Tool(reason));
    }
    println!("knut run: {prompt}");
    println!(
        "  reasoner  {}",
        report.model.as_deref().unwrap_or("unavailable")
    );
    if !report.checks.is_empty() {
        println!("  checks    {}", report.checks.join(", "));
    }
    let (event_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let (command_tx, commands) = tokio::sync::mpsc::unbounded_channel();
    let _ = command_tx.send(knut::SessionCommand::Submit {
        prompt,
        options: Default::default(),
    });
    let client = async move {
        let mut command_tx = Some(command_tx);
        let mut complete = false;
        while let Some(event) = events.recv().await {
            print_run_event(&event, verbose);
            match event {
                knut::SessionEvent::TaskCompleted { .. } => {
                    complete = true;
                    command_tx.take();
                }
                knut::SessionEvent::TaskFailed { .. }
                | knut::SessionEvent::TaskCancelled { .. }
                | knut::SessionEvent::WaitingForUser { .. } => {
                    command_tx.take();
                }
                _ => {}
            }
        }
        complete
    };
    let (_, complete) = tokio::join!(knut::run_engine(engine, commands, event_tx), client);
    if complete {
        Ok(())
    } else {
        Err(KnutError::Tool("The task did not complete. Use a TUI/JSONL session to answer questions or approve writes, or --yes to pre-approve writes in a new run".to_owned()))
    }
}

fn print_run_event(event: &knut::SessionEvent, verbose: bool) {
    match event {
        knut::SessionEvent::Generating { .. } => println!("  generating..."),
        knut::SessionEvent::PlanStarted { node_count, .. } => {
            println!("  plan: {node_count} nodes")
        }
        knut::SessionEvent::NodeResult {
            node_label,
            status,
            output,
            ..
        } => {
            println!("  {node_label}: {status:?}");
            if verbose || *status != knut::NodeStatus::Succeeded {
                println!(
                    "    {}",
                    output.to_string().chars().take(6000).collect::<String>()
                );
            }
        }
        knut::SessionEvent::ToolCallProposed {
            name, arguments, ..
        } => println!("  proposed {name}: {arguments}"),
        knut::SessionEvent::TextDelta { text, .. } => print!("{text}"),
        knut::SessionEvent::WaitingForUser { message, wait, .. } => {
            println!("  {message} ({wait:?})")
        }
        knut::SessionEvent::TaskCompleted { summary, .. } => println!("\n{summary}"),
        knut::SessionEvent::TaskFailed { reason, .. } => eprintln!("\nfailed: {reason}"),
        knut::SessionEvent::TaskCancelled { .. } => eprintln!("\ncancelled"),
        knut::SessionEvent::RuntimeError { message, .. } => eprintln!("{message}"),
        _ => {}
    }
}

#[cfg(test)]
mod census_tests {
    use super::*;
    use crate::cli::run;

    #[tokio::test]
    async fn failed_run_exports_a_report_and_existing_output_is_preserved() {
        let path = std::env::temp_dir().join(format!(
            "knut-census-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let result = task_run_with_census(&[], false, false, path.to_str()).await;
        assert!(result.is_err());
        let original = std::fs::read(&path).unwrap();
        let report: knut::ModelCallReport = serde_json::from_slice(&original).unwrap();
        assert!(!report.run_succeeded);
        assert!(report.calls.is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(
            task_run_with_census(&[], false, false, path.to_str())
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn census_flag_requires_a_path_and_a_run_command() {
        assert!(
            run(vec!["run".to_owned(), "--census".to_owned()])
                .await
                .is_err()
        );
        assert!(
            run(vec![
                "tui".to_owned(),
                "--census".to_owned(),
                "unused.json".to_owned()
            ])
            .await
            .is_err()
        );
    }
}

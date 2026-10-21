mod commands;
mod playground;
mod session;

use knut::KnutError;

pub(crate) async fn run(args: Vec<String>) -> Result<(), KnutError> {
    let mut verbose = false;
    let mut as_json = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut capabilities: Vec<String> = Vec::new();
    let mut confidence_floor: Option<f32> = None;
    let mut backend = String::from("static");
    let mut approve_writes = false;
    let mut census_path = None;

    let mut iter = args.into_iter();
    let Some(command) = iter.next() else {
        if std::io::IsTerminal::is_terminal(&std::io::stdin())
            && std::io::IsTerminal::is_terminal(&std::io::stdout())
        {
            return session::tui().await;
        }
        println!("{}", usage());
        return Ok(());
    };

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--verbose" | "-v" => verbose = true,
            // Explicit consent for writes in a headless run. Without it,
            // a write blocks and the exact approval key is printed.
            "--yes" | "-y" => approve_writes = true,
            "--json" => as_json = true,
            "--census" => {
                census_path = Some(iter.next().ok_or_else(|| {
                    KnutError::Tool("--census needs a new output file path".to_owned())
                })?);
            }
            "--capability" | "-c" => {
                let value = iter
                    .next()
                    .ok_or_else(|| KnutError::SystemOne("--capability needs a value".to_owned()))?;
                capabilities.push(value);
            }
            "--backend" => {
                backend = iter
                    .next()
                    .ok_or_else(|| KnutError::SystemOne("--backend needs a value".to_owned()))?;
            }
            "--confidence" => {
                let value = iter
                    .next()
                    .ok_or_else(|| KnutError::SystemOne("--confidence needs a value".to_owned()))?;
                confidence_floor = Some(value.parse().map_err(|_| {
                    KnutError::SystemOne(format!("--confidence: {value:?} is not a number"))
                })?);
            }
            other => positionals.push(other.to_owned()),
        }
    }

    if census_path.is_some() && command != "run" {
        return Err(KnutError::Tool(
            "--census is supported only by knut run".to_owned(),
        ));
    }

    match command.as_str() {
        "route" => {
            let prompt = positionals.join(" ");
            if prompt.trim().is_empty() {
                return Err(KnutError::SystemOne(usage()));
            }
            playground::route_once(
                prompt,
                capabilities,
                confidence_floor,
                verbose,
                as_json,
                playground::backend(&backend)?,
            )
            .await
        }
        "repl" => {
            playground::repl(
                capabilities,
                confidence_floor,
                verbose,
                playground::backend(&backend)?,
            )
            .await
        }
        "demo-tree" => playground::demo_tree(verbose).await,
        "eval" => playground::eval().await,
        "doctor" => commands::doctor(positionals.iter().any(|arg| arg == "--live")).await,
        "login" | "logout" | "accounts" => {
            commands::openai_account_command(&command, &positionals).await
        }
        "models" => {
            let model = knut::ProviderModel::new(knut::ProviderConfig::from_env()?)?;
            for (id, name) in model.list_models().await? {
                println!("{id}  {name}");
            }
            Ok(())
        }
        "verify" => commands::verify_workspace(verbose, as_json).await,
        "tui" | "workbench" => session::tui().await,
        "sessions" => commands::sessions(&positionals, as_json).await,
        "bench" => playground::bench().await,
        "lsp" => commands::lsp_status().await,
        "jsonl" => session::headless_jsonl(&positionals).await,
        "run" => {
            session::task_run_with_census(
                &positionals,
                verbose,
                approve_writes,
                census_path.as_deref(),
            )
            .await
        }
        "release" => commands::release_artifacts().await,
        "--help" | "-h" | "help" => {
            println!("{}", usage());
            Ok(())
        }
        other => Err(KnutError::SystemOne(format!(
            "unknown command {other:?}\n{}",
            usage()
        ))),
    }
}

fn usage() -> String {
    "Knut — a general agent harness for your terminal

USAGE:
  knut                     open the interactive agent session
  knut run <prompt> [--yes]  run one task (--yes approves writes)
                           --census <new.json> records generator calls, including failures
  knut tui                 open the interactive agent session explicitly
  knut doctor [--live]      diagnose setup (--live calls configured providers)
  knut login openai-codex [--new]  sign in with ChatGPT (or add an account)
  knut logout openai-codex  sign out of the selected ChatGPT account
  knut accounts [<client-id>]  list or select a saved ChatGPT account
  knut models              list models available to the configured provider
  knut verify [--json]      run build, test and lint checks
  knut bench               run the pilot benchmark and write an inspectable report
  knut lsp                 report language-server availability and negotiated features
  knut jsonl [prompt]      headless JSONL: commands on stdin, events on stdout
  knut release             print versioned release artifact instructions
  knut sessions list       list stored sessions
  knut sessions show <id>  replay a stored transcript (state only)
  knut sessions export <id> [--raw]  export without executing anything
  knut sessions plan <id>  report whether a session can be resumed

OFFLINE PLAYGROUND:
  knut route <prompt> [--capability <id>]... [--confidence <f32>] [--verbose] [--json]
  knut repl [--capability <id>]... [--verbose]
  knut demo-tree [--verbose]
  knut eval

Playground backends (--backend):
  static (default)   deterministic mock, fully offline
  jev                live TypeSafe System One API; reads TYPESAFE_API_KEY,
                     optional TYPESAFE_BASE_URL / TYPESAFE_MODEL

Connection and model: F2 Settings in the TUI; saved choices take priority.
Environment provider configuration (knut doctor, and KNUT_PROVIDER_* env):
  KNUT_PROVIDER           zai (default) | openai | openai-codex | chat-completions
  KNUT_PROVIDER_API_KEY   API credential; OPENAI_API_KEY / ZAI_API_KEY also accepted
  KNUT_PROVIDER_BASE_URL  default https://api.z.ai/api/coding/paas/v4
  KNUT_PROVIDER_MODEL     default glm-5.3-flash (Z.ai), gpt-6.1-sol (OpenAI)
  KNUT_PROVIDER_TIER      fast | standard | reasoner"
        .to_owned()
}

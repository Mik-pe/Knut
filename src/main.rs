mod cli;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if std::env::var("KNUT_LOAD_ENV").as_deref() != Ok("0") {
        load_env_file();
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    match runtime.block_on(cli::run(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::ExitCode::from(2)
        }
    }
}

fn load_env_file() {
    let Ok(contents) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in contents.lines() {
        let Some((key, value)) = env_assignment(line) else {
            continue;
        };
        if std::env::var_os(key).is_some() {
            continue;
        }
        // SAFETY: this runs before any worker thread starts, so there is
        // no concurrent reader of the environment.
        unsafe { std::env::set_var(key, value) };
    }
}

fn env_assignment(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line);
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() || key.contains('\0') || value.contains('\0') {
        return None;
    }
    let value = value.trim();
    let value = ['"', '\'']
        .into_iter()
        .find_map(|quote| value.strip_prefix(quote)?.strip_suffix(quote))
        .unwrap_or(value);
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::env_assignment;

    #[test]
    fn dotenv_assignments_handle_export_and_matching_quotes() {
        assert_eq!(
            env_assignment(" export MODEL = 'fixture model' "),
            Some(("MODEL", "fixture model"))
        );
        assert_eq!(env_assignment("VALUE=can't"), Some(("VALUE", "can't")));
        assert_eq!(
            env_assignment("VALUE=\"unclosed"),
            Some(("VALUE", "\"unclosed"))
        );
        assert_eq!(env_assignment("EMPTY="), Some(("EMPTY", "")));
    }

    #[test]
    fn invalid_dotenv_assignments_cannot_panic_environment_setup() {
        for line in [
            "# comment",
            "missing separator",
            "=value",
            "BAD\0KEY=value",
            "KEY=bad\0value",
        ] {
            assert_eq!(env_assignment(line), None);
        }
    }
}

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::{KnutError, Theme};

const MAX_PREFERENCES_BYTES: u64 = 16 * 1024;
static SAVE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalPreferences {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduced_motion: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_background: Option<bool>,
}

impl TerminalPreferences {
    pub fn load() -> Result<Self, KnutError> {
        Self::load_from(&configured_path()?)
    }

    fn load_from(path: &Path) -> Result<Self, KnutError> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(preference_error("Cannot read terminal preferences", error)),
        };
        let metadata = file
            .metadata()
            .map_err(|error| preference_error("Cannot inspect terminal preferences", error))?;
        if !metadata.is_file() || !private(&metadata) {
            return Err(KnutError::Tool(
                "Terminal preferences require an owner-only regular file".to_owned(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_PREFERENCES_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| preference_error("Cannot read terminal preferences", error))?;
        if bytes.len() as u64 > MAX_PREFERENCES_BYTES {
            return Err(KnutError::Tool(
                "Terminal preferences exceed 16 KiB".to_owned(),
            ));
        }
        serde_json::from_slice(&bytes).map_err(|_| KnutError::Tool("Invalid terminal preferences; restore terminal-preferences.json before changing settings".to_owned()))
    }

    pub fn save(&self) -> Result<(), KnutError> {
        self.save_to(&configured_path()?)
    }

    fn save_to(&self, path: &Path) -> Result<(), KnutError> {
        let parent = path
            .parent()
            .ok_or_else(|| KnutError::Tool("Terminal preferences have no directory".to_owned()))?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| {
                preference_error("Cannot create terminal preferences directory", error)
            })?;
        let metadata = std::fs::symlink_metadata(parent).map_err(|error| {
            preference_error("Cannot inspect terminal preferences directory", error)
        })?;
        if !metadata.is_dir() || !private(&metadata) {
            return Err(KnutError::Tool(
                "Terminal preferences directory requires owner-only permissions".to_owned(),
            ));
        }
        Self::load_from(path)?;
        let temporary = parent.join(format!(
            ".terminal-preferences-{}-{}",
            std::process::id(),
            SAVE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec(self)
            .map_err(|error| preference_error("Cannot encode terminal preferences", error))?;
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|error| preference_error("Cannot save terminal preferences", error))?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|error| preference_error("Cannot sync terminal preferences", error))?;
            std::fs::rename(&temporary, path)
                .map_err(|error| preference_error("Cannot replace terminal preferences", error))?;
            File::open(parent)
                .and_then(|file| file.sync_all())
                .map_err(|error| {
                    preference_error("Cannot sync terminal preferences directory", error)
                })
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }

    pub fn apply(&self, theme: Theme) -> Theme {
        self.apply_with_env(theme, |key| std::env::var(key).ok())
    }

    fn apply_with_env(&self, mut theme: Theme, get: impl Fn(&str) -> Option<String>) -> Theme {
        if get("KNUT_TUI_MOTION").is_none()
            && let Some(reduced) = self.reduced_motion
        {
            theme.reduced_motion = reduced;
        }
        if get("TERM").as_deref() == Some("dumb") {
            theme.reduced_motion = true;
            theme.glyphs.unicode = false;
        }
        if let Some(terminal) = self.terminal_background {
            theme.paint_background = !terminal && theme.level.is_color();
        }
        theme
    }

    pub fn toggled_motion(&self, theme: &Theme) -> Result<Self, KnutError> {
        if std::env::var_os("KNUT_TUI_MOTION").is_some() {
            return Err(KnutError::Tool(
                "KNUT_TUI_MOTION controls motion; remove it to change this setting".to_owned(),
            ));
        }
        if std::env::var("TERM").as_deref() == Ok("dumb") {
            return Err(KnutError::Tool(
                "This terminal requires reduced motion".to_owned(),
            ));
        }
        Ok(Self {
            reduced_motion: Some(!theme.reduced_motion),
            ..self.clone()
        })
    }

    pub fn toggled_background(&self, theme: &Theme) -> Result<Self, KnutError> {
        if !theme.level.is_color() {
            return Err(KnutError::Tool(
                "Monochrome uses the terminal background; configure color support to change it"
                    .to_owned(),
            ));
        }
        Ok(Self {
            terminal_background: Some(theme.paint_background),
            ..self.clone()
        })
    }
}

fn configured_path() -> Result<PathBuf, KnutError> {
    let directory = if let Some(path) = std::env::var_os("KNUT_CONFIG_DIR") {
        PathBuf::from(path)
    } else if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(path).join("knut")
    } else {
        PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| KnutError::Tool("HOME is not set".to_owned()))?,
        )
        .join(".config/knut")
    };
    Ok(directory.join("terminal-preferences.json"))
}

fn private(metadata: &std::fs::Metadata) -> bool {
    // SAFETY: geteuid has no preconditions.
    metadata.uid() == unsafe { libc::geteuid() } && metadata.permissions().mode() & 0o077 == 0
}

fn preference_error(context: &str, error: impl std::fmt::Display) -> KnutError {
    KnutError::Tool(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColorLevel;

    #[test]
    fn preferences_round_trip_without_replacing_a_corrupt_file() {
        let directory = std::env::temp_dir().join(format!(
            "knut-preferences-{}-{}",
            std::process::id(),
            SAVE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("terminal-preferences.json");
        let preferences = TerminalPreferences {
            reduced_motion: Some(true),
            terminal_background: Some(true),
        };
        assert_eq!(
            TerminalPreferences::load_from(&path).unwrap(),
            TerminalPreferences::default()
        );
        preferences.save_to(&path).unwrap();
        assert_eq!(TerminalPreferences::load_from(&path).unwrap(), preferences);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&path, "broken preferences").unwrap();
        assert!(preferences.save_to(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "broken preferences"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn saved_preferences_preserve_explicit_motion_and_monochrome_constraints() {
        let preferences = TerminalPreferences {
            reduced_motion: Some(false),
            terminal_background: Some(false),
        };
        let theme = Theme::from_env(|key| match key {
            "KNUT_TUI_MOTION" => Some("off".to_owned()),
            "NO_COLOR" => Some("1".to_owned()),
            _ => None,
        });
        let resolved = preferences.apply_with_env(theme, |key| {
            (key == "KNUT_TUI_MOTION").then(|| "off".to_owned())
        });
        assert!(resolved.reduced_motion);
        assert!(!resolved.paint_background);
        assert_eq!(resolved.level, ColorLevel::Mono);
        let resolved = preferences.apply_with_env(Theme::for_level(ColorLevel::TrueColor), |key| {
            (key == "TERM").then(|| "dumb".to_owned())
        });
        assert!(resolved.reduced_motion);
        assert!(!resolved.glyphs.unicode);
    }

    #[test]
    fn saved_motion_and_background_apply_without_environment_overrides() {
        let preferences = TerminalPreferences {
            reduced_motion: Some(true),
            terminal_background: Some(true),
        };
        let resolved =
            preferences.apply_with_env(Theme::for_level(ColorLevel::TrueColor), |_| None);
        assert!(resolved.reduced_motion);
        assert!(!resolved.paint_background);
    }
}

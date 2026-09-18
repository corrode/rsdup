//! User preferences; confy owns path discovery and TOML persistence.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{theme::ThemeName, tui::GroupSort};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    #[serde(with = "theme_name")]
    pub theme: ThemeName,
    pub diff_enabled: bool,
    pub sort: GroupSort,
    pub editor: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeName::default(),
            diff_enabled: true,
            sort: GroupSort::Occurrences,
            editor: None,
        }
    }
}

pub(crate) fn path() -> Result<PathBuf> {
    confy::get_configuration_file_path("rsdup", "config")
        .context("could not determine user config path")
}

impl Settings {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        confy::load_path(path).with_context(|| format!("could not load config {}", path.display()))
    }

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        confy::store_path(path, self)
            .with_context(|| format!("could not save config {}", path.display()))
    }
}

mod theme_name {
    use super::ThemeName;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(name: &ThemeName, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(name.slug())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ThemeName, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_defaults_and_round_trips_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rsdup/config.toml");
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
        for &theme in ThemeName::all() {
            let settings = Settings {
                theme,
                diff_enabled: false,
                sort: GroupSort::Path,
                editor: Some("zed --wait".into()),
            };
            settings.save(&path).unwrap();
            assert_eq!(Settings::load(&path).unwrap(), settings);
        }
    }

    #[test]
    fn partial_config_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "diff_enabled = false\n").unwrap();
        assert_eq!(
            Settings::load(&path).unwrap(),
            Settings {
                diff_enabled: false,
                ..Settings::default()
            }
        );
    }

    #[test]
    fn invalid_config_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for contents in [
            "not toml",
            "theme = 'unknown'",
            "sort = 'unknown'",
            "diff_enabled = 'yes'",
            "typo = true",
        ] {
            std::fs::write(&path, contents).unwrap();
            let error = Settings::load(&path).unwrap_err();
            assert!(error.to_string().contains(&path.display().to_string()));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        }
    }

    #[test]
    fn save_errors_include_path() {
        let dir = tempfile::tempdir().unwrap();
        let error = Settings::default().save(dir.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&dir.path().display().to_string())
        );
    }
}

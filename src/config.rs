//! The optional `atp.toml` file.
//!
//! Nothing in it is required. A solution directory already describes its own
//! projects and configurations, so the file exists to give solutions short
//! names, to set defaults, and to point at a Studio installed somewhere
//! unusual.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{AppError, AppResult};

pub(crate) const FILE_NAME: &str = "atp.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(default)]
    pub(crate) defaults: Defaults,
    #[serde(default)]
    pub(crate) studio: Studio,
    #[serde(default)]
    pub(crate) projects: BTreeMap<String, ProjectConfig>,
    /// Where the file was read from, so relative paths inside it resolve.
    #[serde(skip)]
    pub(crate) directory: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Defaults {
    pub(crate) project: Option<String>,
    pub(crate) configuration: Option<String>,
    pub(crate) backend: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Studio {
    pub(crate) root: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectConfig {
    pub(crate) description: Option<String>,
    /// The `.atsln` (or `.cproj`) this name refers to.
    pub(crate) solution: Option<String>,
    /// Which project inside the solution, when it holds several.
    pub(crate) project: Option<String>,
    pub(crate) configuration: Option<String>,
}

impl Config {
    /// Loads the configuration, or an empty one when there is no file.
    ///
    /// An explicitly named file must exist; a discovered one need not.
    pub(crate) fn load(explicit: Option<&Path>) -> AppResult<Self> {
        let path = match explicit {
            Some(path) => {
                if !path.is_file() {
                    return Err(AppError::Usage(format!(
                        "{} does not exist",
                        path.display()
                    )));
                }
                Some(path.to_path_buf())
            }
            None => discover(),
        };

        let Some(path) = path else {
            return Ok(Self::default());
        };

        let text = fs::read_to_string(&path).map_err(|error| {
            AppError::Runtime(format!("cannot read {}: {error}", path.display()))
        })?;
        let mut config: Self = toml::from_str(&text).map_err(|error| {
            AppError::Usage(format!("{} is not valid: {error}", path.display()))
        })?;
        config.directory = path.parent().map(Path::to_path_buf);
        Ok(config)
    }

    pub(crate) fn names(&self) -> Vec<&str> {
        self.projects.keys().map(String::as_str).collect()
    }

    /// Resolves a path written in the configuration file against its directory.
    pub(crate) fn resolve(&self, path: PathBuf) -> PathBuf {
        if path.is_absolute() {
            return path;
        }
        match &self.directory {
            Some(directory) => directory.join(path),
            None => path,
        }
    }
}

/// The first configuration file in the search order, if any exists.
///
/// The working directory comes first so that a checkout can carry its own
/// file, then the executable's directory for a portable install, then the
/// user's configuration directory.
fn discover() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(directory) = env::current_dir() {
        candidates.push(directory.join(FILE_NAME));
    }
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join(FILE_NAME));
    }
    if let Some(directory) = user_directory() {
        candidates.push(directory.join(FILE_NAME));
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Where a user-wide `atp.toml` lives on this platform.
pub(crate) fn user_directory() -> Option<PathBuf> {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(xdg).join("atp"));
    }
    if cfg!(windows)
        && let Some(appdata) = env::var_os("APPDATA").filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(appdata).join("atp"));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".config").join("atp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = concat!(
        "[defaults]\n",
        "project = 'udc4k'\n",
        "configuration = 'Release_to_application_partition'\n",
        "\n",
        "[studio]\n",
        "root = 'C:\\Program Files (x86)\\Atmel\\Studio\\7.0'\n",
        "\n",
        "[projects.udc4k]\n",
        "description = 'main firmware'\n",
        "solution = 'C:\\work\\app\\app.atsln'\n",
        "configuration = 'Debug'\n",
    );

    #[test]
    fn reads_defaults_projects_and_studio_root() {
        let config: Config = toml::from_str(SAMPLE).unwrap();
        assert_eq!(config.defaults.project.as_deref(), Some("udc4k"));
        assert_eq!(
            config.defaults.configuration.as_deref(),
            Some("Release_to_application_partition")
        );
        assert!(config.studio.root.unwrap().ends_with("7.0"));

        let project = config.projects.get("udc4k").unwrap();
        assert_eq!(
            project.solution.as_deref(),
            Some("C:\\work\\app\\app.atsln")
        );
        assert_eq!(project.configuration.as_deref(), Some("Debug"));
    }

    #[test]
    fn an_empty_file_is_valid() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.projects.is_empty());
        assert!(config.defaults.project.is_none());
    }

    #[test]
    fn rejects_a_misspelled_key() {
        // Silently ignoring a typo would hide the setting the user meant.
        let error = toml::from_str::<Config>("[defaults]\nconfigration = 'Debug'\n").unwrap_err();
        assert!(error.to_string().contains("configration"));
    }

    #[test]
    fn lists_the_configured_names() {
        // The names are what an unknown-project error offers the user.
        let config: Config = toml::from_str(SAMPLE).unwrap();
        assert_eq!(config.names(), vec!["udc4k"]);
    }

    #[test]
    fn resolves_relative_paths_against_the_file() {
        let mut config = Config::default();
        config.directory = Some(PathBuf::from("/work"));
        assert_eq!(
            config.resolve(PathBuf::from("app/app.atsln")),
            PathBuf::from("/work/app/app.atsln")
        );
        let absolute = if cfg!(windows) {
            PathBuf::from("C:\\other\\app.atsln")
        } else {
            PathBuf::from("/other/app.atsln")
        };
        assert_eq!(config.resolve(absolute.clone()), absolute);
    }
}

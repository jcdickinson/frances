use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use frances_config::EnvString;
use serde::{Deserialize, Serialize};

use crate::Error;

/// Definitions are available to select, never automatically activated.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub servers: BTreeMap<String, ServerConfig>,
    #[serde(default)]
    pub presets: BTreeMap<String, Preset>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub servers: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub transport: Transport,
    #[serde(default = "shutdown_timeout")]
    pub shutdown_timeout_seconds: u64,
    #[serde(default)]
    pub lifecycle: Lifecycle,
    #[serde(default = "startup_timeout")]
    pub startup_timeout_seconds: u64,
    #[serde(default = "request_timeout")]
    pub request_timeout_seconds: u64,
}

fn shutdown_timeout() -> u64 {
    5
}

fn startup_timeout() -> u64 {
    30
}
fn request_timeout() -> u64 {
    120
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    #[default]
    Auto,
    Modern,
    Legacy,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, EnvString>,
        cwd: Option<PathBuf>,
    },
    #[serde(rename = "local-stdio")]
    LocalStdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, EnvString>,
        cwd: Option<PathBuf>,
    },
    Http {
        url: url::Url,
        #[serde(default)]
        headers: BTreeMap<String, EnvString>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    #[serde(default)]
    pub presets: Vec<String>,
    #[serde(default)]
    pub servers: Vec<String>,
}

impl Config {
    pub fn resolve(&self, selection: &Selection) -> Result<Vec<String>, Error> {
        let mut servers: BTreeSet<String> = selection.servers.iter().cloned().collect();
        for name in &selection.presets {
            let preset = self
                .presets
                .get(name)
                .ok_or_else(|| Error::UnknownPreset(name.clone()))?;
            servers.extend(preset.servers.iter().cloned());
        }
        for name in &servers {
            let config = self
                .servers
                .get(name)
                .ok_or_else(|| Error::UnknownServer(name.clone()))?;
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(Error::InvalidServerName(name.clone()));
            }
            if config.startup_timeout_seconds == 0 || config.request_timeout_seconds == 0 {
                return Err(Error::InvalidTimeout(name.clone()));
            }
            if let Transport::Http { url, .. } = &config.transport
                && (!matches!(url.scheme(), "http" | "https")
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some())
            {
                return Err(Error::InvalidUrl(name.clone()));
            }
        }
        Ok(servers.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
            [servers.docs]
            transport = { type = "stdio", command = "docs" }
            [servers.planner]
            transport = { type = "stdio", command = "planner" }
            [presets.rust]
            servers = ["docs"]
            [presets.frances]
            servers = ["planner", "docs"]
        "#,
        )
        .unwrap()
    }

    #[test]
    fn selection_is_explicit_and_composition_deduplicates() {
        let config = config();
        assert_eq!(config.servers["docs"].shutdown_timeout_seconds, 5);
        assert!(config.resolve(&Selection::default()).unwrap().is_empty());
        let selection = Selection {
            presets: vec!["frances".into(), "rust".into(), "rust".into()],
            servers: vec!["docs".into()],
        };
        assert_eq!(config.resolve(&selection).unwrap(), ["docs", "planner"]);
    }

    #[test]
    fn broken_selections_fail_before_starting_servers() {
        let mut config = config();
        assert!(matches!(
            config.resolve(&Selection {
                presets: vec!["missing".into()],
                ..Default::default()
            }),
            Err(Error::UnknownPreset(_))
        ));
        config
            .presets
            .get_mut("rust")
            .unwrap()
            .servers
            .push("missing".into());
        assert!(matches!(
            config.resolve(&Selection {
                presets: vec!["rust".into()],
                ..Default::default()
            }),
            Err(Error::UnknownServer(_))
        ));
    }
}

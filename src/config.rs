use crate::Unix;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;

/// Represents a [`PodcastConfig`] value that is either enabled, disabled, or we defer to the
/// global config.
#[derive(Clone, Copy, Debug, Default)]
pub enum ConfigOption<T> {
    /// Defer to the value in the global config.
    #[default]
    UseGlobal,
    /// Use this value for configuration.
    Enabled(T),
    /// Don't use any values.
    Disabled,
}

impl<T: Clone> ConfigOption<T> {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled(_))
    }

    pub fn into_val(self, global_value: Option<&T>) -> Option<T> {
        match self {
            Self::Disabled => None,
            Self::Enabled(t) => Some(t),
            Self::UseGlobal => global_value.cloned(),
        }
    }
}

fn default_name_pattern() -> String {
    "{pubdate::%Y-%m-%d} {rss::episode::title}".to_string()
}

/// Configuration for a specific podcast.
#[derive(Debug, Clone)]
pub struct Config {
    pub url: String,
    pub name_pattern: String,
    pub download_path: PathBuf,
    pub custom_tags: HashMap<String, String>,
    pub download_hook: Option<PathBuf>,
    pub mode: DownloadMode,
}

impl Config {
    pub fn new(global_config: &GlobalConfig, podcast_config: PodcastConfig) -> Self {
        let mode = match (
            podcast_config.backlog_start,
            podcast_config.backlog_interval,
        ) {
            (None, None) => DownloadMode::Standard {
                max_days: podcast_config
                    .max_days
                    .into_val(global_config.max_days.as_ref()),
                max_episodes: podcast_config
                    .max_episodes
                    .into_val(global_config.max_episodes.as_ref()),
                earliest_date: podcast_config
                    .earliest_date
                    .into_val(global_config.earliest_date.as_ref()),
            },
            (Some(_), None) => {
                eprintln!("missing backlog_interval");
                std::process::exit(1);
            }
            (None, Some(_)) => {
                eprintln!("missing backlog_start");
                std::process::exit(1);
            }
            (Some(start), Some(interval)) => {
                if podcast_config.max_days.is_enabled() {
                    eprintln!("'max_days' not compatible with backlog mode.");
                    std::process::exit(1);
                }

                if podcast_config.max_episodes.is_enabled() {
                    eprintln!("'max_episodes' not compatible with backlog mode. Consider moving the start_date variable.");
                    std::process::exit(1);
                }

                if podcast_config.earliest_date.is_enabled() {
                    eprintln!("'earliest_date' not compatible with backlog mode.");
                    std::process::exit(1);
                }

                let Ok(start) = chrono::NaiveDate::parse_from_str(&start, "%Y-%m-%d") else {
                    eprintln!("invalid backlog_start format. Use YYYY-MM-DD");
                    std::process::exit(1);
                };

                let start = start.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();

                DownloadMode::Backlog {
                    start: std::time::Duration::from_secs(start as u64),
                    interval,
                }
            }
        };

        let custom_tags = {
            let mut map = HashMap::with_capacity(
                global_config.custom_tags.len() + podcast_config.custom_tags.len(),
            );

            for (key, val) in global_config.custom_tags.iter() {
                map.insert(key.clone(), val.clone());
            }

            for (key, val) in podcast_config.custom_tags.iter() {
                map.insert(key.clone(), val.clone());
            }
            map
        };

        let download_hook = podcast_config
            .download_hook
            .into_val(global_config.download_hook.as_ref());

        let download_path = podcast_config
            .path
            .unwrap_or_else(|| global_config.path.clone());

        Self {
            url: podcast_config.url,
            name_pattern: global_config.name_pattern.clone(),
            mode,
            custom_tags,
            download_hook,
            download_path,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    #[serde(default = "default_name_pattern")]
    name_pattern: String,
    max_days: Option<i64>,
    max_episodes: Option<i64>,
    path: PathBuf,
    earliest_date: Option<String>,
    #[serde(default)]
    custom_tags: HashMap<String, String>,
    download_hook: Option<PathBuf>,
}

impl GlobalConfig {
    pub fn load() -> Result<Self> {
        let p = crate::utils::config_toml()?;

        if !p.exists() {
            let default = Self::default();
            let s = toml::to_string_pretty(&default)?;
            let mut f = std::fs::File::create(&p)?;
            f.write_all(s.as_bytes())?;
        }
        let str = std::fs::read_to_string(p)?;

        Ok(toml::from_str(&str)?)
    }
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            name_pattern: default_name_pattern(),
            max_days: Some(120),
            max_episodes: Some(10),
            path: {
                let Some(home) = dirs::home_dir() else {
                    eprintln!("unable to load home directory");
                    std::process::exit(1);
                };
                home.join(crate::APPNAME)
            },
            earliest_date: None,
            custom_tags: Default::default(),
            download_hook: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DownloadMode {
    Standard {
        max_days: Option<i64>,
        earliest_date: Option<String>,
        max_episodes: Option<i64>,
    },
    Backlog {
        start: Unix,
        interval: i64,
    },
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PodcastConfig {
    url: String,
    path: Option<PathBuf>,
    #[serde(default, deserialize_with = "deserialize_config_option_int")]
    max_days: ConfigOption<i64>,
    #[serde(default, deserialize_with = "deserialize_config_option_int")]
    max_episodes: ConfigOption<i64>,
    #[serde(default, deserialize_with = "deserialize_config_option_string")]
    earliest_date: ConfigOption<String>,
    #[serde(default, deserialize_with = "deserialize_config_option_pathbuf")]
    download_hook: ConfigOption<PathBuf>,
    backlog_start: Option<String>,
    backlog_interval: Option<i64>,
    #[serde(default)]
    custom_tags: HashMap<String, String>,
}

fn deserialize_config_option_int<'de, D>(deserializer: D) -> Result<ConfigOption<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;

    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        Some(Value::Number(n)) if n.is_i64() => Ok(ConfigOption::Enabled(n.as_i64().unwrap())),
        Some(Value::Bool(false)) => Ok(ConfigOption::Disabled),
        _ => Err(serde::de::Error::custom(
            "Invalid type for configuration option",
        )),
    }
}

fn deserialize_config_option_string<'de, D>(
    deserializer: D,
) -> Result<ConfigOption<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;

    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        Some(Value::String(s)) => Ok(ConfigOption::Enabled(s)),
        Some(Value::Bool(false)) => Ok(ConfigOption::Disabled),
        _ => Err(serde::de::Error::custom(
            "Invalid type for configuration option",
        )),
    }
}

fn deserialize_config_option_pathbuf<'de, D>(
    deserializer: D,
) -> Result<ConfigOption<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;

    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        Some(Value::String(s)) => Ok(ConfigOption::Enabled(PathBuf::from(&s))),
        Some(Value::Bool(false)) => Ok(ConfigOption::Disabled),
        _ => Err(serde::de::Error::custom(
            "Invalid type for configuration option",
        )),
    }
}

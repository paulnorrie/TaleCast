use crate::config::{Config, GlobalConfig, PodcastConfig};
use crate::utils::current_unix;
use anyhow::Result;
use clap::Parser;
use config::DownloadMode;
use futures_util::StreamExt;
use id3::TagLike;
use indicatif::MultiProgress;
use indicatif::{ProgressBar, ProgressStyle};
use quickxml_to_serde::{xml_string_to_json, Config as XmlConfig};
use reqwest::Client;
use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

mod config;
mod opml;
mod tags;
mod utils;

pub type Unix = std::time::Duration;
pub const APPNAME: &'static str = "cringecast";

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long, value_name = "FILE")]
    import: Option<PathBuf>,
    #[arg(short, long, value_name = "FILE")]
    export: Option<PathBuf>,
    #[arg(short, long)]
    print: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let should_sync = args.import.is_none() && args.export.is_none();

    if let Some(path) = args.import {
        crate::opml::import(&path)?;
    }

    if let Some(path) = args.export {
        crate::opml::export(&path)?;
    }

    if !should_sync {
        return Ok(());
    }

    eprintln!("Checking for new episodes...");
    let mp = MultiProgress::new();
    let mut futures = vec![];

    let global_config = GlobalConfig::load()?;
    let mut podcasts = Podcast::load_all(&global_config)?;
    podcasts.sort_by_key(|pod| pod.name.clone());

    // Longest podcast name is used for formatting.
    let Some(longest_name) = podcasts
        .iter()
        .map(|podcast| podcast.name.chars().count())
        .max()
    else {
        eprintln!("no podcasts configured");
        std::process::exit(1);
    };

    for podcast in podcasts {
        let pb = mp.add(ProgressBar::new_spinner());
        pb.set_style(ProgressStyle::default_spinner().template("{spinner:.green}  {msg}")?);
        pb.set_message(podcast.name.clone());
        pb.enable_steady_tick(std::time::Duration::from_millis(100));

        let future = tokio::task::spawn(async move { podcast.sync(pb, longest_name).await });

        futures.push(future);
    }

    let mut paths = vec![];
    for future in futures {
        let path_vec = future.await??;
        paths.extend(path_vec);
    }

    eprintln!("Syncing complete!");
    eprintln!("{} episodes downloaded.", paths.len());

    if args.print {
        for path in paths {
            println!("\"{}\"", path.to_str().unwrap());
        }
    }

    Ok(())
}

fn truncate_string(s: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut truncated = String::new();

    for c in s.chars() {
        let mut buf = [0; 4];
        let encoded_char = c.encode_utf8(&mut buf);
        let char_width = unicode_width::UnicodeWidthStr::width(encoded_char);
        if width + char_width > max_width {
            break;
        }
        truncated.push(c);
        width += char_width;
    }

    truncated
}

#[derive(Debug, Clone)]
struct Episode {
    title: String,
    url: String,
    guid: String,
    published: i64,
    index: usize,
    _inner: rss::Item,
    _xml: Arc<String>,
}

impl Episode {
    fn new(item: rss::Item, index: usize, xml: Arc<String>) -> Option<Self> {
        Some(Self {
            title: item.title()?.to_owned(),
            url: item.enclosure()?.url().to_owned(),
            guid: item.guid()?.value().to_string(),
            published: chrono::DateTime::parse_from_rfc2822(item.pub_date()?)
                .ok()?
                .timestamp(),
            index,
            _inner: item,
            _xml: xml,
        })
    }

    async fn download(&self, folder: &Path, pb: &ProgressBar) -> Result<PathBuf> {
        let response = Client::new().get(&self.url).send().await?;
        let total_size = response.content_length().unwrap_or(0);

        pb.set_length(total_size);

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|ct| ct.to_str().ok())
            .unwrap_or("application/octet-stream");

        let extensions = mime_guess::get_mime_extensions_str(&content_type).unwrap();

        let ext = if extensions.contains(&"mp3") {
            "mp3"
        } else {
            extensions.first().unwrap()
        };

        let path = {
            let file_name = self.title.replace(" ", "_") + "." + ext;
            folder.join(file_name)
        };

        let mut file = std::fs::File::create(&path)?;
        let mut downloaded: u64 = 0;
        let mut stream = response.bytes_stream();

        while let Some(item) = stream.next().await {
            let chunk = item?;
            file.write_all(&chunk)?;
            let new = std::cmp::min(downloaded + (chunk.len() as u64), total_size);
            pb.set_position(new);
            downloaded = new;
        }

        Ok(path)
    }
}

#[derive(Debug)]
struct Podcast {
    name: String,
    config: Config,
    downloaded: DownloadedEpisodes,
}

impl Podcast {
    fn load_all(global_config: &GlobalConfig) -> Result<Vec<Self>> {
        let configs: HashMap<String, PodcastConfig> = {
            let path = crate::utils::podcasts_toml()?;
            if !path.exists() {
                eprintln!("You need to create a 'podcasts.toml' file to get started");
                std::process::exit(1);
            }
            let config_str = std::fs::read_to_string(path)?;
            toml::from_str(&config_str)?
        };

        let mut podcasts = vec![];
        for (name, config) in configs {
            let config = Config::new(&global_config, config);
            let downloaded = DownloadedEpisodes::load(&name, &config)?;

            podcasts.push(Self {
                name,
                config,
                downloaded,
            });
        }

        Ok(podcasts)
    }

    async fn load_episodes(&self) -> Result<(rss::Channel, Vec<Episode>)> {
        let response = reqwest::Client::new()
            .get(&self.config.url)
            .header(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0",
            )
            .send()
            .await?;

        if response.status().is_success() {
            let xml = response.text().await?;
            let arced = Arc::new(xml.clone());

            let data = xml.as_bytes();

            let channel = rss::Channel::read_from(&data[..])?;
            let mut items = rss::Channel::read_from(&data[..])?.into_items();
            items.sort_by_key(|item| {
                chrono::DateTime::parse_from_rfc2822(item.pub_date().unwrap_or_default())
                    .map(|x| x.timestamp())
                    .unwrap_or_default()
            });

            Ok((
                channel,
                items
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, item)| Episode::new(item, index, arced.clone()))
                    .collect(),
            ))
        } else {
            Err(anyhow::anyhow!(
                "Failed to download RSS feed: HTTP {}",
                response.status()
            ))
        }
    }

    fn download_folder(&self) -> Result<PathBuf> {
        let destination_folder = self.config.download_path.join(&self.name);
        std::fs::create_dir_all(&destination_folder)?;
        Ok(destination_folder)
    }

    fn should_download(&self, episode: &Episode, latest_episode: usize) -> bool {
        if self.downloaded.contains_episode(episode) {
            return false;
        };

        match &self.config.mode {
            DownloadMode::Backlog { start, interval } => {
                let days_passed = (current_unix() - start.as_secs() as i64) / 86400;
                let current_backlog_index = days_passed / interval;

                current_backlog_index >= episode.index as i64
            }

            DownloadMode::Standard {
                max_days,
                max_episodes,
                earliest_date,
            } => {
                if max_days.is_some_and(|max_days| {
                    (current_unix() - episode.published) > max_days as i64 * 86400
                }) {
                    false
                } else if max_episodes.is_some_and(|max_episodes| {
                    (latest_episode - max_episodes as usize) > episode.index
                }) {
                    false
                } else if earliest_date.clone().is_some_and(|date| {
                    chrono::DateTime::parse_from_rfc3339(&date)
                        .unwrap()
                        .timestamp()
                        > episode.published
                }) {
                    false
                } else {
                    true
                }
            }
        }
    }

    fn mark_downloaded(&self, episode: &Episode) -> Result<()> {
        DownloadedEpisodes::append(&self.name, &self.config, &episode)?;
        Ok(())
    }

    async fn sync(&self, pb: ProgressBar, longest_podcast_name: usize) -> Result<Vec<PathBuf>> {
        let (channel, mut episodes) = self.load_episodes().await?;
        let episode_qty = episodes.len();

        episodes = episodes
            .into_iter()
            .filter(|episode| self.should_download(episode, episode_qty))
            .collect();

        // In backlog mode it makes more sense to download earliest episode first.
        // in standard mode, the most recent episodes are seen as more relevant.
        match self.config.mode {
            DownloadMode::Backlog { .. } => {
                episodes.sort_by_key(|ep| ep.index);
            }
            DownloadMode::Standard { .. } => {
                episodes.sort_by_key(|ep| ep.index);
                episodes.reverse();
            }
        }

        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} {msg} {bar:15.cyan/blue} {bytes}/{total_bytes}")?,
        );

        let download_folder = self.download_folder()?;
        let mut file_paths = vec![];
        for (index, episode) in episodes.iter().enumerate() {
            let fitted_episode_title = {
                let title_length = 30;
                let padded = &format!("{:<width$}", &episode.title, width = title_length);
                truncate_string(padded, title_length)
            };

            let msg = format!(
                "{:<podcast_width$} {}/{} {} ",
                &self.name,
                index + 1,
                episodes.len(),
                &fitted_episode_title,
                podcast_width = longest_podcast_name + 3
            );

            pb.set_message(msg);
            pb.set_position(0);

            let file_path = episode.download(&download_folder, &pb).await?;

            self.mark_downloaded(&episode)?;

            let mp3_tags = if file_path.extension().unwrap() == "mp3" {
                let mp3_tags = crate::tags::set_mp3_tags(
                    channel.clone(),
                    &episode,
                    &file_path,
                    &self.config.custom_tags,
                )
                .await?;
                Some(mp3_tags)
            } else {
                None
            };

            let file_path = rename_file(&file_path, &self.config, mp3_tags, episode);
            file_paths.push(file_path.clone());

            if let Some(script_path) = &self.config.download_hook {
                std::process::Command::new(script_path)
                    .arg(&file_path)
                    .output()?;
            }
        }

        pb.set_style(ProgressStyle::default_bar().template("{msg}")?);
        pb.finish_with_message(format!("✅ {}", &self.name));

        Ok(file_paths)
    }
}

/// Keeps track of which episodes have already been downloaded.
#[derive(Debug, Default)]
struct DownloadedEpisodes(HashMap<String, Unix>);

impl DownloadedEpisodes {
    fn contains_episode(&self, episode: &Episode) -> bool {
        self.0.contains_key(&episode.guid)
    }

    fn load(name: &str, config: &Config) -> Result<Self> {
        let path = Self::file_path(config, name);

        let s = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            e @ Err(_) => e?,
        };

        let mut hashmap: HashMap<String, Unix> = HashMap::new();

        for line in s.trim().lines() {
            let mut parts = line.split_whitespace();
            if let (Some(id), Some(timestamp_str)) = (parts.next(), parts.next()) {
                let id = id.to_string();
                let timestamp = timestamp_str
                    .parse::<i64>()
                    .expect("Timestamp should be a valid i64");
                let timestamp = std::time::Duration::from_secs(timestamp as u64);

                hashmap.insert(id, timestamp);
            }
        }

        Ok(Self(hashmap))
    }

    fn append(name: &str, config: &Config, episode: &Episode) -> Result<()> {
        let path = Self::file_path(config, name);

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?;

        writeln!(
            file,
            "{} {} \"{}\"",
            &episode.guid,
            current_unix(),
            &episode.title
        )?;
        Ok(())
    }

    fn file_path(config: &Config, pod_name: &str) -> PathBuf {
        config.download_path.join(pod_name).join(".downloaded")
    }
}

fn rename_file(file: &Path, config: &Config, tags: Option<id3::Tag>, episode: &Episode) -> PathBuf {
    let text = config.name_pattern.clone();
    let re = regex::Regex::new(r"\{([^\}]+)\}").unwrap();

    let mut result = String::new();
    let mut last_end = 0;

    use chrono::TimeZone;
    let datetime = chrono::Utc.timestamp_opt(episode.published, 0).unwrap();

    for cap in re.captures_iter(&text) {
        let match_range = cap.get(0).unwrap().range();
        let key = &cap[1];

        result.push_str(&text[last_end..match_range.start]);

        let replacement = match key {
            date if date.starts_with("pubdate::") => {
                let (_, format) = date.split_once("::").unwrap();
                datetime.format(format).to_string()
            }
            id3 if id3.starts_with("id3::") => {
                let (_, tag) = id3.split_once(":").unwrap();
                if let Some(ref tags) = tags {
                    tags.get(tag)
                        .map(|x| x.content())
                        .map(|c| c.to_string())
                        .unwrap_or(format!("<<invalid id3 tag>>"))
                } else {
                    String::new()
                }
            }
            rss if rss.starts_with("rss::episode::") => {
                let (_, key) = rss.split_once("episode::").unwrap();
                let xml = &episode._xml;
                let value = get_episode_xml(&episode.guid, xml);
                value.get(key).unwrap().as_str().unwrap().to_owned()
            }
            rss if rss.starts_with("rss::channel::") => {
                let (_, key) = rss.split_once("channel::").unwrap();
                let xml = &episode._xml;

                let conf = XmlConfig::new_with_defaults();
                let json = xml_string_to_json(xml.to_string(), &conf).unwrap();
                json.get("rss")
                    .unwrap()
                    .get("channel")
                    .unwrap()
                    .get(key)
                    .unwrap()
                    .to_string()
            }

            _ => "<<unknown tag>>".to_string(),
        };

        result.push_str(&replacement);

        last_end = match_range.end;
    }

    result.push_str(&text[last_end..]);

    let new_name = match file.extension() {
        Some(extension) => {
            let mut new_path = file.with_file_name(result);
            new_path.set_extension(extension);
            new_path
        }
        None => file.with_file_name(result),
    };

    std::fs::rename(file, &new_name).unwrap();
    new_name
}

fn get_episode_xml(id: &str, xml: &str) -> serde_json::Value {
    let conf = XmlConfig::new_with_defaults();
    let json = xml_string_to_json(xml.to_owned(), &conf).unwrap();

    let rss = json.get("rss").unwrap();
    let channel = rss.get("channel").unwrap();
    let items = channel.get("item").unwrap().as_array().unwrap();

    for item in items {
        if &item
            .get("guid")
            .unwrap()
            .as_object()
            .unwrap()
            .get("#text")
            .unwrap()
            .as_str()
            .unwrap()
            == &id
        {
            return item.clone();
        }
    }

    panic!("ep not found lol");
}

#[cfg(test)]
mod tests {
    use super::*;
}

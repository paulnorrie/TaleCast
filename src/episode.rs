use futures_util::StreamExt;
use indicatif::ProgressBar;
use reqwest::Client;
use std::io::Write as IoWrite;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Episode<'a> {
    pub title: &'a str,
    pub url: &'a str,
    pub guid: &'a str,
    pub published: i64,
    pub index: usize,
    pub inner: &'a rss::Item,
    pub raw: &'a serde_json::Map<String, serde_json::Value>,
}

impl<'a> Episode<'a> {
    pub fn new(
        item: &'a rss::Item,
        index: usize,
        raw: &'a serde_json::Map<String, serde_json::Value>,
    ) -> Option<Self> {
        Some(Self {
            title: item.title.as_ref().unwrap(),
            url: item.enclosure().unwrap().url(),
            guid: item.guid().unwrap().value(),
            published: chrono::DateTime::parse_from_rfc2822(item.pub_date().unwrap())
                .ok()
                .unwrap()
                .timestamp(),
            index,
            inner: item,
            raw,
        })
    }

    pub fn get_text_value(&self, tag: &str) -> Option<&str> {
        self.raw.get(tag).unwrap().as_str()
    }

    pub async fn download(&self, folder: &Path, pb: Option<&ProgressBar>) -> PathBuf {
        let partial_path = {
            let file_name = format!("{}.partial", self.guid);
            folder.join(file_name)
        };

        let mut downloaded: u64 = 0;

        let mut file = if partial_path.exists() {
            use std::io::Seek;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&partial_path)
                .unwrap();
            downloaded = file.seek(std::io::SeekFrom::End(0)).unwrap();
            file
        } else {
            std::fs::File::create(&partial_path).unwrap()
        };

        let mut req_builder = Client::new().get(self.url);

        if downloaded > 0 {
            let range_header_value = format!("bytes={}-", downloaded);
            req_builder = req_builder.header(reqwest::header::RANGE, range_header_value);
        }

        let response = req_builder.send().await.unwrap();
        let total_size = response.content_length().unwrap_or(0);

        let ext = {
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|ct| ct.to_str().ok())
                .unwrap_or("application/octet-stream");

            let extensions = mime_guess::get_mime_extensions_str(&content_type).unwrap();

            match extensions.contains(&"mp3") {
                true => "mp3",
                false => extensions.first().expect("extension not found."),
            }
        };

        if let Some(pb) = pb {
            pb.set_length(total_size);
            pb.set_position(downloaded);
        }

        let mut stream = response.bytes_stream();

        while let Some(item) = stream.next().await {
            let chunk = item.unwrap();
            file.write_all(&chunk).unwrap();
            downloaded = std::cmp::min(downloaded + (chunk.len() as u64), total_size);

            if let Some(pb) = pb {
                pb.set_position(downloaded);
            }
        }

        let path = {
            let mut path = partial_path.clone();
            path.set_extension(ext);
            path
        };

        std::fs::rename(partial_path, &path).unwrap();

        path
    }
}

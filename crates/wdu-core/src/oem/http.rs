//! HTTPS client restricted to the OEM download hosts, with a conditional-GET cache.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use serde::{Deserialize, Serialize};

use super::sha256_hex;

/// Hosts catalogs and packages may be downloaded from, redirects included.
pub const ALLOWED_HOSTS: &[&str] = &[
    "downloads.dell.com",
    "hpia.hpcloud.hp.com",
    "ftp.hp.com",
    "ftp.ext.hp.com",
    "download.lenovo.com",
];

fn is_allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https" && ALLOWED_HOSTS.contains(&url.host_str().unwrap_or_default())
}

fn check_url(url: &str) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid URL {url}"))?;
    if !is_allowed(&parsed) {
        bail!("refusing to download from non allow-listed URL {url}");
    }
    Ok(parsed)
}

/// Blocking HTTPS client restricted to [`ALLOWED_HOSTS`].
///
/// With a cache directory, [`Http::get`] revalidates earlier responses with
/// `If-None-Match` / `If-Modified-Since` and serves them on `304`, or when the
/// server cannot be reached.
#[derive(Clone)]
pub struct Http {
    client: reqwest::blocking::Client,
    cache_dir: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct CacheMeta {
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
}

impl Http {
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("wdu/", env!("CARGO_PKG_VERSION")))
            .https_only(true)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    attempt.error("too many redirects")
                } else if is_allowed(attempt.url()) {
                    attempt.follow()
                } else {
                    let msg = format!("redirect to non allow-listed URL {}", attempt.url());
                    attempt.error(msg)
                }
            }))
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Http {
            client,
            cache_dir: None,
        })
    }

    pub fn with_cache(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }

    /// Downloads a (small) document such as a catalog, through the cache when enabled.
    pub fn get(&self, url: &str) -> Result<Vec<u8>> {
        let parsed = check_url(url)?;
        let Some(dir) = &self.cache_dir else {
            tracing::debug!(url, "GET");
            let response = self
                .client
                .get(parsed)
                .send()
                .with_context(|| format!("GET {url} failed"))?
                .error_for_status()?;
            return Ok(response.bytes()?.to_vec());
        };

        let key = &sha256_hex(url.as_bytes())[..32];
        let body_path = dir.join(format!("{key}.bin"));
        let meta_path = dir.join(format!("{key}.json"));
        let cached = fs::read(&meta_path)
            .ok()
            .and_then(|m| serde_json::from_slice::<CacheMeta>(&m).ok())
            .filter(|m| m.url == url && body_path.exists());

        let mut request = self.client.get(parsed);
        if let Some(meta) = &cached {
            if let Some(etag) = &meta.etag {
                request = request.header(IF_NONE_MATCH, etag);
            }
            if let Some(date) = &meta.last_modified {
                request = request.header(IF_MODIFIED_SINCE, date);
            }
        }
        tracing::debug!(url, revalidate = cached.is_some(), "GET");
        let response = match request.send() {
            Ok(response) => response,
            Err(err) if cached.is_some() => {
                tracing::warn!(url, "using cached copy, server unreachable: {err}");
                return Ok(fs::read(&body_path)?);
            }
            Err(err) => return Err(err).with_context(|| format!("GET {url} failed")),
        };
        if response.status() == StatusCode::NOT_MODIFIED && cached.is_some() {
            tracing::debug!(url, "not modified, cache hit");
            return Ok(fs::read(&body_path)?);
        }
        let response = response.error_for_status()?;
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let meta = CacheMeta {
            url: url.to_string(),
            etag: header(ETAG),
            last_modified: header(LAST_MODIFIED),
        };
        let body = response.bytes()?.to_vec();
        if (meta.etag.is_some() || meta.last_modified.is_some())
            && let Err(err) = store(dir, &body_path, &meta_path, &body, &meta)
        {
            tracing::warn!(url, "cannot cache response: {err:#}");
        }
        Ok(body)
    }

    /// Streams a (large) package to `path`, bypassing the cache.
    pub fn download_to(&self, url: &str, path: &Path) -> Result<u64> {
        let parsed = check_url(url)?;
        tracing::info!(url, path = %path.display(), "downloading");
        let mut response = self
            .client
            .get(parsed)
            .send()
            .with_context(|| format!("GET {url} failed"))?
            .error_for_status()?;
        let mut file =
            fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
        Ok(response.copy_to(&mut file)?)
    }
}

fn store(
    dir: &Path,
    body_path: &Path,
    meta_path: &Path,
    body: &[u8],
    meta: &CacheMeta,
) -> Result<()> {
    fs::create_dir_all(dir)?;
    // Body first: a meta file never points at a missing or partial body.
    let tmp = body_path.with_extension("tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, body_path)?;
    fs::write(meta_path, serde_json::to_vec(meta)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_foreign_hosts() {
        let http = Http::new().unwrap();
        assert!(http.get("https://example.com/catalog.cab").is_err());
        assert!(
            http.get("http://downloads.dell.com/catalog/CatalogIndexPC.cab")
                .is_err()
        );
        let dir = std::env::temp_dir().join("wdu-test-download");
        assert!(http.download_to("https://example.com/x.exe", &dir).is_err());
    }
}

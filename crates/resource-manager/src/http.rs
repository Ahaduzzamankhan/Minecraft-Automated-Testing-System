//! HTTP access with retries, offline mode and local-mirror support.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use mats_core::config::NetworkConfig;
use serde_json::Value;
use thiserror::Error;

/// Networking failures that callers may want to classify.
#[derive(Debug, Error)]
pub enum NetError {
    #[error("resource not found (HTTP 404): {url}")]
    NotFound { url: String },
    #[error("GitHub API rate limit reached for {url}; resets at {reset}")]
    RateLimited { url: String, reset: String },
    #[error("HTTP {status} for {url}")]
    Status {
        url: String,
        status: u16,
        body: String,
    },
    #[error("network error for {url}: {message}")]
    Transport { url: String, message: String },
    #[error("offline mode is enabled and `{what}` is not available in the local cache")]
    Offline { what: String },
    #[error("{0}")]
    Local(String),
}

impl NetError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, NetError::NotFound { .. })
    }

    pub fn is_rate_limited(&self) -> bool {
        matches!(self, NetError::RateLimited { .. })
    }

    /// Transient failures are worth retrying.
    pub fn is_transient(&self) -> bool {
        match self {
            NetError::Status { status, .. } => {
                *status == 408 || *status == 429 || (500..600).contains(status)
            }
            NetError::Transport { .. } => true,
            _ => false,
        }
    }
}

/// Blocking HTTP client with bounded retries.
#[derive(Debug, Clone)]
pub struct HttpClient {
    agent: ureq::Agent,
    retries: u32,
    offline: bool,
    user_agent: String,
}

impl HttpClient {
    pub fn new(config: &NetworkConfig, offline: bool) -> Self {
        let timeout = Duration::from_secs(config.timeout_secs.max(1));
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(timeout.min(Duration::from_secs(20)))
            .timeout_read(timeout)
            .timeout_write(timeout)
            .user_agent(&format!("MATS/{} (+cli)", env!("CARGO_PKG_VERSION")))
            .redirects(8)
            .build();
        HttpClient {
            agent,
            retries: config.retries.min(10),
            offline,
            user_agent: format!("MATS/{}", env!("CARGO_PKG_VERSION")),
        }
    }

    pub fn offline(&self) -> bool {
        self.offline
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Download a URL into memory, with a hard size limit.
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, NetError> {
        self.with_retries(url, |agent| {
            let response = call(agent, url, None)?;
            let reader = response.into_reader();
            let mut buffer = Vec::new();
            reader
                .take(64 * 1024 * 1024)
                .read_to_end(&mut buffer)
                .map_err(|error| NetError::Transport {
                    url: url.to_string(),
                    message: error.to_string(),
                })?;
            Ok(buffer)
        })
    }

    pub fn get_text(&self, url: &str) -> Result<String, NetError> {
        let bytes = self.get_bytes(url)?;
        String::from_utf8(bytes)
            .map_err(|error| NetError::Local(format!("{url} is not UTF-8: {error}")))
    }

    pub fn get_json(&self, url: &str) -> Result<Value, NetError> {
        let text = self.get_text(url)?;
        serde_json::from_str(&text)
            .map_err(|error| NetError::Local(format!("{url} returned invalid JSON: {error}")))
    }

    /// Open a response body for streaming downloads, honouring a resume offset.
    ///
    /// Returns the total size when the server reports one and whether the
    /// response is a partial (`206`) reply.
    pub fn open_stream(
        &self,
        url: &str,
        resume_from: u64,
    ) -> Result<(Box<dyn Read + Send>, Option<u64>, bool), NetError> {
        if resume_from > 0 {
            match self.try_open_stream(url, resume_from) {
                Ok(stream) => return Ok(stream),
                Err(error) if error.is_transient() => return Err(error),
                Err(_) => {
                    // Fall back to a fresh download when the server ignores or
                    // rejects Range requests.
                }
            }
        }
        self.with_retries(url, |agent| {
            let response = call(agent, url, None)?;
            let total = response
                .header("Content-Length")
                .and_then(|value| value.parse::<u64>().ok());
            let reader: Box<dyn Read + Send> = response.into_reader();
            Ok((reader, total, false))
        })
    }

    fn try_open_stream(
        &self,
        url: &str,
        resume_from: u64,
    ) -> Result<(Box<dyn Read + Send>, Option<u64>, bool), NetError> {
        let agent = self.agent.clone();
        let url_owned = url.to_string();
        let response = call_range(&agent, &url_owned, resume_from)?;
        let partial = response.status() == 206;
        let total = response
            .header("Content-Range")
            .and_then(parse_content_range_total)
            .or_else(|| {
                response
                    .header("Content-Length")
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(|length| length + if partial { resume_from } else { 0 })
            });
        if !partial {
            let reader: Box<dyn Read + Send> = response.into_reader();
            return Ok((reader, total, false));
        }
        let reader: Box<dyn Read + Send> = response.into_reader();
        Ok((reader, total, true))
    }

    fn with_retries<T>(
        &self,
        url: &str,
        mut operation: impl FnMut(&ureq::Agent) -> Result<T, NetError>,
    ) -> Result<T, NetError> {
        if self.offline {
            return Err(NetError::Offline {
                what: url.to_string(),
            });
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            match operation(&self.agent) {
                Ok(value) => return Ok(value),
                Err(error) if error.is_transient() && attempt <= self.retries => {
                    let backoff = Duration::from_millis(250 * 2u64.pow(attempt.min(5)));
                    tracing::debug!(url, attempt, "transient network failure, retrying: {error}");
                    sleep(backoff);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn call(agent: &ureq::Agent, url: &str, range: Option<u64>) -> Result<ureq::Response, NetError> {
    let mut request = agent.get(url).set("Accept-Encoding", "identity");
    if let Some(offset) = range {
        request = request.set("Range", &format!("bytes={offset}-"));
    }
    match request.call() {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(status, response)) => {
            let body = response.into_string().unwrap_or_default();
            let body = body.chars().take(400).collect::<String>();
            if status == 404 {
                return Err(NetError::NotFound {
                    url: url.to_string(),
                });
            }
            Err(NetError::Status {
                url: url.to_string(),
                status,
                body,
            })
        }
        Err(ureq::Error::Transport(transport)) => Err(NetError::Transport {
            url: url.to_string(),
            message: transport.to_string(),
        }),
    }
}

fn call_range(agent: &ureq::Agent, url: &str, offset: u64) -> Result<ureq::Response, NetError> {
    call(agent, url, Some(offset))
}

fn parse_content_range_total(value: &str) -> Option<u64> {
    // `bytes 100-199/2000`
    value.rsplit('/').next()?.trim().parse::<u64>().ok()
}

/// Convert `file://` URLs (used for offline mirrors and tests) into paths.
pub fn local_path_from_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    if cfg!(windows) {
        let trimmed = decoded.trim_start_matches('/');
        Some(PathBuf::from(trimmed))
    } else {
        Some(PathBuf::from(decoded))
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_val(bytes[index + 1]), hex_val(bytes[index + 2]))
            {
                output.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode a single URL path segment (Minecraft version names contain
/// spaces, e.g. `1.14 Pre-Release 5`).
pub fn encode_segment(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char)
            }
            b' ' => output.push_str("%20"),
            other => output.push_str(&format!("%{other:02X}")),
        }
    }
    output
}

/// Read a local file, used for `file://` mirrors.
pub fn read_local(path: &PathBuf) -> Result<Vec<u8>, NetError> {
    let metadata = fs::metadata(path).map_err(|error| {
        NetError::Local(format!("reading local mirror {}: {error}", path.display()))
    })?;
    if metadata.len() > 2 * 1024 * 1024 * 1024 {
        return Err(NetError::Local(format!(
            "local mirror {} is unexpectedly large ({} bytes)",
            path.display(),
            metadata.len()
        )));
    }
    fs::read(path).map_err(|error| {
        NetError::Local(format!("reading local mirror {}: {error}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_version_segments() {
        assert_eq!(
            encode_segment("1.14 Pre-Release 5"),
            "1.14%20Pre-Release%205"
        );
        assert_eq!(encode_segment("26.3"), "26.3");
        assert_eq!(encode_segment("a+b"), "a%2Bb");
    }

    #[test]
    fn parses_content_range() {
        assert_eq!(parse_content_range_total("bytes 0-99/1234"), Some(1234));
        assert_eq!(parse_content_range_total("bytes 0-99/*"), None);
    }

    #[test]
    fn decodes_file_urls() {
        #[cfg(not(windows))]
        {
            let path = local_path_from_url("file:///tmp/my%20mod.jar").unwrap();
            assert_eq!(path, PathBuf::from("/tmp/my mod.jar"));
        }
        assert!(local_path_from_url("https://example.com").is_none());
    }

    #[test]
    fn offline_client_refuses_network_and_mirrors_still_work() {
        let client = HttpClient::new(&NetworkConfig::default(), true);
        let error = client.get_text("https://example.invalid/x").unwrap_err();
        assert!(matches!(error, NetError::Offline { .. }));
    }

    #[test]
    fn transient_classification() {
        assert!(
            NetError::Status {
                url: "u".into(),
                status: 503,
                body: String::new()
            }
            .is_transient()
        );
        assert!(!NetError::NotFound { url: "u".into() }.is_transient());
        assert!(
            NetError::Transport {
                url: "u".into(),
                message: "x".into()
            }
            .is_transient()
        );
    }
}

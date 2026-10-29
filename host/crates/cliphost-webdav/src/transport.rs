//! A deliberately small WebDAV object transport. It never treats an empty or
//! missing remote as permission to initialise a vault.

use std::fmt;
use std::io::Read;
use std::net::IpAddr;
use std::sync::Barrier;
use std::time::Duration;

use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, ETAG, IF_MATCH, IF_NONE_MATCH,
};
use reqwest::{Method, StatusCode, Url};

use crate::{RemoteFile, RemoteStore, SyncError};

// Accommodate the engine's largest encrypted metadata object, including its
// authentication envelope. Blob chunks themselves are considerably smaller.
const DEFAULT_OBJECT_LIMIT: usize = crate::MAX_ENCRYPTED_OBJECT_BYTES;
const MAX_OBJECT_LIMIT: usize = 64 * 1024 * 1024;
const COLLECTION_RESPONSE_LIMIT: usize = 64 * 1024;

/// Credentials are intentionally excluded from `Debug`. The endpoint is also
/// omitted because a user's dedicated DAV path may itself be sensitive.
pub struct TransportConfig {
    endpoint: String,
    username: String,
    password: String,
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_response_bytes: usize,
    /// Only permits literal loopback IPs over HTTP; never hostnames or LAN IPs.
    pub allow_loopback_http: bool,
}

impl TransportConfig {
    pub fn new(
        endpoint: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            username: username.into(),
            password: password.into(),
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_response_bytes: DEFAULT_OBJECT_LIMIT,
            allow_loopback_http: false,
        }
    }
}

impl fmt::Debug for TransportConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransportConfig")
            .field("endpoint", &"[redacted]")
            .field("credentials", &"[redacted]")
            .field("timeout", &self.timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("allow_loopback_http", &self.allow_loopback_http)
            .finish()
    }
}

pub struct WebDavStore {
    client: Client,
    root: Url,
    username: String,
    password: String,
    max_response_bytes: usize,
}

impl fmt::Debug for WebDavStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebDavStore")
            .field("endpoint", &"[redacted]")
            .field("credentials", &"[redacted]")
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct ProbeReport {
    pub conditional_create: bool,
    pub conditional_replace: bool,
    pub strong_etags: bool,
    pub full_readback: bool,
}

impl WebDavStore {
    pub fn new(config: TransportConfig) -> Result<Self, SyncError> {
        let root = validate_root(&config.endpoint, config.allow_loopback_http)?;
        if config.timeout.is_zero()
            || config.connect_timeout.is_zero()
            || config.timeout > Duration::from_secs(300)
            || config.connect_timeout > config.timeout
            || config.max_response_bytes == 0
            || config.max_response_bytes > MAX_OBJECT_LIMIT
            || config.username.contains(':')
            || config.username.chars().any(char::is_control)
        {
            return Err(SyncError::Transport("invalid_configuration"));
        }
        // Match the host/updater provider, including stand-alone probes which
        // run before the host's normal LAN TLS initialization.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .https_only(root.scheme() == "https")
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
            .pool_max_idle_per_host(2)
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .user_agent("ClipHost-WebDAV/1")
            .build()
            .map_err(|_| SyncError::Transport("client_initialization_failed"))?;
        Ok(Self {
            client,
            root,
            username: config.username,
            password: config.password,
            max_response_bytes: config.max_response_bytes,
        })
    }

    fn object_url(&self, path: &str) -> Result<Url, SyncError> {
        validate_relative_path(path)?;
        let mut url = self.root.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| SyncError::Transport("invalid_endpoint"))?;
            segments.pop_if_empty();
            for component in path.split('/') {
                segments.push(component);
            }
        }
        Ok(url)
    }

    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, SyncError> {
        let url = self.object_url(path)?;
        Ok(self.request_url(method, url))
    }

    fn request_url(&self, method: Method, url: Url) -> RequestBuilder {
        self.client
            .request(method, url)
            .basic_auth(&self.username, Some(&self.password))
            .header(ACCEPT_ENCODING, "identity")
    }

    fn send(&self, request: RequestBuilder) -> Result<Response, SyncError> {
        request.send().map_err(|error| {
            // reqwest errors may include the complete URL. Never format them.
            SyncError::Transport(if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connection_failed"
            } else if error.is_builder() {
                "invalid_request"
            } else {
                "request_failed"
            })
        })
    }

    fn make_collection(&self, path: &str) -> Result<(), SyncError> {
        self.make_collection_url(self.object_url(path)?)
    }

    fn make_collection_url(&self, url: Url) -> Result<(), SyncError> {
        let url = collection_url(url)?;
        let method =
            Method::from_bytes(b"MKCOL").map_err(|_| SyncError::Transport("invalid_method"))?;
        let response = self.send(self.request_url(method, url.clone()))?;
        let status = response.status();
        drop(response);
        match status {
            StatusCode::CREATED | StatusCode::METHOD_NOT_ALLOWED => {
                // 405 can also indicate a file or unsupported MKCOL.
                if self.collection_exists(url)? {
                    Ok(())
                } else {
                    Err(SyncError::Transport("not_a_collection"))
                }
            }
            status => Err(status_error(status)),
        }
    }

    fn collection_exists(&self, url: Url) -> Result<bool, SyncError> {
        let url = collection_url(url)?;
        let method =
            Method::from_bytes(b"PROPFIND").map_err(|_| SyncError::Transport("invalid_method"))?;
        let response = self.send(self.request_url(method, url)
            .header("Depth", "0")
            .header(CONTENT_TYPE, "application/xml; charset=utf-8")
            .body("<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:resourcetype/></d:prop></d:propfind>"))?;
        match response.status() {
            StatusCode::MULTI_STATUS => {
                let bytes = read_bounded(response, COLLECTION_RESPONSE_LIMIT)?;
                if dav_response_is_collection(&bytes)? {
                    Ok(true)
                } else {
                    Err(SyncError::Transport("not_a_collection"))
                }
            }
            StatusCode::NOT_FOUND => Ok(false),
            status => Err(status_error(status)),
        }
    }

    /// Explicit setup can create only the selected URL's final collection. Its
    /// parent must already exist. Reads and ordinary writes never call this.
    pub fn ensure_root(&self) -> Result<(), SyncError> {
        if self.root.path().trim_matches('/').is_empty() {
            return Err(SyncError::Transport("dedicated_root_required"));
        }
        if self.collection_exists(self.root.clone())? {
            return Ok(());
        }
        self.make_collection_url(self.root.clone())
    }

    fn ensure_parents(&self, path: &str) -> Result<(), SyncError> {
        validate_relative_path(path)?;
        // Start below the configured root. Never MKCOL its ancestors or root.
        for (index, _) in path.match_indices('/') {
            self.make_collection(&path[..index])?;
        }
        Ok(())
    }

    fn read_file(&self, response: Response) -> Result<RemoteFile, SyncError> {
        let mut tags = response.headers().get_all(ETAG).iter();
        let etag = tags
            .next()
            .and_then(|header| header.to_str().ok())
            .filter(|tag| is_strong_etag(tag))
            .ok_or(SyncError::Transport("weak_or_invalid_etag"))?
            .to_owned();
        if tags.next().is_some() {
            return Err(SyncError::Transport("weak_or_invalid_etag"));
        }
        let bytes = read_bounded(response, self.max_response_bytes)?;
        Ok(RemoteFile { bytes, etag })
    }

    fn put(&self, path: &str, condition: (&str, &str), bytes: &[u8]) -> Result<bool, SyncError> {
        if bytes.len() > self.max_response_bytes {
            return Err(SyncError::Transport("request_too_large"));
        }
        let response = self.send(
            self.request(Method::PUT, path)?
                .header(condition.0, condition.1)
                .header(CONTENT_TYPE, "application/octet-stream")
                .body(bytes.to_owned()),
        )?;
        match response.status() {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => Ok(true),
            StatusCode::PRECONDITION_FAILED => Ok(false),
            status => Err(status_error(status)),
        }
    }

    fn delete_probe_collection(&self, path: &str) -> Result<(), SyncError> {
        // This helper is private and only called for a fresh random probe name.
        let response =
            self.send(self.request_url(Method::DELETE, collection_url(self.object_url(path)?)?))?;
        match response.status() {
            StatusCode::OK | StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(()),
            _ => Err(SyncError::Transport("probe_cleanup_failed")),
        }
    }

    /// Exercise actual competing conditional writes and verify every winning
    /// payload. Only a fresh random child collection is created and deleted.
    pub fn probe(&self) -> Result<ProbeReport, SyncError> {
        self.ensure_root()?;
        let directory = format!(".cliphost-probe-{}", uuid::Uuid::new_v4());
        let path = format!("{directory}/conditional.bin");
        let first = format!("create-a-{}", uuid::Uuid::new_v4()).into_bytes();
        let second = format!("create-b-{}", uuid::Uuid::new_v4()).into_bytes();
        let result = (|| {
            self.make_collection(&directory)?;
            let barrier = Barrier::new(2);
            let (left, right) = std::thread::scope(|scope| {
                let a = scope.spawn(|| {
                    barrier.wait();
                    self.create(&path, &first)
                });
                let b = scope.spawn(|| {
                    barrier.wait();
                    self.create(&path, &second)
                });
                (a.join(), b.join())
            });
            let a = left.map_err(|_| SyncError::Transport("probe_worker_failed"))??;
            let b = right.map_err(|_| SyncError::Transport("probe_worker_failed"))??;
            if a == b {
                return Err(SyncError::Transport("conditional_create_unsupported"));
            }
            let file = self
                .get(&path)?
                .ok_or(SyncError::Transport("probe_readback_failed"))?;
            let expected = if a { &first } else { &second };
            if file.bytes != *expected {
                return Err(SyncError::Transport("probe_readback_failed"));
            }

            let update_a = format!("replace-a-{}", uuid::Uuid::new_v4()).into_bytes();
            let update_b = format!("replace-b-{}", uuid::Uuid::new_v4()).into_bytes();
            let barrier = Barrier::new(2);
            let (left, right) = std::thread::scope(|scope| {
                let a = scope.spawn(|| {
                    barrier.wait();
                    self.replace(&path, &file.etag, &update_a)
                });
                let b = scope.spawn(|| {
                    barrier.wait();
                    self.replace(&path, &file.etag, &update_b)
                });
                (a.join(), b.join())
            });
            let a = left.map_err(|_| SyncError::Transport("probe_worker_failed"))??;
            let b = right.map_err(|_| SyncError::Transport("probe_worker_failed"))??;
            if a == b {
                return Err(SyncError::Transport("conditional_replace_unsupported"));
            }
            let updated = self
                .get(&path)?
                .ok_or(SyncError::Transport("probe_readback_failed"))?;
            let expected = if a { &update_a } else { &update_b };
            if updated.bytes != *expected {
                return Err(SyncError::Transport("probe_readback_failed"));
            }
            if updated.etag == file.etag {
                return Err(SyncError::Transport("etag_not_updated"));
            }
            if self.replace(&path, &file.etag, b"must-not-replace")? {
                return Err(SyncError::Transport("conditional_replace_unsupported"));
            }
            let final_file = self
                .get(&path)?
                .ok_or(SyncError::Transport("probe_readback_failed"))?;
            if final_file.bytes != updated.bytes || final_file.etag != updated.etag {
                return Err(SyncError::Transport("probe_readback_failed"));
            }
            Ok(ProbeReport {
                conditional_create: true,
                conditional_replace: true,
                strong_etags: true,
                full_readback: true,
            })
        })();
        let cleanup = self.delete_probe_collection(&directory);
        match result {
            Err(error) => Err(error),
            Ok(report) => cleanup.map(|()| report),
        }
    }
}

impl RemoteStore for WebDavStore {
    fn get(&self, path: &str) -> Result<Option<RemoteFile>, SyncError> {
        let response = self.send(self.request(Method::GET, path)?)?;
        match response.status() {
            StatusCode::OK => self.read_file(response).map(Some),
            StatusCode::NOT_FOUND => Ok(None),
            status => Err(status_error(status)),
        }
    }

    fn create(&self, path: &str, bytes: &[u8]) -> Result<bool, SyncError> {
        if bytes.len() > self.max_response_bytes {
            return Err(SyncError::Transport("request_too_large"));
        }
        self.ensure_parents(path)?;
        self.put(path, (IF_NONE_MATCH.as_str(), "*"), bytes)
    }

    fn replace(&self, path: &str, etag: &str, bytes: &[u8]) -> Result<bool, SyncError> {
        if !is_strong_etag(etag) {
            return Err(SyncError::Transport("weak_or_invalid_etag"));
        }
        self.put(path, (IF_MATCH.as_str(), etag), bytes)
    }
}

fn collection_url(mut url: Url) -> Result<Url, SyncError> {
    if !url.path().ends_with('/') {
        url.path_segments_mut()
            .map_err(|_| SyncError::Transport("invalid_endpoint"))?
            .push("");
    }
    Ok(url)
}

fn read_bounded(mut response: Response, limit: usize) -> Result<Vec<u8>, SyncError> {
    if response
        .headers()
        .get_all(CONTENT_ENCODING)
        .iter()
        .any(|encoding| encoding.as_bytes() != b"identity")
    {
        return Err(SyncError::Transport("unexpected_content_encoding"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(SyncError::Transport("response_too_large"));
    }
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            SyncError::Transport(
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) {
                    "timeout"
                } else {
                    "response_read_failed"
                },
            )
        })?;
    if bytes.len() > limit {
        return Err(SyncError::Transport("response_too_large"));
    }
    Ok(bytes)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DavNode {
    MultiStatus,
    Response,
    PropStat,
    Prop,
    ResourceType,
    Collection,
    Status,
    Other,
}

fn dav_node(namespace: &ResolveResult<'_>, name: &[u8]) -> DavNode {
    use DavNode::*;
    if !matches!(namespace, ResolveResult::Bound(ns) if ns.as_ref() == b"DAV:") {
        return Other;
    }
    match name {
        b"multistatus" => MultiStatus,
        b"response" => Response,
        b"propstat" => PropStat,
        b"prop" => Prop,
        b"resourcetype" => ResourceType,
        b"collection" => Collection,
        b"status" => Status,
        _ => Other,
    }
}

/// Only a DAV:resourcetype/DAV:collection in a successful DAV:propstat
/// qualifies. XML is size/depth bounded and DTDs are never accepted.
fn dav_response_is_collection(bytes: &[u8]) -> Result<bool, SyncError> {
    use DavNode::*;
    let invalid = || SyncError::Transport("invalid_collection_response");
    let mut reader = NsReader::from_reader(bytes);
    let mut stack = Vec::new();
    let mut saw_root = false;
    let mut responses = 0usize;
    let mut property_status = String::new();
    let mut resource_types = 0usize;
    let mut has_collection = false;
    let mut found = false;
    loop {
        let (namespace, event) = reader.read_resolved_event().map_err(|_| invalid())?;
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                let node = dav_node(&namespace, element.local_name().as_ref());
                if stack.len() >= 32 {
                    return Err(invalid());
                }
                if stack.is_empty() {
                    if saw_root || node != MultiStatus {
                        return Err(invalid());
                    }
                    saw_root = true;
                }
                if stack == [MultiStatus] && node == Response {
                    responses += 1;
                    if responses > 1 {
                        return Err(invalid());
                    }
                }
                if stack == [MultiStatus, Response] && node == PropStat {
                    property_status.clear();
                    resource_types = 0;
                    has_collection = false;
                }
                if stack == [MultiStatus, Response, PropStat, Prop] && node == ResourceType {
                    resource_types += 1;
                }
                if stack == [MultiStatus, Response, PropStat, Prop, ResourceType]
                    && node == Collection
                {
                    has_collection = true;
                }
                stack.push(node);
                if empty {
                    stack.pop();
                }
            }
            Event::End(element) => {
                let node = dav_node(&namespace, element.local_name().as_ref());
                if stack.last() != Some(&node) {
                    return Err(invalid());
                }
                if stack == [MultiStatus, Response, PropStat] {
                    let mut status = property_status.split_whitespace();
                    let success = status
                        .next()
                        .is_some_and(|value| value.starts_with("HTTP/"))
                        && status.next() == Some("200");
                    found |= success && resource_types == 1 && has_collection;
                }
                stack.pop();
            }
            Event::Text(text) if stack == [MultiStatus, Response, PropStat, Status] => {
                property_status.push_str(&text.decode().map_err(|_| invalid())?);
                if property_status.len() > 128 {
                    return Err(invalid());
                }
            }
            Event::DocType(_) => return Err(invalid()),
            Event::GeneralRef(_) if stack == [MultiStatus, Response, PropStat, Status] => {
                return Err(invalid())
            }
            Event::Eof => {
                if !saw_root || !stack.is_empty() {
                    return Err(invalid());
                }
                return Ok(responses == 1 && found);
            }
            _ => {}
        }
    }
}

fn is_strong_etag(value: &str) -> bool {
    let bytes = value.as_bytes();
    (2..=512).contains(&bytes.len())
        && bytes.first() == Some(&b'"')
        && bytes.last() == Some(&b'"')
        && bytes[1..bytes.len() - 1]
            .iter()
            .all(|byte| *byte == 0x21 || (0x23..=0x7e).contains(byte))
}

fn status_error(status: StatusCode) -> SyncError {
    SyncError::Transport(match status.as_u16() {
        401 | 403 => "authentication_failed",
        404 => "resource_not_found",
        409 => "missing_parent",
        412 => "precondition_failed",
        413 => "remote_size_limit",
        423 => "resource_locked",
        429 => "rate_limited",
        507 => "remote_storage_full",
        300..=399 => "redirect_rejected",
        500..=599 => "server_error",
        _ => "unexpected_status",
    })
}

fn validate_relative_path(path: &str) -> Result<(), SyncError> {
    if path.is_empty() || path.len() > 1024 || path.split('/').count() > 16 {
        return Err(SyncError::Transport("invalid_object_path"));
    }
    for component in path.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.len() > 128
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return Err(SyncError::Transport("invalid_object_path"));
        }
    }
    Ok(())
}

fn validate_root(raw: &str, allow_loopback_http: bool) -> Result<Url, SyncError> {
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) || raw.contains('\\') {
        return Err(SyncError::Transport("invalid_endpoint"));
    }
    let (_, authority_and_path) = raw
        .split_once("://")
        .ok_or(SyncError::Transport("invalid_endpoint"))?;
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(SyncError::Transport("invalid_endpoint"));
    }
    // Validate before URL parsing, which otherwise normalises encoded dot paths.
    let raw_path = authority_and_path[authority.len()..]
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    for component in raw_path.split('/') {
        let mut decoded = Vec::new();
        let bytes = component.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                if index + 2 >= bytes.len() {
                    return Err(SyncError::Transport("invalid_endpoint"));
                }
                let digit = |byte: u8| (byte as char).to_digit(16);
                let a = digit(bytes[index + 1]).ok_or(SyncError::Transport("invalid_endpoint"))?;
                let b = digit(bytes[index + 2]).ok_or(SyncError::Transport("invalid_endpoint"))?;
                decoded.push((a * 16 + b) as u8);
                index += 3;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        if decoded == b"."
            || decoded == b".."
            || decoded
                .iter()
                .any(|b| b"/\\%".contains(b) || *b < 0x20 || *b == 0x7f)
        {
            return Err(SyncError::Transport("invalid_endpoint"));
        }
    }
    let mut url = Url::parse(raw).map_err(|_| SyncError::Transport("invalid_endpoint"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
        || url.path().contains("//")
    {
        return Err(SyncError::Transport("invalid_endpoint"));
    }
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        _ => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && allow_loopback_http && loopback) {
        return Err(SyncError::Transport("https_required"));
    }
    if !url.path().ends_with('/') {
        url.path_segments_mut()
            .map_err(|_| SyncError::Transport("invalid_endpoint"))?
            .push("");
    }
    Ok(url)
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

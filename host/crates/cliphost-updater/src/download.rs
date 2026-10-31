use crate::package::{valid_hash, Version, MAX_ARCHIVE};
use crate::{invalid, OfficialRelease, Result};
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    time::Duration,
};

const BITIFUL: &str = "https://quicker-pkgs.s3.bitiful.net";
const GITHUB: &str = "https://api.github.com/repos/ceastld/eaclip/releases/latest";
const UPDATE_API: &str = "https://update.alinko.cc/eaclip/version.json";

fn allowed_url(value: &url::Url) -> bool {
    value.scheme() == "https"
        && value.username().is_empty()
        && value.password().is_none()
        && value.port().is_none_or(|p| p == 443)
        && matches!(
            value.host_str(),
            Some(
                "update.alinko.cc"
                    | "quicker-pkgs.s3.bitiful.net"
                    | "api.github.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

fn client(timeout: Duration) -> Result<Client> {
    // Same provider as ClipHost; helper executes before normal host TLS setup.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(Client::builder()
        .user_agent(concat!("EaClip-Updater/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .connect_timeout(Duration::from_secs(12))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !allowed_url(attempt.url()) {
                attempt.error("更新源重定向不可信")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

fn bounded_get(client: &Client, address: &str, limit: u64) -> Result<Vec<u8>> {
    let url = url::Url::parse(address).map_err(|_| invalid("更新源地址无效"))?;
    if !allowed_url(&url) {
        return Err(invalid("更新源地址不可信"));
    }
    let response = client.get(url).send()?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(invalid("更新元数据过大"));
    }
    let mut data = Vec::new();
    response.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err(invalid("更新元数据过大"));
    }
    Ok(data)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiManifest {
    product: String,
    release: String,
    version: String,
    download_url: String,
    sha256_sums_url: String,
    sha256: Option<String>,
    size: Option<u64>,
    notes: Option<String>,
}

fn validate_api_manifest(manifest: &ApiManifest) -> Result<String> {
    let release = Version::parse_release(&manifest.release)?;
    if manifest.product != "eaclip"
        || Version::parse(&manifest.version)? != release
        || manifest.version != format!("{}.0", manifest.release)
        || manifest
            .size
            .is_some_and(|size| size == 0 || size > MAX_ARCHIVE)
        || manifest
            .sha256
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
    {
        return Err(invalid("版本检查API的产品、版本或摘要无效"));
    }
    let stem = format!("eaclip-{}-windows-x64", manifest.release);
    let github = format!(
        "https://github.com/ceastld/eaclip/releases/download/rust-v{}/{stem}",
        manifest.release
    );
    let domestic = format!("{BITIFUL}/eaclip/rust-v{}/{stem}", manifest.release);
    if ![format!("{github}.zip"), format!("{domestic}.zip")].contains(&manifest.download_url)
        || ![
            format!("{github}.SHA256SUMS.txt"),
            format!("{domestic}.SHA256SUMS.txt"),
        ]
        .contains(&manifest.sha256_sums_url)
    {
        return Err(invalid("版本检查API的附件地址与正式版本不匹配"));
    }
    Ok(stem)
}

fn api_release(client: &Client) -> Result<OfficialRelease> {
    let manifest: ApiManifest =
        serde_json::from_slice(&bounded_get(client, UPDATE_API, 64 * 1024)?)?;
    let stem = validate_api_manifest(&manifest)?;
    let sha256 = match manifest.sha256 {
        Some(hash) => hash.to_lowercase(),
        None => {
            let sums = bounded_get(client, &manifest.sha256_sums_url, 16 * 1024)?;
            parse_sums(&sums, &stem)?[&format!("{stem}.zip")].clone()
        }
    };
    let github = format!(
        "https://github.com/ceastld/eaclip/releases/download/rust-v{}/{stem}.zip",
        manifest.release
    );
    let domestic = format!("{BITIFUL}/eaclip/rust-v{}/{stem}.zip", manifest.release);
    let alternate = if manifest.download_url == github {
        domestic
    } else {
        github
    };
    Ok(OfficialRelease {
        version: manifest.version,
        download_urls: vec![manifest.download_url, alternate],
        sha256,
        size: manifest.size,
        release_url: format!(
            "https://github.com/ceastld/eaclip/releases/tag/rust-v{}",
            manifest.release
        ),
        release_notes: manifest.notes,
        source: "Cloudflare".into(),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pointer {
    package_name: String,
    version: String,
    release: String,
    build: u64,
}

fn domestic_release(client: &Client) -> Result<OfficialRelease> {
    let pointer: Pointer = serde_json::from_slice(&bounded_get(
        client,
        &format!("{BITIFUL}/cea.cliphost.rust/version.json"),
        64 * 1024,
    )?)?;
    domestic_from_pointer(client, pointer)
}

fn domestic_from_pointer(client: &Client, pointer: Pointer) -> Result<OfficialRelease> {
    validate_pointer(&pointer)?;
    let version = pointer.release;
    let stem = format!("eaclip-{version}-windows-x64");
    let base = format!("{BITIFUL}/eaclip/rust-v{version}/{stem}");
    let sums = bounded_get(client, &format!("{base}.SHA256SUMS.txt"), 16 * 1024)?;
    let sums = parse_sums(&sums, &stem)?;
    Ok(OfficialRelease {
        version: pointer.version,
        download_urls: vec![
            format!("{base}.zip"),
            format!(
                "https://github.com/ceastld/eaclip/releases/download/rust-v{version}/{stem}.zip"
            ),
        ],
        sha256: sums[&format!("{stem}.zip")].clone(),
        size: None,
        release_url: format!("https://github.com/ceastld/eaclip/releases/tag/rust-v{version}"),
        release_notes: None,
        source: "Bitiful".into(),
    })
}

fn validate_pointer(pointer: &Pointer) -> Result<()> {
    let remote = Version::parse_release(&pointer.release)?;
    let full = Version::parse(&pointer.version)?;
    if pointer.package_name != "cea.cliphost.rust"
        || pointer.build != 0
        || full != remote
        || pointer.version != format!("{}.0", pointer.release)
    {
        return Err(invalid("国内更新源的包身份或版本不匹配"));
    }
    Ok(())
}

fn parse_sums(bytes: &[u8], stem: &str) -> Result<HashMap<String, String>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid("更新摘要不是UTF-8"))?
        .trim_start_matches('\u{feff}');
    let names = [format!("{stem}.zip"), format!("{stem}.build-info.json")];
    let mut values = HashMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (hash, name) = line
            .split_once("  ")
            .ok_or_else(|| invalid("更新摘要格式无效"))?;
        if !valid_hash(hash)
            || !names.iter().any(|allowed| allowed == name)
            || values.insert(name.into(), hash.to_lowercase()).is_some()
        {
            return Err(invalid("更新摘要包含重复或未知文件"));
        }
    }
    if values.len() != 2 {
        return Err(invalid("更新摘要缺少文件"));
    }
    Ok(values)
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    html_url: String,
    body: Option<String>,
    assets: Vec<GithubAsset>,
}
#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    size: u64,
    state: String,
    digest: Option<String>,
    browser_download_url: String,
}

fn github_release(client: &Client) -> Result<OfficialRelease> {
    let release: GithubRelease =
        serde_json::from_slice(&bounded_get(client, GITHUB, 2 * 1024 * 1024)?)?;
    parse_github(release)
}

fn parse_github(release: GithubRelease) -> Result<OfficialRelease> {
    if release.draft || release.prerelease {
        return Err(invalid("更新源不是正式发布"));
    }
    let version = release
        .tag_name
        .strip_prefix("rust-v")
        .ok_or_else(|| invalid("发布标签不是Rust正式版"))?;
    Version::parse_release(version)?;
    let release_url = format!("https://github.com/ceastld/eaclip/releases/tag/rust-v{version}");
    if release.html_url != release_url {
        return Err(invalid("发布仓库不匹配"));
    }
    let name = format!("eaclip-{version}-windows-x64.zip");
    let assets: Vec<_> = release.assets.iter().filter(|a| a.name == name).collect();
    if assets.len() != 1 {
        return Err(invalid("发布缺少唯一的x64便携包"));
    }
    let asset = assets[0];
    let hash = asset
        .digest
        .as_deref()
        .and_then(|v| v.strip_prefix("sha256:"))
        .ok_or_else(|| invalid("GitHub附件缺少可信SHA-256摘要"))?;
    let address =
        format!("https://github.com/ceastld/eaclip/releases/download/rust-v{version}/{name}");
    if !valid_hash(hash)
        || asset.size == 0
        || asset.size > MAX_ARCHIVE
        || asset.state != "uploaded"
        || asset.browser_download_url != address
    {
        return Err(invalid("GitHub发布附件身份或摘要无效"));
    }
    Ok(OfficialRelease {
        version: format!("{version}.0"),
        download_urls: vec![format!("{BITIFUL}/eaclip/rust-v{version}/{name}"), address],
        sha256: hash.to_lowercase(),
        size: Some(asset.size),
        release_url,
        release_notes: release.body,
        source: "GitHub".into(),
    })
}

/// Fetches fixed public HTTPS metadata only. This performs no filesystem writes.
pub fn check_official_release() -> Result<OfficialRelease> {
    let client = client(Duration::from_secs(12))?;
    with_fallbacks(
        || api_release(&client),
        || domestic_release(&client),
        || github_release(&client),
    )
}

fn with_fallbacks(
    api: impl FnOnce() -> Result<OfficialRelease>,
    domestic: impl FnOnce() -> Result<OfficialRelease>,
    github: impl FnOnce() -> Result<OfficialRelease>,
) -> Result<OfficialRelease> {
    let api_error = match api() {
        Ok(release) => return Ok(release),
        Err(error) => error,
    };
    match domestic() {
        Ok(release) => Ok(release),
        Err(domestic) => github().map_err(|github| {
            invalid(format!(
                "检查更新失败。版本API：{api_error}；国内源：{domestic}；GitHub：{github}"
            ))
        }),
    }
}

pub(crate) fn check_for_version(current: &str) -> Result<OfficialRelease> {
    let domestic = check_official_release()?;
    if Version::parse(&domestic.version)? > Version::parse(current)? || domestic.source != "Bitiful"
    {
        return Ok(domestic);
    }
    // A mirror may lag when a release deliberately skips its upload. Failure
    // of this supplementary check does not make the domestic source unusable.
    if let Ok(github) = github_release(&client(Duration::from_secs(8))?) {
        if Version::parse(&github.version)? > Version::parse(&domestic.version)? {
            return Ok(github);
        }
    }
    Ok(domestic)
}

pub(crate) fn download_archive(
    release: &OfficialRelease,
    destination: &Path,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let client = client(Duration::from_secs(300))?;
    download_with_fetch(release, destination, progress, |address| {
        let url = url::Url::parse(address).map_err(|_| invalid("更新包URL无效"))?;
        if !allowed_url(&url) {
            return Err(invalid("更新包URL不可信"));
        }
        Ok(client.get(url).send()?.error_for_status()?)
    })
}

// The streaming verifier is shared by production HTTPS and the explicitly
// compiled simulation transport. Production source/redirect rules stay above.
fn download_with_fetch(
    release: &OfficialRelease,
    destination: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
    mut fetch: impl FnMut(&str) -> Result<reqwest::blocking::Response>,
) -> Result<()> {
    let mut errors = Vec::new();
    for address in &release.download_urls {
        let mut created = false;
        let attempt = (|| {
            let mut response = fetch(address)?;
            let length = response.content_length();
            if length.is_some_and(|v| v > MAX_ARCHIVE || release.size.is_some_and(|s| v != s)) {
                return Err(invalid("更新包大小不匹配"));
            }
            let total = release.size.or(length);
            let mut file = File::options()
                .write(true)
                .create_new(true)
                .open(destination)?;
            created = true;
            let mut sha = Sha256::new();
            let mut count = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = response.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                count += read as u64;
                if count > MAX_ARCHIVE || total.is_some_and(|n| count > n) {
                    return Err(invalid("更新包超过预期大小"));
                }
                file.write_all(&buffer[..read])?;
                sha.update(&buffer[..read]);
                progress(count, total);
            }
            file.sync_all()?;
            if count == 0
                || total.is_some_and(|n| count != n)
                || format!("{:x}", sha.finalize()) != release.sha256
            {
                return Err(invalid("更新包SHA-256或大小校验失败"));
            }
            Ok(())
        })();
        match attempt {
            Ok(()) => return Ok(()),
            Err(error) => {
                errors.push(error.to_string());
                if created && destination.exists() {
                    fs::remove_file(destination)?;
                }
            }
        }
    }
    Err(invalid(format!(
        "所有更新下载源均失败：{}",
        errors.join("；")
    )))
}

#[cfg(feature = "test-support")]
pub(crate) fn download_loopback(
    release: &OfficialRelease,
    destination: &Path,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    download_with_fetch(release, destination, progress, |address| {
        let url = url::Url::parse(address).map_err(|_| invalid("模拟更新URL无效"))?;
        let loopback = matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip == std::net::Ipv4Addr::LOCALHOST)
            || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip == std::net::Ipv6Addr::LOCALHOST);
        if url.scheme() != "http"
            || !loopback
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid("模拟下载只允许127.0.0.1或::1的HTTP地址"));
        }
        let response = client.get(url).send()?.error_for_status()?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(invalid("模拟下载拒绝重定向及非200响应"));
        }
        Ok(response)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_manifest() -> ApiManifest {
        serde_json::from_value(serde_json::json!({
            "product": "eaclip",
            "release": "0.32.5",
            "version": "0.32.5.0",
            "downloadUrl": "https://github.com/ceastld/eaclip/releases/download/rust-v0.32.5/eaclip-0.32.5-windows-x64.zip",
            "sha256SumsUrl": "https://github.com/ceastld/eaclip/releases/download/rust-v0.32.5/eaclip-0.32.5-windows-x64.SHA256SUMS.txt",
            "notes": "ClipHost 0.32.5 (GPUI)"
        })).unwrap()
    }

    #[test]
    fn api_manifest_accepts_deployed_schema_and_inline_hash() {
        let mut manifest = api_manifest();
        assert_eq!(
            validate_api_manifest(&manifest).unwrap(),
            "eaclip-0.32.5-windows-x64"
        );
        manifest.sha256 = Some("a".repeat(64));
        manifest.size = Some(23_945_856);
        assert!(validate_api_manifest(&manifest).is_ok());
    }

    #[test]
    fn api_manifest_rejects_wrong_product_version_and_attachments() {
        for (field, value) in [
            ("product", "raishot"),
            ("release", "0.32.5-beta"),
            ("version", "0.32.6.0"),
            ("version", "0.32.5.1"),
            ("downloadUrl", "https://github.com/ceastld/eaclip/releases/download/rust-v0.32.4/eaclip-0.32.4-windows-x64.zip"),
            ("downloadUrl", "https://github.com/ceastld/other/releases/download/rust-v0.32.5/eaclip-0.32.5-windows-x64.zip"),
            ("sha256SumsUrl", "https://example.test/sums.txt"),
            ("sha256", "invalid"),
        ] {
            let mut json = serde_json::to_value(serde_json::json!({
                "product": "eaclip", "release": "0.32.5", "version": "0.32.5.0",
                "downloadUrl": api_manifest().download_url,
                "sha256SumsUrl": api_manifest().sha256_sums_url,
            })).unwrap();
            json[field] = value.into();
            let manifest: ApiManifest = serde_json::from_value(json).unwrap();
            assert!(validate_api_manifest(&manifest).is_err(), "{field}: {value}");
        }
    }

    fn fake_release(source: &str) -> OfficialRelease {
        OfficialRelease {
            version: "0.32.5.0".into(),
            download_urls: vec![],
            sha256: "a".repeat(64),
            size: None,
            release_url: String::new(),
            release_notes: None,
            source: source.into(),
        }
    }

    #[test]
    fn api_success_does_not_query_legacy_sources_and_failure_falls_back() {
        let release = with_fallbacks(
            || Ok(fake_release("Cloudflare")),
            || panic!("successful API must not query Bitiful"),
            || panic!("successful API must not query GitHub"),
        )
        .unwrap();
        assert_eq!(release.source, "Cloudflare");
        let release = with_fallbacks(
            || Err(invalid("API unavailable")),
            || Ok(fake_release("Bitiful")),
            || panic!("successful Bitiful fallback must not query GitHub"),
        )
        .unwrap();
        assert_eq!(release.source, "Bitiful");
        let release = with_fallbacks(
            || Err(invalid("API unavailable")),
            || Err(invalid("Bitiful unavailable")),
            || Ok(fake_release("GitHub")),
        )
        .unwrap();
        assert_eq!(release.source, "GitHub");
        let error = with_fallbacks(
            || Err(invalid("API unavailable")),
            || Err(invalid("Bitiful unavailable")),
            || Err(invalid("GitHub unavailable")),
        )
        .unwrap_err()
        .to_string();
        for message in [
            "API unavailable",
            "Bitiful unavailable",
            "GitHub unavailable",
        ] {
            assert!(error.contains(message));
        }
    }
    #[test]
    fn pointer_rejects_other_product_and_prerelease() {
        let mut p = Pointer {
            package_name: "cea.cliphost.rust".into(),
            version: "0.31.31.0".into(),
            release: "0.31.31".into(),
            build: 0,
        };
        assert!(validate_pointer(&p).is_ok());
        p.package_name = "cea.cliphost".into();
        assert!(validate_pointer(&p).is_err());
        p.package_name = "cea.cliphost.rust".into();
        p.release = "0.31.32-beta".into();
        assert!(validate_pointer(&p).is_err());
    }
    #[test]
    fn sums_rejects_duplicate_unknown_and_missing() {
        let stem = "eaclip-1.2.3-windows-x64";
        let good = format!(
            "{}  {stem}.zip\n{}  {stem}.build-info.json\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert!(parse_sums(good.as_bytes(), stem).is_ok());
        assert!(parse_sums(
            format!("{good}{}  {stem}.zip", "a".repeat(64)).as_bytes(),
            stem
        )
        .is_err());
        assert!(parse_sums(good.replace(".build-info.json", ".exe").as_bytes(), stem).is_err());
    }
    #[test]
    fn redirects_require_official_https_hosts() {
        for address in [
            "http://github.com/a",
            "https://github.com.evil.test/a",
            "https://user@github.com/a",
            "https://github.com:444/a",
        ] {
            assert!(!allowed_url(&url::Url::parse(address).unwrap()));
        }
        assert!(allowed_url(
            &url::Url::parse("https://release-assets.githubusercontent.com/a").unwrap()
        ));
        assert!(allowed_url(&url::Url::parse(UPDATE_API).unwrap()));
        assert!(!allowed_url(
            &url::Url::parse("https://update.alinko.cc.evil.test/eaclip/version.json").unwrap()
        ));
    }
}

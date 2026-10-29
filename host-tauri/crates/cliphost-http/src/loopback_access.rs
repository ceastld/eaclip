//! Browser access boundary for the native loopback API. Native callers do not
//! send Origin/Fetch Metadata; browser requests must identify a trusted page.

use axum::extract::Request;
use axum::http::{header, uri::Authority, HeaderMap, HeaderName, HeaderValue, Method, Uri};

fn one_header<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
) -> Result<Option<&'a HeaderValue>, ()> {
    let mut values = headers.get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    Ok(first)
}

fn loopback_authority(value: &str) -> bool {
    let Ok(authority) = value.parse::<Authority>() else {
        return false;
    };
    let host = authority.host();
    if !(host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]")
        || value.contains('@')
    {
        return false;
    }
    let suffix = &value[host.len()..];
    suffix.is_empty()
        || suffix.strip_prefix(':').is_some_and(|port| {
            !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port != 0)
        })
}

fn trusted_origin(value: &str, development: bool) -> bool {
    // Tauri's bundled Windows asset origin (HTTP by default, HTTPS when enabled).
    if matches!(value, "http://tauri.localhost" | "https://tauri.localhost") {
        return true;
    }
    if !development {
        return false;
    }
    let Some(authority) = value.strip_prefix("http://") else {
        return false;
    };
    // Origin is a serialized origin, never a URL with path/query/credentials.
    loopback_authority(authority)
}

fn trusted_referer(value: &HeaderValue, development: bool) -> bool {
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Ok(uri) = value.parse::<Uri>() else {
        return false;
    };
    match (uri.scheme_str(), uri.authority()) {
        (Some(scheme), Some(authority)) => {
            trusted_origin(&format!("{scheme}://{authority}"), development)
        }
        _ => false,
    }
}

pub(super) fn check_request(req: &Request, development: bool) -> Result<Option<HeaderValue>, ()> {
    let headers = req.headers();
    // Browsers always send a Host/:authority; validate it even when Origin is
    // absent so an attacker-controlled DNS name cannot rebind to loopback.
    if let Some(host) = one_header(headers, &header::HOST)? {
        if !loopback_authority(host.to_str().map_err(|_| ())?) {
            return Err(());
        }
    }
    if let Some(authority) = req.uri().authority() {
        if !loopback_authority(authority.as_str()) {
            return Err(());
        }
    }
    if let Some(origin) = one_header(headers, &header::ORIGIN)? {
        if !trusted_origin(origin.to_str().map_err(|_| ())?, development) {
            return Err(());
        }
        return Ok(Some(origin.clone()));
    }

    let referer = one_header(headers, &header::REFERER)?;
    let trusted_page = referer.is_some_and(|value| trusted_referer(value, development));
    if referer.is_some() && !trusted_page {
        return Err(());
    }
    let fetch_site = one_header(headers, &HeaderName::from_static("sec-fetch-site"))?;
    let fetch_mode = one_header(headers, &HeaderName::from_static("sec-fetch-mode"))?;
    let fetch_dest = one_header(headers, &HeaderName::from_static("sec-fetch-dest"))?;
    if fetch_site.is_some() || fetch_mode.is_some() || fetch_dest.is_some() {
        // Browser writes require an explicit trusted Origin. Cross-origin img
        // loads normally omit it, so trusted Referer permits read-only assets.
        if req.method() != Method::GET && req.method() != Method::HEAD {
            return Err(());
        }
        match fetch_site.and_then(|value| value.to_str().ok()) {
            Some("same-origin" | "none") => {}
            Some("same-site" | "cross-site") | None if trusted_page => {}
            _ => return Err(()),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{event_bus, router, HttpState};
    use axum::{body::Body, http::StatusCode};
    use cliphost_core::{paths::AppPaths, HostState};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    fn fixture() -> (tempfile::TempDir, HttpState) {
        cliphost_core::force_headless(true);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("loopback-access-")
            .tempdir_in(root)
            .unwrap();
        let host = HostState::open(
            AppPaths::from_data_root(directory.path().to_path_buf()),
            29065,
        )
        .unwrap();
        (
            directory,
            HttpState {
                host,
                events: event_bus(),
            },
        )
    }

    fn mutation_request() -> axum::http::request::Builder {
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/rpc")
            .header(header::HOST, "127.0.0.1:29065")
            // This is a simple browser POST: no CORS preflight is necessary.
            .header(header::CONTENT_TYPE, "text/plain")
    }

    fn mutation_body() -> Body {
        Body::from(
            json!({"id":"boundary", "method":"lan.config.update",
            "params":{"deviceAlias":"browser-changed"}})
            .to_string(),
        )
    }

    #[test]
    fn production_origin_policy_is_separate_from_development() {
        for origin in ["http://tauri.localhost", "https://tauri.localhost"] {
            assert!(trusted_origin(origin, false));
        }
        for origin in [
            "http://127.0.0.1:5280",
            "http://localhost:5281",
            "http://[::1]:5280",
        ] {
            assert!(!trusted_origin(origin, false));
            assert!(trusted_origin(origin, true));
        }
        for origin in [
            "null",
            "https://evil.test",
            "http://tauri.localhost.evil.test",
            "http://127.0.0.1.evil.test:5280",
            "http://user@localhost:5280",
            "http://localhost:5280/",
            "http://localhost:5280?x=1",
            "http://localhost:",
            "http://localhost:0",
            "http://localhost:99999",
        ] {
            assert!(!trusted_origin(origin, false), "{origin}");
            assert!(!trusted_origin(origin, true), "{origin}");
        }
    }

    #[tokio::test]
    async fn hostile_simple_rpc_is_rejected_before_state_changes() {
        let (_directory, state) = fixture();
        let alias = state.host.lan_status()["deviceAlias"].clone();
        let app = router(state.clone());
        for origin in [
            "https://evil.test",
            "null",
            "http://tauri.localhost.evil.test",
        ] {
            let response = app
                .clone()
                .oneshot(
                    mutation_request()
                        .header(header::ORIGIN, origin)
                        .body(mutation_body())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin}");
            assert!(!response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
            assert_eq!(state.host.lan_status()["deviceAlias"], alias);
        }
        for site in ["cross-site", "same-site", "same-origin", "none"] {
            let response = app
                .clone()
                .oneshot(
                    mutation_request()
                        .header("Sec-Fetch-Site", site)
                        .header(header::REFERER, "http://tauri.localhost/clipboard")
                        .body(mutation_body())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "missing Origin: {site}"
            );
            assert_eq!(state.host.lan_status()["deviceAlias"], alias);
        }
        let response = app
            .oneshot(
                mutation_request()
                    .header(header::ORIGIN, "http://tauri.localhost")
                    .header(header::ORIGIN, "https://evil.test")
                    .body(mutation_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(state.host.lan_status()["deviceAlias"], alias);
    }

    #[tokio::test]
    async fn rebinding_hosts_and_untrusted_browser_reads_are_rejected() {
        let (_directory, state) = fixture();
        let app = router(state.clone());
        let alias = state.host.lan_status()["deviceAlias"].clone();
        for host in [
            "evil.test:29065",
            "localhost.evil.test",
            "127.0.0.1.evil.test",
            "user@127.0.0.1:29065",
            "localhost:",
            "localhost:0",
            "localhost:99999",
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/api/rpc")
                        .header(header::HOST, host)
                        .header(header::ORIGIN, "http://tauri.localhost")
                        .body(mutation_body())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host}");
            assert_eq!(state.host.lan_status()["deviceAlias"], alias);
        }
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("http://evil.test/api/rpc")
                    .header(header::HOST, "127.0.0.1:29065")
                    .body(mutation_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(state.host.lan_status()["deviceAlias"], alias);
        for path in [
            "/api/health",
            "/api/events",
            "/api/clip/local-image?path=test.png",
        ] {
            for origin in [Some("https://evil.test"), None] {
                let mut request = axum::http::Request::builder()
                    .uri(path)
                    .header(header::HOST, "127.0.0.1:29065");
                request = match origin {
                    Some(origin) => request.header(header::ORIGIN, origin),
                    None => request.header("Sec-Fetch-Site", "cross-site"),
                };
                let response = app
                    .clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::FORBIDDEN,
                    "{path}: {origin:?}"
                );
                assert!(!response
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
            }
        }
    }

    #[tokio::test]
    async fn trusted_tauri_and_native_rpc_remain_usable() {
        let (_directory, state) = fixture();
        let app = router(state.clone());
        for origin in [
            Some("http://tauri.localhost"),
            Some("https://tauri.localhost"),
            None,
        ] {
            let mut request = mutation_request();
            if let Some(origin) = origin {
                request = request
                    .header(header::ORIGIN, origin)
                    .header("Sec-Fetch-Site", "cross-site");
            }
            let response = app
                .clone()
                .oneshot(request.body(mutation_body()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response
                    .headers()
                    .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .and_then(|value| value.to_str().ok()),
                origin
            );
            assert_eq!(response.headers().get(header::VARY).unwrap(), "Origin");
            let body: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(body["ok"], true, "{body}");
            assert_eq!(state.host.lan_status()["deviceAlias"], "browser-changed");
        }
        // Tauri <img> and CSS requests omit Origin. Their trusted page Referer
        // permits reads while a hostile/no-referrer cross-site request is denied.
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/health")
                    .header(header::HOST, "127.0.0.1:29065")
                    .header("Sec-Fetch-Site", "cross-site")
                    .header("Sec-Fetch-Mode", "no-cors")
                    .header("Sec-Fetch-Dest", "image")
                    .header(header::REFERER, "http://tauri.localhost/clipboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
    }

    #[tokio::test]
    async fn preflight_rejects_unknown_pages_and_only_echoes_trusted_origins() {
        let (_directory, state) = fixture();
        let app = router(state);
        for (origin, expected) in [
            ("https://evil.test", StatusCode::FORBIDDEN),
            ("null", StatusCode::FORBIDDEN),
            ("http://tauri.localhost", StatusCode::NO_CONTENT),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("OPTIONS")
                        .uri("/api/rpc")
                        .header(header::HOST, "127.0.0.1:29065")
                        .header(header::ORIGIN, origin)
                        .header("Access-Control-Request-Method", "POST")
                        .header("Access-Control-Request-Private-Network", "true")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if expected == StatusCode::NO_CONTENT {
                assert_eq!(
                    response
                        .headers()
                        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                        .unwrap(),
                    origin
                );
                assert_eq!(
                    response
                        .headers()
                        .get("access-control-allow-private-network")
                        .unwrap(),
                    "true"
                );
            } else {
                assert!(!response
                    .headers()
                    .contains_key("access-control-allow-private-network"));
                assert!(!response
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
            }
        }
    }

    #[tokio::test]
    async fn development_proxy_origin_obeys_build_mode() {
        let (_directory, state) = fixture();
        let response = router(state)
            .oneshot(
                mutation_request()
                    .header(header::ORIGIN, "http://127.0.0.1:5280")
                    .header("Sec-Fetch-Site", "same-origin")
                    .body(mutation_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if cfg!(debug_assertions) {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            }
        );
    }
}

//! Real HostState placement admission with deterministic work between the two
//! identity checks. All state and storage belong to a repository-local fixture.
use super::*;

const POPUP_HWND: isize = 100;

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&directory).unwrap();
    let root = tempfile::Builder::new()
        .prefix("native-qp-placement-")
        .tempdir_in(directory)
        .unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 29065).unwrap();
    host.add_text(&json!({"text":"placement fixture item"}))
        .unwrap();
    host.set_qp_hwnd(POPUP_HWND);
    assert_eq!(host.qp_debug_begin()["overlayVisible"], true);
    {
        let mut inner = host.inner.lock();
        let mut place = inner.qp.overlay_place.expect("native popup placement");
        place.window_left = -1200;
        place.window_top = -300;
        place.window_width = 1000;
        place.window_height = 600;
        inner.qp.overlay_place = Some(place);
        qp_complete_reveal(&mut inner);
    }
    while host.take_chrome_command().is_some() {}
    (root, host)
}

fn native_body(host: &HostState) -> Value {
    let inner = host.inner.lock();
    json!({
        "stateRevision":inner.qp.state_revision,
        "sessionRevision":inner.qp.session_revision,
        "sourceHwnd":inner.qp_hwnd,
        "clusterLeft":10.0,"clusterTop":20.0,
        "clusterWidth":100.0,"clusterHeight":40.0,
        "viewportWidth":500.0,"viewportHeight":300.0,
        "devicePixelRatio":1.5
    })
}

#[derive(Debug, PartialEq)]
struct PlacementState {
    hit: (i32, i32, i32, i32, bool),
    ready: bool,
    visible: bool,
    opacity: f64,
    awaiting_reveal: bool,
    reveal_deadline: Option<Instant>,
    chrome: Vec<(ChromeCommand, Option<u64>)>,
}

fn snapshot(host: &HostState) -> PlacementState {
    let inner = host.inner.lock();
    let hit = inner.qp_wheel_hit;
    PlacementState {
        hit: (hit.left, hit.top, hit.width, hit.height, hit.visible),
        ready: inner.qp.overlay_ready,
        visible: inner.qp.overlay_visible,
        opacity: inner.qp.overlay_opacity,
        awaiting_reveal: inner.qp.awaiting_reveal,
        reveal_deadline: inner.qp.reveal_deadline,
        chrome: inner
            .chrome
            .0
            .iter()
            .map(|entry| (entry.command, entry.presentation_epoch))
            .collect(),
    }
}

fn pending_reveal(host: &HostState) {
    let mut inner = host.inner.lock();
    inner.qp.overlay_ready = false;
    inner.qp.overlay_opacity = 0.0;
    inner.qp.awaiting_reveal = true;
    inner.qp.reveal_deadline = Some(Instant::now() + Duration::from_secs(1));
    inner.qp_wheel_hit.visible = false;
}

#[test]
fn old_placement_snapshot_cannot_reveal_or_rewrite_a_reopened_session() {
    for legacy in [false, true] {
        let (_root, host) = fixture();
        let mut body = native_body(&host);
        if legacy {
            let object = body.as_object_mut().unwrap();
            object.remove("stateRevision");
            object.remove("sessionRevision");
            object.remove("sourceHwnd");
        }
        let old_session = host.inner.lock().qp.session_revision;
        let mut replacement_state = None;
        let reply = host.qp_placement_ready_after_snapshot(&body, || {
            host.qp_debug_end();
            assert_eq!(host.qp_debug_begin()["overlayVisible"], true);
            assert_ne!(host.inner.lock().qp.session_revision, old_session);
            pending_reveal(&host);
            replacement_state = Some(snapshot(&host));
        });
        assert_eq!(reply["ok"], false, "legacy={legacy}: {reply}");
        assert_eq!(
            reply["reason"],
            if legacy {
                "session-retired"
            } else {
                "revision-mismatch"
            }
        );
        assert_eq!(
            snapshot(&host),
            replacement_state.unwrap(),
            "legacy={legacy}"
        );
    }
}

#[test]
fn native_placement_rejects_wrong_session_source_and_truncated_revision() {
    let (_root, host) = fixture();
    pending_reveal(&host);
    let accepted = native_body(&host);
    let original = snapshot(&host);
    let state_revision = host.inner.lock().qp.state_revision;
    let session_revision = host.inner.lock().qp.session_revision;
    for (field, value, reason) in [
        (
            "sessionRevision",
            json!(session_revision + 1),
            "session-mismatch",
        ),
        ("sourceHwnd", json!(POPUP_HWND + 1), "source-mismatch"),
        (
            "stateRevision",
            json!(i64::from(state_revision) + (1_i64 << 32)),
            "invalid-revision",
        ),
    ] {
        let mut body = accepted.clone();
        body[field] = value;
        let reply = host.qp_placement_ready(&body);
        assert_eq!(reply["ok"], false, "{field}: {reply}");
        assert_eq!(reply["reason"], reason, "{field}");
        assert_eq!(snapshot(&host), original, "{field}");
    }

    // Retain the WebView's missing/nonpositive revision contract. Without
    // cluster metrics its valid native origin supplies the full-window hit.
    for revision in [None, Some(0), Some(-1)] {
        let mut body = json!({});
        if let Some(revision) = revision {
            body["stateRevision"] = json!(revision);
        }
        assert_eq!(host.qp_placement_ready(&body)["ok"], true);
        let state = snapshot(&host);
        assert_eq!(state.hit, (-1200, -300, 1000, 600, true));
        assert!(state.ready);
        assert!(!state.awaiting_reveal);
    }
}

#[test]
fn malformed_placement_keeps_the_last_hit_reveal_state_and_chrome_queue() {
    let (_root, host) = fixture();
    let accepted = native_body(&host);
    assert_eq!(host.qp_placement_ready(&accepted)["ok"], true);
    assert_eq!(snapshot(&host).hit, (-1185, -270, 150, 60, true));
    pending_reveal(&host);
    let original = snapshot(&host);

    let mut partial_cluster = accepted.clone();
    partial_cluster
        .as_object_mut()
        .unwrap()
        .remove("clusterHeight");
    let mut negative_size = accepted.clone();
    negative_size["clusterWidth"] = json!(-1.0);
    let mut overflow = accepted.clone();
    overflow["clusterWidth"] = json!(1e100);
    let mut zero_dpr = accepted.clone();
    zero_dpr["devicePixelRatio"] = json!(0.0);
    let mut partial_viewport = accepted.clone();
    partial_viewport
        .as_object_mut()
        .unwrap()
        .remove("viewportHeight");
    for (name, body) in [
        ("partial-cluster", partial_cluster),
        ("negative-size", negative_size),
        ("overflow", overflow),
        ("zero-dpr", zero_dpr),
        ("partial-viewport", partial_viewport),
    ] {
        let reply = host.qp_placement_ready(&body);
        assert_eq!(reply["ok"], false, "{name}: {reply}");
        assert_eq!(reply["reason"], "invalid-placement", "{name}");
        assert_eq!(snapshot(&host), original, "{name}");
    }
}

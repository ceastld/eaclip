//! Keep slow menu materialization out of the host lock and other menu sessions.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::explorer::OpenedFolder;
use crate::menu_api::{CollectionKind, MenuNodesLoad, MenuNodesResult, MenuBuildContext, MenuItemNode, MenuRegistry};

struct Session {
    action: String,
    closed: AtomicBool,
    // Each registry owns exactly one session. Node/page mutations stay serialized;
    // File reads and native window queries use snapshots outside this lock.
    registry: Mutex<MenuRegistry>,
}

#[derive(Default)]
pub struct MenuSessions {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl MenuSessions {
    #[cfg(test)]
    pub(crate) fn with_sessions_held_for_test<T>(&self, work: impl FnOnce() -> T) -> T {
        let _sessions = self.sessions.lock();
        work()
    }
    /// Register synthetic nodes through the normal per-session registry owner.
    #[cfg(test)]
    pub(crate) fn create_fixture_session(&self, ctx: &MenuBuildContext, nodes: Vec<MenuItemNode>) -> Value {
        let mut registry = MenuRegistry::default();
        let created = registry.create_fixture_session(ctx, nodes);
        let id = created["sessionId"].as_str().expect("fixture session id").to_string();
        self.sessions.lock().insert(id, Arc::new(Session {
            action: "window".into(),
            closed: AtomicBool::new(false),
            registry: Mutex::new(registry),
        }));
        created
    }

    pub fn create_from_context(&self, action: &str, style: Value, ctx: &MenuBuildContext) -> Value {
        let mut registry = MenuRegistry::default();
        let created = registry.create_from_context(action, style, ctx);
        let id = created["sessionId"]
            .as_str()
            .expect("created menu session id")
            .to_string();
        self.sessions.lock().insert(
            id,
            Arc::new(Session {
                action: action.into(),
                closed: AtomicBool::new(false),
                registry: Mutex::new(registry),
            }),
        );
        created
    }

    pub fn close(&self, id: &str) -> bool {
        let Some(session) = self.sessions.lock().remove(id) else {
            return false;
        };
        // Do not wait for a slow directory read. Its late result is discarded by
        // with_session, and dropping the last reader releases the retired tree.
        session.closed.store(true, Ordering::Release);
        true
    }

    /// Retire cached input ownership without removing/dropping a materialized
    /// tree on the hook thread. The ordinary worker removes it afterwards.
    pub(crate) fn try_retire(&self, id: &str) -> bool {
        let Some(sessions) = self.sessions.try_lock() else { return false; };
        let Some(session) = sessions.get(id) else { return false; };
        session.closed.store(true, Ordering::Release);
        true
    }

    pub(crate) fn with_session<T>(
        &self,
        id: &str,
        work: impl FnOnce(&mut MenuRegistry) -> Result<T, String>,
    ) -> Result<T, String> {
        let session = self
            .sessions
            .lock()
            .get(id)
            .cloned()
            .ok_or("Session not found")?;
        let mut registry = session.registry.lock();
        if session.closed.load(Ordering::Acquire) {
            return Err("Session not found".into());
        }
        let result = work(&mut registry);
        if session.closed.load(Ordering::Acquire) {
            return Err("Session not found".into());
        }
        result
    }

    pub fn get_nodes(
        &self,
        id: &str,
        node: &str,
        kind: CollectionKind,
        cursor: Option<&str>,
    ) -> Result<Value, String> {
        self.get_nodes_with_loader(id, node, kind, cursor, MenuNodesLoad::materialize)
    }

    fn get_nodes_with_loader(
        &self,
        id: &str,
        node: &str,
        kind: CollectionKind,
        cursor: Option<&str>,
        materialize: impl FnOnce(MenuNodesLoad) -> MenuNodesResult,
    ) -> Result<Value, String> {
        let session = self.sessions.lock().get(id).cloned().ok_or("Session not found")?;
        let load = {
            let mut registry = session.registry.lock();
            if session.closed.load(Ordering::Acquire) {
                return Err("Session not found".into());
            }
            match registry.prepare_nodes(id, node, kind, cursor)? {
                Some(load) => load,
                None => {
                    let result = registry.get_nodes(id, node, kind, cursor);
                    if session.closed.load(Ordering::Acquire) {
                        return Err("Session not found".into());
                    }
                    return result;
                }
            }
        };
        // Native queries may synchronously send messages to our own UI. Its
        // lifecycle checks need this registry, so never retain the lock while
        // querying HWNDs, reading files or waiting for RecentFiles readiness.
        let loaded = materialize(load);
        let mut registry = session.registry.lock();
        if session.closed.load(Ordering::Acquire) {
            return Err("Session not found".into());
        }
        let result = registry.commit_nodes(loaded);
        if session.closed.load(Ordering::Acquire) {
            return Err("Session not found".into());
        }
        result
    }

    pub fn node(&self, id: &str, node: &str) -> Result<MenuItemNode, String> {
        self.with_session(id, |registry| registry.node(id, node))
    }

    pub fn session_target_handle(&self, id: &str) -> i64 {
        self.with_session(id, |registry| Ok(registry.session_target_handle(id)))
            .unwrap_or(0)
    }

    pub fn session_opened_and_reopen(&self, id: &str) -> (Vec<OpenedFolder>, Vec<String>) {
        self.with_session(id, |registry| Ok(registry.session_opened_and_reopen(id)))
            .unwrap_or_default()
    }

    pub fn set_session_handles(&self, id: &str, source: i64, target: i64) {
        let _ = self.with_session(id, |registry| {
            registry.set_session_handles(id, source, target);
            Ok(())
        });
    }

    pub fn patch_check(&self, id: &str, node: &str, checked: bool) -> Result<Value, String> {
        self.with_session(id, |registry| registry.patch_check(id, node, checked))
    }

    pub fn patch_value(&self, id: &str, node: &str, current: f64) -> Result<Value, String> {
        self.with_session(id, |registry| registry.patch_value(id, node, current))
    }

    pub fn list(&self) -> Value {
        json!({"sessions": self.sessions.lock().iter().filter(|(_, session)| !session.closed.load(Ordering::Acquire)).map(|(id, session)| json!({
            "sessionId": id, "action": session.action,
        })).collect::<Vec<_>>()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu_api::LazyKind;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    fn file_session(menus: &MenuSessions, paths: &[PathBuf]) -> (String, Vec<String>) {
        let mut ctx = MenuBuildContext::default();
        ctx.file_menu.lines = paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
        ctx.menu_cfg.max_single_load_count = 5;
        ctx.menu_cfg.quick_load_submenu = false;
        let created = menus.create_from_context("file", json!({}), &ctx);
        let id = created["sessionId"].as_str().unwrap().to_string();
        let nodes = paths.iter().map(|path| {
            created["root"]["items"].as_array().unwrap().iter()
                .find(|node| node["payload"]["path"].as_str() == path.to_str())
                .unwrap()["id"].as_str().unwrap().to_string()
        }).collect();
        (id, nodes)
    }

    fn fixture_node(id: &str, role: &str, lazy: Option<LazyKind>) -> MenuItemNode {
        MenuItemNode {
            id: id.into(), kind: "Folder".into(), title: id.into(), description: String::new(),
            icon: String::new(), is_enabled: true, invoke_mode: "None".into(),
            keep_open_on_invoke: true, has_children: lazy.is_some(), has_context_menu: true,
            role: Some(role.into()), payload: json!({ "windowHandle": 42, "path": "fixture.txt" }),
            child_ids: Vec::new(), context_ids: Vec::new(), check: None, value: None,
            visual: None, lazy, pending: Vec::new(), served: 0,
        }
    }

    fn window_session(menus: &MenuSessions, role: &str, lazy: Option<LazyKind>) -> (String, String) {
        let mut ctx = MenuBuildContext::default();
        ctx.target_window_handle = 42;
        ctx.menu_cfg.max_single_load_count = 5;
        let node = fixture_node("window-parent", role, lazy);
        let node_id = node.id.clone();
        let created = menus.create_fixture_session(&ctx, vec![node]);
        (created["sessionId"].as_str().unwrap().into(), node_id)
    }

    fn fixture_results(prefix: &str) -> Vec<MenuItemNode> {
        (0..6).map(|index| fixture_node(&format!("{prefix}-{index}"), "fixture", None)).collect()
    }

    #[test]
    fn menu_lifetime_window_queries_keep_same_session_lifecycle_checks_responsive() {
        for (role, lazy, kind) in [
            ("window", Some(LazyKind::WindowOps { hwnd: 42 }), CollectionKind::Children),
            ("window-group", Some(LazyKind::WindowGroup { hwnds: Vec::new() }), CollectionKind::Children),
            ("window", Some(LazyKind::ContextWindow { hwnd: 42 }), CollectionKind::Children),
            ("window", None, CollectionKind::ContextMenu),
            ("window-group", None, CollectionKind::ContextMenu),
            ("file", None, CollectionKind::ContextMenu),
        ] {
            let menus = Arc::new(MenuSessions::default());
            let (id, node) = window_session(&menus, role, lazy);
            let (entered, started) = mpsc::channel();
            let (release, blocked) = mpsc::channel();
            let worker = menus.clone();
            let worker_id = id.clone();
            let worker_node = node.clone();
            let load = std::thread::spawn(move || {
                worker.get_nodes_with_loader(&worker_id, &worker_node, kind, None, |load| {
                    entered.send(()).unwrap();
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    load.fixture_result(fixture_results("loaded"))
                })
            });
            started.recv_timeout(Duration::from_secs(5)).expect("native load was not deferred");
            let (finished, response) = mpsc::channel();
            let worker = menus.clone();
            let worker_id = id.clone();
            let worker_node = node.clone();
            let lifecycle = std::thread::spawn(move || {
                // GPUI bind_native_super_menu_keyboard checks this same
                // session/target while Windows may be awaiting its UI thread.
                let target = worker.with_session(&worker_id, |registry| {
                    Ok(registry.session_target_handle(&worker_id))
                }).unwrap();
                worker.node(&worker_id, &worker_node).unwrap();
                worker.get_nodes(&worker_id, "root", CollectionKind::Children, None).unwrap();
                finished.send(target).unwrap();
            });
            let ready = response.recv_timeout(Duration::from_secs(1));
            // Always unblock and join before asserting a registry regression.
            release.send(()).unwrap();
            let page = load.join().unwrap().unwrap();
            lifecycle.join().unwrap();
            assert_eq!(ready.expect("lifecycle check waited for native window queries"), 42);
            if kind == CollectionKind::Children {
                assert_eq!(page["items"].as_array().unwrap().len(), 5);
                assert_eq!(page["hasMore"], true);
                let rest = menus.get_nodes_with_loader(&id, &node, kind, Some("more"), |_| {
                    panic!("continuation must reuse the loaded window children");
                }).unwrap();
                assert_eq!(rest["items"].as_array().unwrap().len(), 1);
                assert_eq!(rest["hasMore"], false);
            } else {
                assert_eq!(page["items"].as_array().unwrap().len(), 6);
                let cached = menus.get_nodes_with_loader(&id, &node, kind, None, |_| {
                    panic!("window context must reuse its already materialized items");
                }).unwrap();
                assert_eq!(cached["items"], page["items"]);
            }
        }
    }

    #[test]
    fn menu_lifetime_window_reload_and_close_reject_late_native_results() {
        for kind in [CollectionKind::Children, CollectionKind::ContextMenu] {
            let menus = Arc::new(MenuSessions::default());
            let (id, node) = window_session(&menus, "window", Some(LazyKind::WindowOps { hwnd: 42 }));
            let (entered, started) = mpsc::channel();
            let (release, blocked) = mpsc::channel();
            let worker = menus.clone();
            let worker_id = id.clone();
            let worker_node = node.clone();
            let old = std::thread::spawn(move || {
                worker.get_nodes_with_loader(&worker_id, &worker_node, kind, None, |load| {
                    let result = load.fixture_result(fixture_results("old"));
                    entered.send(()).unwrap();
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    result
                })
            });
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            let newer = menus.get_nodes_with_loader(&id, &node, kind, None, |load| {
                load.fixture_result(fixture_results("new"))
            }).unwrap();
            let committed = menus.node(&id, &node).unwrap();
            release.send(()).unwrap();
            assert_eq!(old.join().unwrap().unwrap_err(), "Node load superseded");
            let retained = menus.node(&id, &node).unwrap();
            assert_eq!(retained.child_ids, committed.child_ids);
            assert_eq!(retained.context_ids, committed.context_ids);
            assert_eq!(retained.served, committed.served);
            assert_eq!(newer["items"][0]["id"], "new-0");
            for index in 0..6 {
                assert!(menus.node(&id, &format!("old-{index}")).is_err(),
                    "superseded window load inserted an orphan node");
            }

            let (closing_id, closing_node) = window_session(&menus, "window", Some(LazyKind::WindowOps { hwnd: 42 }));
            let retired = menus.sessions.lock().get(&closing_id).cloned().unwrap();
            let (entered, started) = mpsc::channel();
            let (release, blocked) = mpsc::channel();
            let worker = menus.clone();
            let worker_id = closing_id.clone();
            let closing = std::thread::spawn(move || {
                worker.get_nodes_with_loader(&worker_id, &closing_node, kind, None, |load| {
                    entered.send(()).unwrap();
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    load.fixture_result(fixture_results("closed"))
                })
            });
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(menus.close(&closing_id));
            release.send(()).unwrap();
            assert_eq!(closing.join().unwrap().unwrap_err(), "Session not found");
            assert!(retired.registry.lock().node(&closing_id, "closed-0").is_err(),
                "closed native load committed into its retired registry");
        }
    }

    #[test]
    fn menu_lifetime_window_children_and_context_use_independent_load_generations() {
        let menus = MenuSessions::default();
        let (id, node) = window_session(&menus, "window", Some(LazyKind::WindowOps { hwnd: 42 }));
        let children = menus.with_session(&id, |registry| {
            Ok(registry.prepare_nodes(&id, &node, CollectionKind::Children, None)?.unwrap())
        }).unwrap().fixture_result(fixture_results("child"));
        let context = menus.with_session(&id, |registry| {
            Ok(registry.prepare_nodes(&id, &node, CollectionKind::ContextMenu, None)?.unwrap())
        }).unwrap().fixture_result(fixture_results("context"));
        menus.with_session(&id, |registry| registry.commit_nodes(context)).unwrap();
        menus.with_session(&id, |registry| registry.commit_nodes(children)).unwrap();
        let retained = menus.node(&id, &node).unwrap();
        assert_eq!(retained.child_ids.len(), 6);
        assert_eq!(retained.context_ids.len(), 6);
        assert_eq!(retained.served, 5);
    }

    #[test]
    fn menu_lifetime_file_load_keeps_same_session_responsive_and_paging() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        for index in 0..6 {
            std::fs::write(first.join(format!("{index}.txt")), b"x").unwrap();
        }
        std::fs::write(second.join("other.txt"), b"x").unwrap();
        let menus = Arc::new(MenuSessions::default());
        let (id, nodes) = file_session(&menus, &[first, second]);
        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let worker = menus.clone();
        let worker_id = id.clone();
        let worker_node = nodes[0].clone();
        let load = std::thread::spawn(move || {
            worker.get_nodes_with_loader(&worker_id, &worker_node, CollectionKind::Children, None, |load| {
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                load.materialize()
            })
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let (finished, response) = mpsc::channel();
        let worker = menus.clone();
        let worker_id = id.clone();
        let worker_nodes = nodes.clone();
        let clicks = std::thread::spawn(move || {
            worker.node(&worker_id, &worker_nodes[0]).unwrap();
            worker.patch_check(&worker_id, &worker_nodes[0], true).unwrap();
            worker.get_nodes(&worker_id, "root", CollectionKind::Children, None).unwrap();
            let page = worker.get_nodes(&worker_id, &worker_nodes[1], CollectionKind::Children, None).unwrap();
            finished.send(page).unwrap();
        });
        // Release even on failure so a regression cannot leave blocked threads.
        let ready = response.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        let page = load.join().unwrap().unwrap();
        clicks.join().unwrap();
        assert_eq!(ready.expect("same-session clicks waited for file loading")["items"].as_array().unwrap().len(), 1);
        assert!(menus.node(&id, &nodes[0]).unwrap().check.unwrap());
        assert_eq!(page["items"].as_array().unwrap().len(), 5);
        assert_eq!(page["hasMore"], true);
        let rest = menus.get_nodes_with_loader(&id, &nodes[0], CollectionKind::Children, Some("more"), |_| {
            panic!("continuation must reuse the loaded children");
        }).unwrap();
        assert_eq!(rest["items"].as_array().unwrap().len(), 1);
        assert_eq!(rest["hasMore"], false);
    }

    #[test]
    fn menu_lifetime_file_reload_and_close_reject_late_commits() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..6 {
            std::fs::write(dir.path().join(format!("{index}.txt")), b"x").unwrap();
        }
        let menus = Arc::new(MenuSessions::default());
        let (id, nodes) = file_session(&menus, &[dir.path().to_path_buf()]);
        let node = nodes[0].clone();
        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let worker = menus.clone();
        let worker_id = id.clone();
        let worker_node = node.clone();
        let old = std::thread::spawn(move || {
            worker.get_nodes_with_loader(&worker_id, &worker_node, CollectionKind::Children, None, |load| {
                let loaded = load.materialize();
                entered.send(loaded.built.iter().map(|node| node.id.clone()).collect::<Vec<_>>()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                loaded
            })
        });
        let old_ids = started.recv_timeout(Duration::from_secs(5)).unwrap();
        std::fs::remove_file(dir.path().join("0.txt")).unwrap();
        std::fs::write(dir.path().join("new.txt"), b"new").unwrap();
        let newer = menus.get_nodes(&id, &node, CollectionKind::Children, None).unwrap();
        let committed = menus.node(&id, &node).unwrap();
        release.send(()).unwrap();
        assert_eq!(old.join().unwrap().unwrap_err(), "Node load superseded");
        let retained = menus.node(&id, &node).unwrap();
        assert_eq!(retained.child_ids, committed.child_ids);
        assert_eq!(retained.served, committed.served);
        assert_eq!(newer["items"].as_array().unwrap().len(), 5);
        assert_eq!(retained.served, 5);
        for old_id in old_ids {
            assert!(menus.node(&id, &old_id).is_err(), "superseded load inserted an orphan node");
        }
        assert_eq!(menus.get_nodes(&id, &node, CollectionKind::Children, Some("more"))
            .unwrap()["items"].as_array().unwrap().len(), 1);

        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let worker = menus.clone();
        let worker_id = id.clone();
        let closing = std::thread::spawn(move || {
            worker.get_nodes_with_loader(&worker_id, &node, CollectionKind::Children, None, |load| {
                let loaded = load.materialize();
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                loaded
            })
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(menus.close(&id));
        assert!(menus.list()["sessions"].as_array().unwrap().is_empty());
        release.send(()).unwrap();
        assert_eq!(closing.join().unwrap().unwrap_err(), "Session not found");
        assert!(menus.get_nodes(&id, "root", CollectionKind::Children, None).is_err());
    }

    #[test]
    fn closing_a_loading_session_is_immediate_and_discards_its_late_result() {
        let menus = Arc::new(MenuSessions::default());
        let created = menus.create_from_context("text", json!({}), &MenuBuildContext::default());
        let id = created["sessionId"].as_str().unwrap().to_string();
        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let worker = menus.clone();
        let worker_id = id.clone();
        let load = std::thread::spawn(move || {
            worker.with_session(&worker_id, |_| {
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(json!({"late": true}))
            })
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        // These operations must all finish while the original loader is held.
        assert_eq!(menus.list()["sessions"].as_array().unwrap().len(), 1);
        let other = menus.create_from_context("text", json!({}), &MenuBuildContext::default());
        assert!(menus
            .get_nodes(
                other["sessionId"].as_str().unwrap(),
                "root",
                CollectionKind::Children,
                None
            )
            .is_ok());
        assert!(menus.close(&id));
        assert!(!menus.close(&id));
        release.send(()).unwrap();
        assert_eq!(load.join().unwrap().unwrap_err(), "Session not found");
        assert_eq!(menus.list()["sessions"].as_array().unwrap().len(), 1);
        assert!(menus
            .get_nodes(&id, "root", CollectionKind::Children, None)
            .is_err());
    }
}

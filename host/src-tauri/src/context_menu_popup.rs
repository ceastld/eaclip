//! A retained, bounded menu WebView. Business callbacks stay in the source page.
use cliphost_core::HostState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Mutex};
use tauri::Manager;

pub const LABEL: &str = "context-menu";

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRequest {
    session_id: String,
    document_id: String,
    point: [f64; 2],
    entries: Vec<Value>,
    theme: String,
    aria_label: String,
}

impl OpenRequest {
    fn action_keys(&self) -> Result<HashSet<String>, String> {
        if self.session_id.is_empty() || self.session_id.len() > 128
            || self.document_id.is_empty() || self.document_id.len() > 128
            || !self.point.iter().all(|value| value.is_finite())
            || !matches!(self.theme.as_str(), "dark" | "light")
            || self.aria_label.len() > 512 || self.entries.is_empty() || self.entries.len() > 128
        { return Err("Invalid context menu request".into()); }
        let mut keys = HashSet::new();
        for entry in &self.entries {
            let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("");
            let key = entry.get("key").and_then(Value::as_str).unwrap_or("");
            if key.is_empty() || key.len() > 256 || !matches!(kind, "action" | "separator" | "label") {
                return Err("Invalid context menu entry".into());
            }
            if kind != "separator" && !entry.get("label").and_then(Value::as_str)
                .is_some_and(|label| !label.is_empty() && label.len() <= 8192)
            { return Err("Invalid context menu label".into()); }
            if kind == "action" && !keys.insert(key.to_owned()) {
                return Err("Duplicate context menu action key".into());
            }
            if entry.get("onClick").is_some() { return Err("Context menu callbacks must stay in their source page".into()); }
        }
        Ok(keys)
    }
}

#[derive(Clone, Debug)]
struct Ticket {
    id: u64,
    session: String,
    document: String,
    owner: isize,
    keys: HashSet<String>,
    shown: bool,
}

#[derive(Default)]
struct SessionBook {
    next: u64,
    current: Option<Ticket>,
    // A source document may never recycle an issued session ID: delayed choose,
    // close and layout messages have no authority over a later menu.
    issued: HashSet<String>,
    preclosed: HashSet<(isize, String)>,
}

impl SessionBook {
    fn admissible(&self, request: &OpenRequest, owner: isize) -> Result<(), String> {
        if self.preclosed.contains(&(owner, request.session_id.clone())) {
            return Err("Context menu was closed before opening".into());
        }
        if self.current.as_ref().is_some_and(|ticket| ticket.session == request.session_id
            && ticket.document == request.document_id && ticket.owner == owner) { return Ok(()); }
        if self.issued.contains(&request.session_id) { return Err("Context menu session was already retired".into()); }
        Ok(())
    }
    fn preclose(&mut self, owner: isize, session: &str) {
        self.preclosed.insert((owner, session.to_owned()));
    }
    fn begin(&mut self, request: &OpenRequest, owner: isize) -> Result<Ticket, String> {
        let keys = request.action_keys()?;
        self.admissible(request, owner)?;
        if let Some(ticket) = self.current.as_ref().filter(|ticket|
            ticket.session == request.session_id && ticket.document == request.document_id && ticket.owner == owner)
        { return Ok(ticket.clone()); }
        if !self.issued.insert(request.session_id.clone()) { return Err("Context menu session was already retired".into()); }
        self.next = self.next.wrapping_add(1).max(1);
        let ticket = Ticket { id: self.next, session: request.session_id.clone(), document: request.document_id.clone(),
            owner, keys, shown: false };
        self.current = Some(ticket.clone());
        Ok(ticket)
    }
    fn take(&mut self, id: u64) -> Option<Ticket> {
        if self.current.as_ref().is_some_and(|ticket| ticket.id == id) { self.current.take() } else { None }
    }
    fn select(&self, session: &str, key: &str) -> Option<u64> {
        self.current.as_ref().filter(|ticket| ticket.session == session && ticket.shown && ticket.keys.contains(key))
            .map(|ticket| ticket.id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScreenArea { left: i32, top: i32, width: i32, height: i32, scale_x: f64, scale_y: f64 }
#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds { x: i32, y: i32, width: i32, height: i32 }

fn menu_bounds(point: (i32, i32), area: ScreenArea, width_css: f64, height_css: f64) -> Result<Bounds, String> {
    if ![width_css, height_css, area.scale_x, area.scale_y].iter().all(|value| value.is_finite() && *value > 0.0)
        || width_css > 1_000_000.0 || height_css > 1_000_000.0
        || area.scale_x > 16.0 || area.scale_y > 16.0 || area.width <= 0 || area.height <= 0
    { return Err("Invalid context menu dimensions".into()); }
    let width = (width_css * area.scale_x).ceil().clamp(1.0, area.width as f64) as i32;
    let height = (height_css * area.scale_y).ceil().clamp(1.0, area.height as f64) as i32;
    // The renderer's menu starts 8 CSS px inside its transparent shadow margin.
    Ok(Bounds {
        x: (point.0 - (8.0 * area.scale_x).round() as i32).clamp(area.left, area.left + area.width - width),
        y: (point.1 - (8.0 * area.scale_y).round() as i32).clamp(area.top, area.top + area.height - height),
        width, height,
    })
}

#[derive(Clone)]
struct Presentation {
    ticket: Ticket,
    source: tauri::WebviewWindow,
    request: OpenRequest,
    point: (i32, i32),
    area: ScreenArea,
    opened_at: std::time::Instant,
}

#[derive(Default)]
struct State {
    book: SessionBook,
    presentation: Option<Presentation>,
    ready: bool,
    building: bool,
    preload_scheduled: bool,
    load_deadline: Option<std::time::Instant>,
}

impl State {
    fn renderer_ready(&mut self) -> bool {
        if self.ready { return false; }
        self.ready = true;
        true
    }

    fn wait_for_fit(&mut self, id: u64, now: std::time::Instant) {
        if self.book.current.as_ref().is_some_and(|ticket| ticket.id == id && !ticket.shown) {
            self.load_deadline = Some(now + std::time::Duration::from_secs(15));
        }
    }

    fn fitted(&mut self, id: u64) {
        if let Some(ticket) = self.book.current.as_mut().filter(|ticket| ticket.id == id) {
            ticket.shown = true;
            self.ready = true;
            self.load_deadline = None;
        }
    }

    fn fit_stalled(&self, now: std::time::Instant) -> bool {
        self.book.current.as_ref().is_some_and(|ticket| !ticket.shown)
            && self.load_deadline.is_some_and(|deadline| now >= deadline)
    }
}

#[derive(Default)]
pub struct Service(Mutex<State>);

impl Service {
    fn presentation(&self) -> Option<Presentation> { self.0.lock().unwrap().presentation.clone() }

    pub fn schedule_preload(&self) -> bool {
        let mut state = self.0.lock().unwrap();
        if state.preload_scheduled { return false; }
        state.preload_scheduled = true;
        true
    }

    /// Warm the hidden renderer without opening a session or acquiring an owner lease.
    pub fn preload(&self, app: &tauri::AppHandle) -> Result<(), String> {
        #[cfg(windows)]
        {
            let started = std::time::Instant::now();
            tracing::info!("Native context menu preload started");
            let result = self.ensure_window(app);
            tracing::info!(elapsed_ms = started.elapsed().as_millis(), success = result.is_ok(),
                ready = self.0.lock().unwrap().ready, "Native context menu preload completed");
            result
        }
        #[cfg(not(windows))]
        { let _ = app; Ok(()) }
    }

    pub fn open(&self, app: &tauri::AppHandle, host: &HostState, source: tauri::WebviewWindow,
        request: OpenRequest) -> Result<(), String> {
        #[cfg(not(windows))]
        { let _ = (app, host, source, request); return Err("Native context menus are unavailable".into()); }
        #[cfg(windows)]
        {
            let opened_at = std::time::Instant::now();
            tracing::info!(owner = source.label(), warm_window = app.get_webview_window(LABEL).is_some(),
                "Native context menu open requested");
            if !matches!(source.label(), "clipboard" | "panel" | "main" | "copy-toast") {
                return Err("This window cannot own a context menu".into());
            }
            request.action_keys()?;
            let owner = source.hwnd().map_err(|error| error.to_string())?.0 as isize;
            if !source_is_current(app, &source, owner) || !source.is_visible().unwrap_or(false) {
                return Err("Context menu source is unavailable".into());
            }
            let scale = source.scale_factor().map_err(|error| error.to_string())?;
            let point = client_point(owner, request.point, scale)?;
            let work = cliphost_core::window_place::overlay_work_area_from_point(point.0, point.1)
                .ok_or("Context menu screen is unavailable")?;
            let area = ScreenArea { left: work.work.left.round() as i32, top: work.work.top.round() as i32,
                width: work.work.width.round() as i32, height: work.work.height.round() as i32,
                scale_x: work.dpi_scale_x, scale_y: work.dpi_scale_y };
            menu_bounds(point, area, 1.0, 1.0)?;
            // Async invokes may enqueue close before open. Reject retired work
            // before it can dismiss a newer menu owned by another request.
            self.0.lock().unwrap().book.admissible(&request, owner)?;
            if let Some(previous) = self.presentation() {
                if previous.ticket.session == request.session_id && previous.ticket.document == request.document_id
                    && previous.ticket.owner == owner && host.native_context_menu_is_current(previous.ticket.id)
                { return Ok(()); }
                self.dismiss(app, host, previous.ticket.id, "dismiss", None);
            }
            let ticket = self.0.lock().unwrap().book.begin(&request, owner)?;
            if !host.acquire_native_context_menu(ticket.id, source.label()) {
                self.0.lock().unwrap().book.take(ticket.id);
                return Err("Context menu owner no longer requests display".into());
            }
            self.0.lock().unwrap().presentation = Some(Presentation { ticket: ticket.clone(), source, request, point, area, opened_at });
            if let Err(error) = self.ensure_window(app) {
                self.dismiss(app, host, ticket.id, "fallback", None);
                return Err(error);
            }
            {
                let mut state = self.0.lock().unwrap();
                state.wait_for_fit(ticket.id, std::time::Instant::now());
            }
            if let Err(error) = self.publish(app, host) {
                self.dismiss(app, host, ticket.id, "fallback", None);
                return Err(error);
            }
            tracing::info!(ticket = ticket.id, elapsed_ms = opened_at.elapsed().as_millis(),
                "Native context menu open queued for fit");
            Ok(())
        }
    }

    #[cfg(windows)]
    fn ensure_window(&self, app: &tauri::AppHandle) -> Result<(), String> {
        if app.get_webview_window(LABEL).is_some() { return Ok(()); }
        {
            let mut state = self.0.lock().unwrap();
            if state.building { return Ok(()); }
            state.building = true;
            state.ready = false;
        }
        let started = std::time::Instant::now();
        tracing::info!("Native context menu WebView creation started");
        let result = (|| {
            let mut config = app.config().app.windows.iter().find(|config| config.label == LABEL)
                .ok_or("Context menu configuration is unavailable")?.clone();
            config.visible = false;
            let loading = app.state::<crate::WindowLoading>();
            if let Some(base) = loading.vite_base.as_deref() {
                config.url = tauri::WebviewUrl::External(format!("{base}/overlay/context-menu").parse().map_err(|error: url::ParseError| error.to_string())?);
            }
            let window = tauri::WebviewWindowBuilder::from_config(app, &config).map_err(|error| error.to_string())?
                .focused(false).focusable(false).skip_taskbar(true).shadow(false)
                .initialization_script(crate::boot_script(loading.api_port, "/overlay/context-menu", LABEL))
                .initialization_script(include_str!("context-menu-popup.js"))
                .build().map_err(|error| error.to_string())?;
            let hwnd = window.hwnd().map_err(|error| error.to_string())?;
            crate::tool_window::configure(windows::Win32::Foundation::HWND(hwnd.0), LABEL);
            install_no_activate(hwnd.0 as isize)?;
            Ok(())
        })();
        self.0.lock().unwrap().building = false;
        tracing::info!(elapsed_ms = started.elapsed().as_millis(), success = result.is_ok(),
            "Native context menu WebView creation completed");
        result
    }

    fn source_valid(&self, app: &tauri::AppHandle, host: &HostState, presentation: &Presentation) -> bool {
        host.native_context_menu_is_current(presentation.ticket.id)
            && source_is_current(app, &presentation.source, presentation.ticket.owner)
            && presentation.source.is_visible().unwrap_or(false)
    }

    fn publish(&self, app: &tauri::AppHandle, host: &HostState) -> Result<(), String> {
        let Some(presentation) = self.presentation() else { return Ok(()); };
        if !self.source_valid(app, host, &presentation) {
            self.dismiss(app, host, presentation.ticket.id, "dismiss", None);
            return Ok(());
        }
        let Some(window) = app.get_webview_window(LABEL) else { return Ok(()); };
        #[cfg(windows)]
        {
            let bounds = menu_bounds(presentation.point, presentation.area,
                (presentation.area.width as f64 / presentation.area.scale_x).min(436.0),
                presentation.area.height as f64 / presentation.area.scale_y)?;
            let _ = window.hide();
            window.set_position(tauri::PhysicalPosition::new(bounds.x, bounds.y)).map_err(|error| error.to_string())?;
            window.set_size(tauri::PhysicalSize::new(bounds.width as u32, bounds.height as u32)).map_err(|error| error.to_string())?;
            let hwnd = window.hwnd().map_err(|error| error.to_string())?.0 as isize;
            configure_native_popup(hwnd, presentation.ticket.owner, bounds, false)?;
            host.set_native_context_menu_hwnd(presentation.ticket.id, hwnd);
        }
        let mut detail = serde_json::to_value(&presentation.request).map_err(|error| error.to_string())?;
        detail["workAreaWidthCss"] = json!(presentation.area.width as f64 / presentation.area.scale_x);
        detail["workAreaHeightCss"] = json!(presentation.area.height as f64 / presentation.area.scale_y);
        // Document-start and React readiness are independent. Retain only the
        // newest payload; ready() publishes it after the renderer installs its API.
        let result = window.eval(&format!("(()=>{{const d={detail};window.__clipHostContextMenuPending=d;if(typeof window.__clipHostContextMenuState==='function')window.__clipHostContextMenuState(d);}})();"))
            .map_err(|error| error.to_string());
        tracing::info!(ticket = presentation.ticket.id, elapsed_ms = presentation.opened_at.elapsed().as_millis(),
            success = result.is_ok(), "Native context menu state published");
        result
    }

    pub fn ready(&self, app: &tauri::AppHandle, host: &HostState, window: &tauri::WebviewWindow) -> Result<(), String> {
        if !popup_is_current(app, window) { return Err("Context menu renderer is unavailable".into()); }
        {
            let mut state = self.0.lock().unwrap();
            if !state.renderer_ready() { return Ok(()); }
        }
        tracing::info!(has_session = self.presentation().is_some(), "Native context menu renderer ready");
        let result = self.publish(app, host);
        if result.is_err() {
            if let Some(show) = self.presentation() { self.dismiss(app, host, show.ticket.id, "fallback", None); }
        }
        result
    }

    pub fn resize(&self, app: &tauri::AppHandle, host: &HostState, window: &tauri::WebviewWindow,
        session: &str, width: f64, height: f64) -> Result<(), String> {
        if !popup_is_current(app, window) { return Err("Context menu renderer is unavailable".into()); }
        let Some(presentation) = self.presentation().filter(|show| show.ticket.session == session) else { return Ok(()); };
        if !self.source_valid(app, host, &presentation) {
            self.dismiss(app, host, presentation.ticket.id, "dismiss", None);
            return Ok(());
        }
        let bounds = menu_bounds(presentation.point, presentation.area, width, height)?;
        // Separate async invokes can enqueue fit before ready. A valid layout
        // from this renderer/session proves readiness; late ready stays inert.
        {
            let mut state = self.0.lock().unwrap();
            state.renderer_ready();
        }
        window.set_position(tauri::PhysicalPosition::new(bounds.x, bounds.y)).map_err(|error| error.to_string())?;
        window.set_size(tauri::PhysicalSize::new(bounds.width as u32, bounds.height as u32)).map_err(|error| error.to_string())?;
        if !self.source_valid(app, host, &presentation) { return Ok(()); }
        #[cfg(windows)]
        configure_native_popup(window.hwnd().map_err(|error| error.to_string())?.0 as isize,
            presentation.ticket.owner, bounds, true)?;
        if !self.source_valid(app, host, &presentation) {
            self.dismiss(app, host, presentation.ticket.id, "dismiss", None);
            return Ok(());
        }
        window.show().map_err(|error| error.to_string())?;
        #[cfg(windows)]
        configure_native_popup(window.hwnd().map_err(|error| error.to_string())?.0 as isize,
            presentation.ticket.owner, bounds, true)?;
        if !self.source_valid(app, host, &presentation) {
            self.dismiss(app, host, presentation.ticket.id, "dismiss", None);
            return Ok(());
        }
        self.0.lock().unwrap().fitted(presentation.ticket.id);
        tracing::info!(ticket = presentation.ticket.id, elapsed_ms = presentation.opened_at.elapsed().as_millis(),
            width = bounds.width, height = bounds.height, "Native context menu fit revealed");
        Ok(())
    }

    pub fn close_from(&self, app: &tauri::AppHandle, host: &HostState, source: &tauri::WebviewWindow, session: &str) {
        if session.is_empty() || session.len() > 128 { return; }
        #[cfg(windows)]
        if matches!(source.label(), "clipboard" | "panel" | "main" | "copy-toast") {
            if let Ok(hwnd) = source.hwnd() {
                let owner = hwnd.0 as isize;
                if source_is_current(app, source, owner) { self.0.lock().unwrap().book.preclose(owner, session); }
            }
        }
        let Some(presentation) = self.presentation().filter(|show| show.ticket.session == session) else { return; };
        if popup_is_current(app, source) || (source_is_current(app, source, presentation.ticket.owner)
            && source.label() == presentation.source.label()) {
            self.dismiss(app, host, presentation.ticket.id, "dismiss", None);
        }
    }

    pub fn select(&self, app: &tauri::AppHandle, host: &HostState, popup: &tauri::WebviewWindow, session: &str, key: &str) -> Result<(), String> {
        if !popup_is_current(app, popup) { return Err("Context menu renderer is unavailable".into()); }
        let selected = self.0.lock().unwrap().book.select(session, key);
        if let Some(id) = selected {
            if self.presentation().is_some_and(|show| self.source_valid(app, host, &show)) {
                self.dismiss(app, host, id, "select", Some(key));
            } else { self.dismiss(app, host, id, "dismiss", None); }
        }
        Ok(())
    }

    pub fn dismiss(&self, app: &tauri::AppHandle, host: &HostState, id: u64, kind: &str, key: Option<&str>) {
        let presentation = {
            let mut state = self.0.lock().unwrap();
            if state.book.take(id).is_none() { return; }
            state.load_deadline = None;
            state.presentation.take()
        };
        tracing::info!(ticket = id, reason = kind,
            elapsed_ms = presentation.as_ref().map(|show| show.opened_at.elapsed().as_millis()),
            "Native context menu dismissed");
        host.release_native_context_menu(id);
        if let Some(window) = app.get_webview_window(LABEL) {
            let _ = window.hide();
            #[cfg(windows)]
            if let Ok(hwnd) = window.hwnd() { detach_owner(hwnd.0 as isize); }
            let _ = window.eval("window.__clipHostContextMenuPending=null;if(typeof window.__clipHostContextMenuState==='function')window.__clipHostContextMenuState(null);");
        }
        if let Some(show) = presentation.filter(|show| source_is_current(app, &show.source, show.ticket.owner)) {
            let mut detail = json!({ "sessionId": show.ticket.session, "documentId": show.ticket.document, "type": kind });
            if let Some(key) = key { detail["key"] = json!(key); }
            let _ = show.source.eval(&format!("if(typeof window.__clipHostContextMenuDeliver==='function')window.__clipHostContextMenuDeliver({detail});"));
        }
    }

    pub fn close_owner(&self, app: &tauri::AppHandle, host: &HostState, label: &str) {
        if let Some(show) = self.presentation().filter(|show| show.source.label() == label) {
            self.dismiss(app, host, show.ticket.id, "dismiss", None);
        }
    }

    pub fn document_started(&self, app: &tauri::AppHandle, host: &HostState, label: &str) {
        if label == LABEL {
            let was_ready = {
                let mut state = self.0.lock().unwrap();
                let was_ready = state.ready;
                state.ready = false;
                was_ready
            };
            // The initial navigation belongs to the opening session. A later
            // reload retires the old renderer and falls back once.
            if was_ready {
                if let Some(show) = self.presentation() { self.dismiss(app, host, show.ticket.id, "fallback", None); }
            }
        } else { self.close_owner(app, host, label); }
    }

    pub fn renderer_destroyed(&self, app: &tauri::AppHandle, host: &HostState) {
        self.0.lock().unwrap().ready = false;
        if let Some(show) = self.presentation() { self.dismiss(app, host, show.ticket.id, "fallback", None); }
    }

    pub fn fit_failed(&self, app: &tauri::AppHandle, host: &HostState, popup: &tauri::WebviewWindow, session: &str) {
        if !popup_is_current(app, popup) { return; }
        if let Some(show) = self.presentation().filter(|show| show.ticket.session == session) {
            self.dismiss(app, host, show.ticket.id, "fallback", None);
        }
    }

    pub fn validate_owner(&self, app: &tauri::AppHandle, host: &HostState) {
        if let Some(show) = self.presentation() {
            if !self.source_valid(app, host, &show) { self.dismiss(app, host, show.ticket.id, "dismiss", None); }
            else {
                let timed_out = {
                    let state = self.0.lock().unwrap();
                    state.fit_stalled(std::time::Instant::now())
                };
                if timed_out { self.dismiss(app, host, show.ticket.id, "fallback", None); }
            }
        }
    }
}

fn source_is_current(app: &tauri::AppHandle, window: &tauri::WebviewWindow, expected: isize) -> bool {
    #[cfg(windows)]
    { app.get_webview_window(window.label()).and_then(|current| current.hwnd().ok()).is_some_and(|hwnd|
        hwnd.0 as isize == expected && window.hwnd().ok().is_some_and(|original| original.0 as isize == expected)
            && unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(windows::Win32::Foundation::HWND(hwnd.0)).as_bool() }) }
    #[cfg(not(windows))]
    { let _ = (app, window, expected); false }
}

fn popup_is_current(app: &tauri::AppHandle, window: &tauri::WebviewWindow) -> bool {
    #[cfg(windows)]
    { window.label() == LABEL && window.hwnd().ok().is_some_and(|hwnd| source_is_current(app, window, hwnd.0 as isize)) }
    #[cfg(not(windows))]
    { let _ = (app, window); false }
}

#[cfg(windows)]
fn client_point(owner: isize, point: [f64; 2], scale: f64) -> Result<(i32, i32), String> {
    use windows::Win32::{Foundation::{HWND, POINT, RECT}, Graphics::Gdi::ClientToScreen, UI::WindowsAndMessaging::GetClientRect};
    if !scale.is_finite() || !(0.0..=16.0).contains(&scale) || scale == 0.0 || !point.iter().all(|value| value.is_finite()) {
        return Err("Invalid context menu anchor".into());
    }
    unsafe {
        let hwnd = HWND(owner as *mut _);
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect).map_err(|error| error.to_string())?;
        let mut anchor = POINT { x: (point[0] * scale).round() as i32, y: (point[1] * scale).round() as i32 };
        if point[0] < 0.0 || point[1] < 0.0 || anchor.x > rect.right || anchor.y > rect.bottom {
            return Err("Context menu anchor is outside its source".into());
        }
        if !ClientToScreen(hwnd, &mut anchor).as_bool() { return Err("Context menu source is unavailable".into()); }
        Ok((anchor.x, anchor.y))
    }
}

#[cfg(windows)]
fn install_no_activate(hwnd: isize) -> Result<(), String> {
    use windows::Win32::{Foundation::HWND, UI::Shell::SetWindowSubclass};
    unsafe { SetWindowSubclass(HWND(hwnd as *mut _), Some(popup_proc), 174, 0).as_bool()
        .then_some(()).ok_or_else(|| "Cannot protect context menu activation".into()) }
}

#[cfg(windows)]
unsafe extern "system" fn popup_proc(hwnd: windows::Win32::Foundation::HWND, message: u32,
    wp: windows::Win32::Foundation::WPARAM, lp: windows::Win32::Foundation::LPARAM, id: usize, _: usize) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::{Foundation::LRESULT, UI::{Shell::{DefSubclassProc, RemoveWindowSubclass}, WindowsAndMessaging::*}};
    if message == WM_MOUSEACTIVATE { return LRESULT(MA_NOACTIVATE as isize); }
    if message == WM_STYLECHANGING && wp.0 as isize == GWL_EXSTYLE.0 as isize && lp.0 != 0 {
        let styles = &mut *(lp.0 as *mut STYLESTRUCT);
        styles.styleNew = (styles.styleNew | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0;
    }
    if message == WM_NCDESTROY { let _ = RemoveWindowSubclass(hwnd, Some(popup_proc), id); }
    DefSubclassProc(hwnd, message, wp, lp)
}

#[cfg(windows)]
fn configure_native_popup(hwnd: isize, owner: isize, bounds: Bounds, reveal: bool) -> Result<(), String> {
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
    unsafe {
        let window = HWND(hwnd as *mut _);
        let source = HWND(owner as *mut _);
        if !IsWindow(window).as_bool() || !IsWindow(source).as_bool() || window == source
            || (reveal && !IsWindowVisible(source).as_bool())
        { return Err("Context menu owner is unavailable".into()); }
        SetWindowLongPtrW(window, GWLP_HWNDPARENT, owner);
        let ex = GetWindowLongPtrW(window, GWL_EXSTYLE);
        SetWindowLongPtrW(window, GWL_EXSTYLE, (ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize)
            & !(WS_EX_APPWINDOW.0 as isize | WS_EX_TRANSPARENT.0 as isize));
        SetWindowPos(window, HWND_TOPMOST, bounds.x, bounds.y, bounds.width, bounds.height,
            SWP_NOACTIVATE | SWP_NOOWNERZORDER | if reveal { SWP_SHOWWINDOW } else { SWP_NOSENDCHANGING })
            .map_err(|error| error.to_string())
    }
}

#[cfg(windows)]
fn detach_owner(hwnd: isize) {
    unsafe { windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
        windows::Win32::Foundation::HWND(hwnd as *mut _), windows::Win32::UI::WindowsAndMessaging::GWLP_HWNDPARENT, 0); }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(session: &str, document: &str) -> OpenRequest {
        serde_json::from_value(json!({ "sessionId": session, "documentId": document, "point": [30.5, 10.25],
            "entries": [{"kind":"action", "key":"copy", "label":"复制"}], "theme":"dark", "ariaLabel":"菜单" })).unwrap()
    }

    #[test]
    fn native_menu_tickets_reject_stale_close_fit_select_and_duplicate_choose() {
        let mut book = SessionBook::default();
        let first = book.begin(&request("first", "doc1"), 101).unwrap();
        assert_eq!(book.begin(&request("first", "doc1"), 101).unwrap().id, first.id);
        assert!(book.select("first", "copy").is_none(), "hidden measurement cannot execute actions");
        let second = book.begin(&request("second", "doc2"), 102).unwrap();
        book.current.as_mut().unwrap().shown = true;
        assert!(book.take(first.id).is_none());
        assert!(book.select("first", "copy").is_none());
        assert!(book.select("second", "delete").is_none());
        assert_eq!(book.select("second", "copy"), Some(second.id));
        assert!(book.take(second.id).is_some());
        assert!(book.take(second.id).is_none());
        assert!(book.select("second", "copy").is_none());
        assert!(book.begin(&request("first", "doc3"), 103).is_err(), "retired ticket cannot be reused by a new document");
    }

    #[test]
    fn native_menu_close_before_open_cannot_revive_or_dismiss_a_newer_session() {
        let mut book = SessionBook::default();
        book.preclose(101, "canceled");
        let current = book.begin(&request("current", "doc"), 101).unwrap();
        assert!(book.admissible(&request("canceled", "doc"), 101).is_err());
        assert!(book.begin(&request("canceled", "doc"), 101).is_err());
        assert_eq!(book.current.as_ref().unwrap().id, current.id);
        // Unknown closes have source HWND scope and cannot retire another source.
        book.preclose(102, "other-source");
        assert!(book.admissible(&request("other-source", "doc"), 101).is_ok());
    }

    #[test]
    fn native_menu_preload_ready_before_open_preserves_first_fit_deadline_and_late_ready_is_inert() {
        let service = Service::default();
        assert!(service.schedule_preload());
        assert!(!service.schedule_preload(), "primary reloads do not queue more warmups");
        let mut state = service.0.lock().unwrap();
        assert!(state.book.current.is_none());
        assert!(state.presentation.is_none());
        assert!(state.load_deadline.is_none(), "warming an idle page has no show deadline or lease ticket");
        assert!(state.renderer_ready(), "a preloaded renderer can be ready before its first owner");
        let opened = std::time::Instant::now();
        let ticket = state.book.begin(&request("warm", "doc"), 101).unwrap();
        state.wait_for_fit(ticket.id, opened);
        assert!(!state.renderer_ready(), "open after warm ready does not republish or hide the page");
        assert!(!state.fit_stalled(opened));
        assert!(state.fit_stalled(opened + std::time::Duration::from_secs(15)),
            "ready without layout still falls back instead of waiting invisibly forever");
        state.fitted(ticket.id);
        assert!(state.book.current.as_ref().unwrap().shown);
        assert!(state.load_deadline.is_none());
        assert!(!state.renderer_ready(), "late duplicate ready cannot hide a fitted menu");
        assert!(!state.fit_stalled(opened + std::time::Duration::from_secs(30)));
    }

    #[test]
    fn native_menu_ready_during_open_and_stale_fit_do_not_clear_a_newer_show_deadline() {
        let mut state = State::default();
        let opened = std::time::Instant::now();
        let first = state.book.begin(&request("cold", "doc"), 101).unwrap();
        state.wait_for_fit(first.id, opened);
        assert!(state.renderer_ready());
        assert!(state.fit_stalled(opened + std::time::Duration::from_secs(15)),
            "the ready handshake is not proof of fit/reveal");
        let second = state.book.begin(&request("next", "doc"), 101).unwrap();
        state.wait_for_fit(second.id, opened);
        state.fitted(first.id);
        assert!(!state.book.current.as_ref().unwrap().shown);
        assert!(state.fit_stalled(opened + std::time::Duration::from_secs(15)));
        state.fitted(second.id);
        assert!(!state.fit_stalled(opened + std::time::Duration::from_secs(15)));
    }

    #[test]
    fn native_menu_bounds_preserve_natural_size_and_limit_to_destination_work_area() {
        let area = ScreenArea { left: -1920, top: -200, width: 1920, height: 1040, scale_x: 1.5, scale_y: 1.5 };
        assert_eq!(menu_bounds((-1700, 100), area, 328.0, 222.0).unwrap(), Bounds { x:-1712, y:88, width:492, height:333 });
        assert_eq!(menu_bounds((-5, 830), area, 328.0, 222.0).unwrap(), Bounds { x:-492, y:507, width:492, height:333 });
        assert_eq!(menu_bounds((-1700, 100), area, 4000.0, 2000.0).unwrap(), Bounds { x:-1920, y:-200, width:1920, height:1040 });
        assert!(menu_bounds((0, 0), area, f64::NAN, 100.0).is_err());
        assert!(menu_bounds((0, 0), area, 100.0, -1.0).is_err());
    }

    #[test]
    fn native_menu_rejects_callback_payload_and_duplicate_action_keys() {
        let mut value = request("id", "doc");
        value.entries.push(value.entries[0].clone());
        assert!(value.action_keys().is_err());
        value.entries.pop();
        value.entries[0]["onClick"] = json!("callback");
        assert!(value.action_keys().is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_menu_hidden_fixture_is_owned_no_activate_and_uses_physical_bounds() {
        use windows::{core::w, Win32::{Foundation::{BOOL, HWND, LPARAM, RECT, WPARAM},
            Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK},
            UI::{HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2}, WindowsAndMessaging::*}}};
        struct Window(HWND);
        impl Drop for Window { fn drop(&mut self) { unsafe { if IsWindow(self.0).as_bool() { let _ = DestroyWindow(self.0); } } } }
        unsafe {
            let previous_dpi = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let foreground = GetForegroundWindow();
            let source = Window(CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, w!("STATIC"), w!("menu owner fixture"),
                WS_POPUP, -1700, 100, 400, 300, None, None, None, None).unwrap());
            let popup = Window(CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, w!("STATIC"), w!("menu fixture"),
                WS_POPUP, 0, 0, 1, 1, None, None, None, None).unwrap());
            install_no_activate(popup.0.0 as isize).unwrap();
            let point = client_point(source.0.0 as isize, [20.5, 10.25], 1.5).unwrap();
            assert_eq!(point, (-1669, 115));
            let area = ScreenArea { left:-1920, top:-200, width:1920, height:1040, scale_x:1.5, scale_y:1.5 };
            let bounds = menu_bounds(point, area, 328.0, 222.0).unwrap();
            configure_native_popup(popup.0.0 as isize, source.0.0 as isize, bounds, false).unwrap();
            assert_eq!(GetWindow(popup.0, GW_OWNER).unwrap(), source.0);
            assert_eq!(SendMessageW(popup.0, WM_MOUSEACTIVATE, WPARAM(source.0.0 as usize), LPARAM(0)).0, MA_NOACTIVATE as isize);
            let ex = GetWindowLongPtrW(popup.0, GWL_EXSTYLE) as u32;
            assert_ne!(ex & WS_EX_NOACTIVATE.0, 0);
            assert_eq!(ex & (WS_EX_APPWINDOW.0 | WS_EX_TRANSPARENT.0), 0);
            let mut actual = RECT::default();
            GetWindowRect(popup.0, &mut actual).unwrap();
            assert_eq!((actual.left, actual.top, actual.right - actual.left, actual.bottom - actual.top),
                (bounds.x, bounds.y, bounds.width, bounds.height));
            assert!(!IsWindowVisible(popup.0).as_bool());
            assert_eq!(GetForegroundWindow(), foreground);
            // Exercise the exact reveal helper without exposing UI or moving
            // the user's input: both owned fixture surfaces are DWM-cloaked.
            let cloak = BOOL(1);
            for hwnd in [source.0, popup.0] {
                DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const _, 4).unwrap();
            }
            let _ = ShowWindow(source.0, SW_SHOWNOACTIVATE);
            configure_native_popup(popup.0.0 as isize, source.0.0 as isize, bounds, true).unwrap();
            assert!(IsWindowVisible(popup.0).as_bool());
            assert_eq!(GetForegroundWindow(), foreground);
            GetWindowRect(popup.0, &mut actual).unwrap();
            assert!(actual.right > -1300, "menu may extend beyond its small owner");
            assert!(actual.left >= area.left && actual.right <= area.left + area.width);
            assert!(actual.top >= area.top && actual.bottom <= area.top + area.height);
            let _ = ShowWindow(source.0, SW_HIDE);
            assert!(configure_native_popup(popup.0.0 as isize, source.0.0 as isize, bounds, true).is_err(),
                "a hidden owner cannot revive its menu");
            DestroyWindow(source.0).unwrap();
            assert!(!IsWindow(popup.0).as_bool(), "owner destruction destroys its popup");
            assert!(configure_native_popup(popup.0.0 as isize, source.0.0 as isize, bounds, false).is_err());
            assert_eq!(GetForegroundWindow(), foreground);
            SetThreadDpiAwarenessContext(previous_dpi);
        }
    }
}

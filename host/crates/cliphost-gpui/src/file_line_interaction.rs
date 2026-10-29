use super::*;
use cliphost_core::host::file_line_host::{FileLineChange, FileLineRemoval};

const UNDO_LIFETIME: Duration = Duration::from_secs(6);
const DRAG_THRESHOLD: f32 = 5.0;
const TRAY_WIDTH: f32 = 180.0;
const TRAY_HEIGHT: f32 = 46.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    Tag(String),
    Sheet(String, Option<usize>),
}

pub(super) struct Gesture {
    pub(super) node: Value,
    pub(super) start: (f32, f32),
    pub point: (f32, f32),
    pub active: bool,
    pub removable: bool,
}

#[derive(Default)]
pub(super) struct Interaction {
    pub gesture: Option<Gesture>,
    pub task: Option<Task<()>>,
    pub undo: Option<(FileLineRemoval, Instant)>,
    pub keyboard_mode: bool,
    pub focus: Option<Focus>,
    last_pointer: Option<(f32, f32)>,
    pub cancelled_press: bool,
    pub suppress_click_until: Option<Instant>,
}

enum Mutation {
    Hang(String, Option<String>),
    Incoming(Vec<String>, Option<String>),
    Reorder(String, Option<String>),
    Remove(String),
    Restore(FileLineRemoval),
}

impl FileLine {
    pub(super) fn expire_undo(&mut self, cx: &mut Context<Self>) {
        if self.interaction.cancelled_press && !left_pressed() {
            self.interaction.cancelled_press = false;
            self.interaction.suppress_click_until = Some(Instant::now() + Duration::from_millis(160));
        }
        if self.interaction.undo.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
            self.interaction.undo = None;
            self.sync_windows(cx);
        }
    }

    fn mutate(&mut self, mutation: Mutation, cx: &mut Context<Self>) {
        if self.interaction.task.is_some() || self.action_task.is_some() { return; }
        let host = self.host.clone();
        let epoch = self.epoch;
        self.hover_task = None;
        self.interaction.task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move {
                match mutation {
                    Mutation::Incoming(paths, before) => {
                        let paths = native_drop::directories(&paths)?;
                        host.file_line_hang_many(&paths, before.as_deref())?;
                        Ok((None, None))
                    }
                    Mutation::Hang(path, before) => host.file_line_hang(&path, before.as_deref())
                        .map(|change| (None, if change == FileLineChange::AlreadyPresent { Some(tr("文件夹已在绳上", "Folder is already on the line").to_string()) } else { None })),
                    Mutation::Reorder(path, before) => host.file_line_reorder(&path, before.as_deref()).map(|_| (None, None)),
                    Mutation::Remove(path) => host.file_line_take_down(&path).map(|undo| (undo, None)),
                    Mutation::Restore(undo) => host.file_line_restore(&undo).map(|_| (None, None)),
                }
            }).await;
            let _ = this.update(cx, |line, cx| {
                line.interaction.task = None;
                if line.epoch != epoch { return; }
                match result {
                    Ok((undo, message)) => {
                        line.interaction.undo = undo.map(|undo| (undo, Instant::now() + UNDO_LIFETIME));
                        line.status = message;
                        line.load_root(cx);
                    }
                    Err(error) => line.status = Some(error),
                }
                line.sync_windows(cx);
            });
        }));
        self.sync_windows(cx);
    }

    pub(super) fn poll_drop(&mut self, cx: &mut Context<Self>) {
        for event in self.native_drop.drain() {
            match event {
                native_drop::Event::Dropped(event) => {
                    let Some(edge)=self.edge else {continue};
                    let x=((event.screen_point.0 as f64-edge.work.left)/edge.dpi_scale) as f32;
                    let y=((event.screen_point.1 as f64-edge.work.top)/edge.dpi_scale) as f32;
                    if self.trigger.phase()!=Phase::Down || y<0.0 || y>BAND_HEIGHT { continue; }
                    let before=self.insert_before(x,None);
                    if event.ticket.verify().is_ok() { self.mutate(Mutation::Incoming(event.formats.files,before),cx); }
                }
                native_drop::Event::Error(error) => { self.status=Some(error.message); cx.notify(); }
                native_drop::Event::Sent { result, .. } => {
                    self.interaction.suppress_click_until=Some(Instant::now()+Duration::from_millis(160));
                    if let Err(error)=result {self.status=Some(error);}
                    cx.notify();
                }
            }
        }
    }

    fn remove_tag(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(tag) = self.tag(id) {
            let path = node_path(&tag.node);
            if self.host.file_menu_contains(&path) {
                self.mutate(Mutation::Remove(path), cx);
            }
        }
    }

    pub(super) fn undo_remove(&mut self, cx: &mut Context<Self>) {
        let Some((undo, until)) = self.interaction.undo.as_ref() else { return };
        if Instant::now() < *until { self.mutate(Mutation::Restore(undo.clone()), cx); }
    }

    fn insert_before(&self, x: f32, skip: Option<&str>) -> Option<String> {
        self.layout().0.iter().find_map(|(index, bounds)| {
            let Slot::Tag(tag) = &self.slots[*index] else { return None };
            let path = node_path(&tag.node);
            (bounds.left + bounds.width / 2.0 >= x
                && !skip.is_some_and(|skip| path.eq_ignore_ascii_case(skip))
                && self.host.file_menu_contains(&path)).then_some(path)
        })
    }

    pub(super) fn begin_gesture(&mut self, node: Value, root_tag: bool, cx: &mut Context<Self>) {
        if self.action_task.is_some() || self.interaction.task.is_some() || !enabled(&node) { return; }
        let path = node_path(&node);
        if path.is_empty() { return; }
        let Some((x,y)) = platform::cursor_position() else { return };
        let Some(edge) = self.edge else { return };
        let point = (((x as f64-edge.work.left)/edge.dpi_scale) as f32, ((y as f64-edge.work.top)/edge.dpi_scale) as f32);
        self.interaction.gesture = Some(Gesture { removable: root_tag && self.host.file_menu_contains(&path), node, start: point, point, active: false });
        self.hover_task = None;
        cx.notify();
    }

    pub(super) fn ghost_rect(&self, point: (f32,f32)) -> (f32,f32,f32,f32) {
        ((point.0+12.0).min(self.width()-240.0).max(4.0), point.1+15.0, 240.0, 30.0)
    }

    pub(super) fn tray_rect(&self) -> (f32, f32, f32, f32) {
        ((self.width()-196.0).max(8.0), BAND_HEIGHT+16.0, TRAY_WIDTH, TRAY_HEIGHT)
    }

    pub(super) fn poll_gesture(&mut self, point: (f32, f32), cx: &mut Context<Self>) -> bool {
        let Some(mut gesture) = self.interaction.gesture.take() else { return false };
        gesture.point = point;
        if self.suspended { self.sync_windows(cx); return false; }
        let pressed = left_pressed();
        if !gesture.active && distance(gesture.start, point) >= DRAG_THRESHOLD {
            if !expandable(&gesture.node) && !gesture.removable {
                self.host.clear_native_file_line_keyboard(self.epoch);
                match self.native_drop.start_files(&self.host, vec![node_path(&gesture.node)]) {
                    Ok(()) => self.interaction.suppress_click_until = Some(Instant::now() + Duration::from_millis(160)),
                    Err(error) => self.status = Some(error),
                }
                self.sync_windows(cx);
                return true;
            }
            gesture.active = true;
            self.hover_task = None;
            self.sheets.retain(|sheet| sheet.pinned);
        }
        if gesture.active {
            self.interaction.suppress_click_until = Some(Instant::now() + Duration::from_millis(160));
        }
        if pressed {
            let active = gesture.active;
            self.interaction.gesture = Some(gesture);
            if active { self.sync_windows(cx); }
            return active;
        }
        if !gesture.active { return false; }
        let path = node_path(&gesture.node);
        if gesture.removable && contains(self.tray_rect(), point, 6.0) {
            self.mutate(Mutation::Remove(path), cx);
        } else if point.1 >= 0.0 && point.1 <= BAND_HEIGHT && point.0 >= 0.0 && point.0 < self.width() {
            let before = self.insert_before(point.0, Some(&path));
            if gesture.removable { self.mutate(Mutation::Reorder(path, before), cx); }
            else if expandable(&gesture.node) { self.mutate(Mutation::Hang(path, before), cx); }
            else { self.status = Some(tr("绳上只挂文件夹", "Only folders can be hung on the line").into()); }
        } else if !self.in_zone(point) && expandable(&gesture.node) && pointer_over_target(self.target) {
            // Folder delivery uses the same captured target as a primary click.
            self.invoke(gesture.node, false, cx);
        }
        self.sync_windows(cx);
        true
    }

    pub(super) fn click_is_suppressed(&self) -> bool {
        self.interaction.cancelled_press || self.interaction.suppress_click_until.is_some_and(|until| Instant::now() < until)
    }

    pub(super) fn focus_is_tag(&self, id: &str) -> bool {
        self.interaction.keyboard_mode && self.interaction.focus == Some(Focus::Tag(id.to_string()))
    }
    pub(super) fn focus_is_row(&self, id: &str, row: usize) -> bool {
        self.interaction.keyboard_mode && self.interaction.focus == Some(Focus::Sheet(id.to_string(), Some(row)))
    }

    #[cfg(windows)]
    pub(super) fn poll_keyboard(&mut self, point: (f32, f32), cx: &mut Context<Self>) {
        if self.interaction.last_pointer.is_some_and(|old| distance(old, point)>3.0) {
            self.interaction.keyboard_mode = false;
        }
        self.interaction.last_pointer = Some(point);
        let Some(handle) = self.line_window else { return };
        let Ok(Ok(hwnd)) = handle.update(cx, |_, window, _| platform::hwnd(window).map(|hwnd| hwnd.0 as isize)) else { return };
        let active = !self.suspended && self.line_shown && self.action_task.is_none() && !self.native_drop.outgoing_active()
            && (self.interaction.keyboard_mode || self.interaction.gesture.is_some() || self.regions().iter().any(|rect| contains(*rect, point, 0.0)));
        let Some(edge) = self.edge else { return };
        let regions: Vec<_> = self.regions().iter().map(|(x,y,w,h)| (
            (edge.work.left+*x as f64*edge.dpi_scale) as i32,
            (edge.work.top+*y as f64*edge.dpi_scale) as i32,
            (*w as f64*edge.dpi_scale).ceil() as i32,
            (*h as f64*edge.dpi_scale).ceil() as i32)).collect();
        if !self.host.register_native_file_line_keyboard(hwnd, self.epoch, active, &regions) {
            self.interaction.keyboard_mode = false;
        }
        for event in self.host.drain_native_file_line_keys(self.epoch) {
            if self.host.native_file_line_key_is_current(&event) {
                self.handle_key(event.key(), event.ctrl(), cx);
            }
        }
    }

    #[cfg(not(windows))]
    pub(super) fn poll_keyboard(&mut self, _: (f32, f32), _: &mut Context<Self>) {}

    pub(super) fn handle_key(&mut self, key: &str, ctrl: bool, cx: &mut Context<Self>) {
        if self.interaction.gesture.is_some() {
            if key == "Escape" {
                self.interaction.gesture = None;
                self.interaction.cancelled_press = true;
                self.interaction.suppress_click_until = Some(Instant::now() + Duration::from_millis(160));
                self.sync_windows(cx);
            }
            return;
        }
        if key == "Undo" { self.undo_remove(cx); return; }
        self.hover_task = None;
        self.interaction.keyboard_mode = true;
        let tags: Vec<_> = self.slots.iter().filter_map(|slot| match slot { Slot::Tag(tag) => Some(tag.id.clone()), _ => None }).collect();
        let focus = self.interaction.focus.clone().filter(|focus| match focus {
            Focus::Tag(id) => self.trigger.phase() == Phase::Down && tags.contains(id),
            Focus::Sheet(id, _) => self.sheets.iter().any(|sheet| &sheet.tag == id),
        }).or_else(|| {
            if self.trigger.phase() == Phase::Down { tags.first().cloned().map(Focus::Tag) }
            else { self.sheets.first().map(|sheet| Focus::Sheet(sheet.tag.clone(), None)) }
        });
        self.interaction.focus = focus.clone();
        match focus {
            Some(Focus::Tag(id)) => {
                let index = tags.iter().position(|tag| tag == &id).unwrap_or(0);
                match key {
                    "ArrowLeft" | "ArrowRight" | "Home" | "End" => {
                        let next = match key { "Home" => 0, "End" => tags.len()-1, "ArrowLeft" => (index+tags.len()-1)%tags.len(), _ => (index+1)%tags.len() };
                        self.interaction.focus = Some(Focus::Tag(tags[next].clone()));
                    }
                    "ArrowDown" => {
                        if !self.sheets.iter().any(|sheet| sheet.tag == id) { self.open_sheet(id.clone(), false, cx); }
                        self.focus_sheet(&id);
                    }
                    "Space" => {
                        if !self.sheets.iter().any(|sheet| sheet.tag == id) { self.open_sheet(id.clone(), false, cx); }
                        self.toggle_pin(&id, cx);
                    }
                    "Enter" => self.activate_tag(id, ctrl, cx),
                    "Delete" | "Backspace" => self.remove_tag(&id, cx),
                    "Escape" => { self.trigger.retract(); self.fold(cx); self.interaction.focus = None; }
                    _ => {}
                }
            }
            Some(Focus::Sheet(id, selected)) => {
                let Some(sheet) = self.sheets.iter().find(|sheet| sheet.tag == id) else { return };
                let depth = sheet.levels.len();
                let items = sheet.levels.last().map(|level| level.items.clone()).unwrap_or_default();
                let rows: Vec<_> = items.iter().enumerate().filter(|(_, item)| enabled(item) && item["kind"] != "Separator").map(|(i,_)| i).collect();
                match key {
                    "ArrowDown" | "ArrowUp" | "Home" | "End" if !rows.is_empty() => {
                        let at = selected.and_then(|i| rows.iter().position(|row| *row==i));
                        let next = match (key, at) { ("Home",_) => 0, ("End",_) => rows.len()-1, ("ArrowUp",Some(i)) => (i+rows.len()-1)%rows.len(), (_,Some(i)) => (i+1)%rows.len(), _=>0 };
                        self.interaction.focus = Some(Focus::Sheet(id.clone(), Some(rows[next])));
                        if let Some(level)=self.sheets.iter().find(|sheet|sheet.tag==id).and_then(|sheet|sheet.levels.last()) { level.scroll.scroll_to_item(rows[next]); }
                        if next + 1 == rows.len() { self.load_level(id, true, cx); }
                    }
                    "ArrowRight" => if let Some(node) = selected.and_then(|i| items.get(i)).filter(|node| expandable(node)) {
                        self.drill(&id, node.clone(), cx); self.focus_sheet(&id);
                    },
                    "Enter" => if let Some(node) = selected.and_then(|i| items.get(i)) { self.activate_row(&id, node.clone(), ctrl, cx); },
                    "Space" => self.toggle_pin(&id, cx),
                    "ArrowLeft" | "Escape" => {
                        if depth>1 { self.back(&id, depth-1, cx); self.focus_sheet(&id); }
                        else {
                            if key == "Escape" { self.sheets.retain(|sheet| sheet.tag != id); }
                            self.interaction.focus = if self.trigger.phase()==Phase::Down { Some(Focus::Tag(id)) } else { None };
                        }
                    }
                    _ => {}
                }
            }
            None if key=="Escape" => { self.trigger.retract(); self.sheets.clear(); self.fold(cx); }
            _ => {}
        }
        self.sync_windows(cx);
    }

    pub(super) fn focus_sheet(&mut self, id: &str) {
        let row = self.sheets.iter().find(|sheet| sheet.tag==id).and_then(|sheet| sheet.levels.last())
            .and_then(|level| level.items.iter().position(|node| enabled(node) && node["kind"]!="Separator"));
        self.interaction.focus = Some(Focus::Sheet(id.into(), row));
    }
}

pub(super) fn contains((x,y,w,h):(f32,f32,f32,f32), point:(f32,f32), slack:f32)->bool {
    point.0>=x-slack && point.0<=x+w+slack && point.1>=y-slack && point.1<=y+h+slack
}
fn distance(a:(f32,f32),b:(f32,f32))->f32 { (a.0-b.0).hypot(a.1-b.1) }
#[cfg(windows)]
fn left_pressed()->bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState,VK_LBUTTON};
    unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON.0)) as u16 & 0x8000 != 0 }
}
#[cfg(not(windows))]
fn left_pressed()->bool { false }

pub(super) fn render(entity: &Entity<FileLine>, line: &FileLine, tokens: &DesignTokens, t: &crate::preview::ThemeTokens) -> Vec<AnyElement> {
    let mut elements = Vec::new();
    let accent = color(&t.accent);
    if let Some((_, until)) = &line.interaction.undo {
        if Instant::now() < *until && line.trigger.phase() == Phase::Down {
            let entity = entity.clone();
            elements.push(div().id("file-line-undo").absolute().right(px(LINE_PAD+ADD+6.0)).top(px(LINE_TOP+PEG_DROP)).h(px(26.0)).max_w(px(240.0))
                .px(px(10.0)).flex().items_center().gap(px(8.0)).rounded_full().border_1().border_color(color(&t.border))
                .bg(color(&t.surface)).text_size(px(11.0)).child(tr("已从绳上取下", "Removed from the line"))
                .child(div().rounded_full().px(px(8.0)).py(px(2.0)).text_color(accent).bg(accent.opacity(0.12)).child(tr("撤销", "Undo")))
                .cursor_pointer().on_click(move |_,_,cx| entity.update(cx, |line,cx|line.undo_remove(cx))).into_any_element());
        }
    }
    if let Some(gesture) = line.interaction.gesture.as_ref().filter(|gesture|gesture.active) {
        let tray = line.tray_rect();
        let over_tray = gesture.removable && contains(tray, gesture.point, 6.0);
        if gesture.removable {
            elements.push(div().absolute().left(px(tray.0)).top(px(tray.1)).w(px(tray.2)).h(px(tray.3))
                .flex().items_center().justify_center().rounded(px(10.0)).border_1().border_dashed().border_color(color(&t.error).opacity(0.6))
                .bg(motion::mix_hsla(color(&t.background), color(&t.error), if over_tray {0.18} else {0.07}))
                .text_color(color(&t.error)).child(tr("拖到这里取下", "Drop here to remove")).into_any_element());
        }
        if gesture.point.1 <= BAND_HEIGHT {
            let before=line.insert_before(gesture.point.0, Some(&node_path(&gesture.node)));
            let tags=line.layout().0;
            let x=before.and_then(|before|tags.iter().find_map(|(slot,b)|match &line.slots[*slot]{Slot::Tag(tag) if node_path(&tag.node)==before=>Some(b.left-5.0),_=>None})).unwrap_or(gesture.point.0);
            elements.push(div().absolute().left(px(x)).top(px(LINE_TOP)).w(px(2.0)).h(px(42.0)).rounded(px(1.0)).bg(accent).into_any_element());
        }
        let ghost = line.ghost_rect(gesture.point);
        elements.push(div().absolute().left(px(ghost.0)).top(px(ghost.1))
            .h(px(30.0)).max_w(px(240.0)).px(px(10.0)).flex().items_center().gap(px(7.0)).rounded(px(8.0)).border_1()
            .border_color(color(&t.border)).bg(color(&t.surface).opacity(0.94)).shadow_md().whitespace_nowrap().overflow_hidden()
            .child(icon(tokens, row_icon(&gesture.node),14.0)).child(text(&gesture.node,"title"))
            .child(div().text_size(px(11.0)).text_color(if over_tray {color(&t.error)} else {accent}).child(if over_tray {tr("取下", "Remove")} else {tr("拖动", "Drag")}))
            .into_any_element());
    }
    elements
}

#[cfg(windows)]
fn pointer_over_target(target: i64) -> bool {
    use windows::Win32::{Foundation::POINT,UI::WindowsAndMessaging::{GetAncestor,WindowFromPoint,GA_ROOT}};
    let Some((x,y))=platform::cursor_position() else {return false};
    let root=unsafe {GetAncestor(WindowFromPoint(POINT{x,y}),GA_ROOT)};
    target!=0 && root.0 as i64==target
}
#[cfg(not(windows))]
fn pointer_over_target(_:i64)->bool {false}

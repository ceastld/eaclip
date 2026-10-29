//! Pure FIFO keyboard ownership. The adapter supplies presentation/foreground
//! validity; this ledger never queries windows, the clipboard or queue items.
use std::collections::{HashMap, HashSet, VecDeque};

const MAX_INBOX: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Control {
    Container,
    Row(usize),
    Retry,
    Start,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub generation: u64,
    pub focus_epoch: u64,
    pub enabled_order: Vec<Control>,
    pub focused: Control,
    pub selected_row: usize,
    pub composing: bool,
    pub starting: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Input {
    pub vk: u32,
    pub down: bool,
    pub repeat: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub win: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub epoch: u64,
    pub focus_epoch: u64,
    pub press_id: u64,
    pub generation: u64,
    pub control: Control,
    pub key: &'static str,
    pub down: bool,
    pub repeat: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub win: bool,
    // Space checks this only while pairing its release. Later native cursor
    // movement cannot invalidate actions already accepted in queue order.
    control_epoch: u64,
}

impl Stroke {
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn focus_epoch(&self) -> u64 {
        self.focus_epoch
    }
    pub fn press_id(&self) -> u64 {
        self.press_id
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn control(&self) -> Control {
        self.control
    }
    pub fn key(&self) -> &'static str {
        self.key
    }
    pub fn down(&self) -> bool {
        self.down
    }
    pub fn repeat(&self) -> bool {
        self.repeat
    }
    pub fn alt(&self) -> bool {
        self.alt
    }
    pub fn ctrl(&self) -> bool {
        self.ctrl
    }
    pub fn shift(&self) -> bool {
        self.shift
    }
    pub fn win(&self) -> bool {
        self.win
    }
}

#[derive(Clone)]
struct Press {
    stroke: Stroke,
    actionable: bool,
}

pub struct Ledger {
    scope: Option<Scope>,
    epoch: u64,
    control_epoch: u64,
    next_press_id: u64,
    cursor: Control,
    selected_row: usize,
    enabled_index: HashMap<Control, usize>,
    enabled_rows: Vec<usize>,
    inbox: VecDeque<Stroke>,
    // Only the six whitelisted virtual keys can enter this bounded map.
    // Retirement retains their ownership solely until release/fresh down.
    held: HashMap<u32, Press>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            scope: None,
            epoch: 0,
            control_epoch: 0,
            next_press_id: 0,
            cursor: Control::Container,
            selected_row: 0,
            enabled_index: HashMap::new(),
            enabled_rows: Vec::new(),
            inbox: VecDeque::with_capacity(MAX_INBOX),
            held: HashMap::with_capacity(6),
        }
    }
}

impl Ledger {
    pub fn bind(&mut self, scope: Scope) -> Result<(), String> {
        let mut seen = HashSet::new();
        if (!scope.starting && !scope.enabled_order.contains(&Control::Cancel))
            || scope
                .enabled_order
                .iter()
                .any(|control| *control == Control::Container || !seen.insert(*control))
        {
            self.retire();
            return Err(
                "FIFO keyboard order requires unique controls and Cancel outside starting.".into(),
            );
        }
        if self.scope.as_ref().is_some_and(|bound| {
            bound.generation == scope.generation
                && bound.focus_epoch == scope.focus_epoch
                && bound.enabled_order == scope.enabled_order
                && bound.composing == scope.composing
                && bound.starting == scope.starting
        }) {
            // Native Tab/arrows may be several events ahead of the GPUI poll.
            // Only an external focus epoch authorizes replacing their cursor.
            return Ok(());
        }
        self.retire();
        self.cursor = scope.focused;
        self.selected_row = scope.selected_row;
        // Precompute outside the hook: routing does no row-vector allocation
        // or linear enabled-control scan, even for a long FIFO preview.
        self.enabled_index = scope
            .enabled_order
            .iter()
            .enumerate()
            .map(|(index, control)| (*control, index))
            .collect();
        self.enabled_rows = scope
            .enabled_order
            .iter()
            .filter_map(|control| {
                if let Control::Row(index) = control {
                    Some(*index)
                } else {
                    None
                }
            })
            .collect();
        self.enabled_rows.sort_unstable();
        self.scope = Some(scope);
        Ok(())
    }

    pub fn retire(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.control_epoch = self.control_epoch.wrapping_add(1);
        self.scope = None;
        self.inbox.clear();
    }

    pub fn is_bound(&self) -> bool {
        self.scope.is_some()
    }

    pub fn current(&self, stroke: &Stroke) -> bool {
        self.scope.as_ref().is_some_and(|scope| {
            stroke.epoch == self.epoch
                && stroke.generation == scope.generation
                && stroke.focus_epoch == scope.focus_epoch
                && !scope.composing
                && !scope.starting
        })
    }

    pub fn pop(&mut self) -> Option<Stroke> {
        while let Some(stroke) = self.inbox.pop_front() {
            if self.current(&stroke) {
                return Some(stroke);
            }
        }
        None
    }

    /// Drain original presses before another priority route. A fresh down only
    /// clears a stale drain; the adapter separately adjudicates its new owner.
    pub fn route_held(&mut self, input: Input, allowed: bool) -> bool {
        if input.down && !input.repeat {
            self.held.remove(&input.vk);
            return false;
        }
        self.route(input, allowed)
    }

    pub fn route(&mut self, input: Input, allowed: bool) -> bool {
        if !allowed && self.is_bound() {
            self.retire();
        }
        if input.down && !input.repeat {
            // A missed release cannot let a new physical press inherit a drain.
            self.held.remove(&input.vk);
            return self.accept_down(input, allowed);
        }
        let Some(press) = self.held.get(&input.vk).cloned() else {
            // Never acquire a preheld repeat or an unmatched key-up.
            return false;
        };
        if !input.down {
            self.held.remove(&input.vk);
        }
        if !allowed || !self.current(&press.stroke) {
            return true;
        }
        if input.down {
            if !modifiers_allowed(input, press.stroke.key) {
                return true;
            }
            if matches!(press.stroke.key, "Tab" | "ArrowUp" | "ArrowDown") {
                if let Some(control) = self.down_action(press.stroke.key, input.shift) {
                    let mut stroke = press.stroke.clone();
                    stroke.control = control;
                    stroke.control_epoch = self.control_epoch;
                    set_input(&mut stroke, input);
                    self.enqueue(stroke);
                }
            }
            // Enter/Escape execute once; Space activates only on release.
            return true;
        }
        let mut stroke = press.stroke;
        set_input(&mut stroke, input);
        if stroke.key == "Space" {
            if !press.actionable
                || !modifiers_allowed(input, stroke.key)
                || stroke.control != self.cursor
                || stroke.control_epoch != self.control_epoch
            {
                return true;
            }
            if let Control::Row(index) = stroke.control {
                self.selected_row = index;
            }
        }
        if press.actionable {
            self.enqueue(stroke);
        }
        true
    }

    fn accept_down(&mut self, input: Input, allowed: bool) -> bool {
        let Some(scope) = self.scope.as_ref() else {
            return false;
        };
        if !allowed || scope.composing {
            return false;
        }
        let Some(key) = key_name(input.vk) else {
            return false;
        };
        if !modifiers_allowed(input, key) {
            return false;
        }
        let generation = scope.generation;
        let focus_epoch = scope.focus_epoch;
        let starting = scope.starting;
        let action = (!starting)
            .then(|| self.down_action(key, input.shift))
            .flatten();
        self.next_press_id = self.next_press_id.wrapping_add(1);
        let stroke = Stroke {
            epoch: self.epoch,
            focus_epoch,
            press_id: self.next_press_id,
            generation,
            control: action.unwrap_or(self.cursor),
            key,
            down: true,
            repeat: false,
            alt: input.alt,
            ctrl: input.ctrl,
            shift: input.shift,
            win: input.win,
            control_epoch: self.control_epoch,
        };
        self.held.insert(
            input.vk,
            Press {
                stroke: stroke.clone(),
                actionable: action.is_some(),
            },
        );
        if action.is_some() {
            self.enqueue(stroke);
        }
        true
    }

    fn down_action(&mut self, key: &'static str, shift: bool) -> Option<Control> {
        let scope = self.scope.as_ref()?;
        match key {
            "Escape" => Some(Control::Cancel),
            "Tab" => {
                let order = &scope.enabled_order;
                let position = self.enabled_index.get(&self.cursor).copied();
                let index = match (position, shift) {
                    (Some(index), true) => (index + order.len() - 1) % order.len(),
                    (Some(index), false) => (index + 1) % order.len(),
                    (None, true) => order.len() - 1,
                    (None, false) => 0,
                };
                self.cursor = order[index];
                // Every traversal counts, including a one-control wrap.
                self.control_epoch = self.control_epoch.wrapping_add(1);
                Some(self.cursor)
            }
            "ArrowUp" | "ArrowDown" => {
                let rows = &self.enabled_rows;
                let first = *rows.first()?;
                let last = *rows.last().unwrap();
                let selected = if key == "ArrowUp" {
                    let position = rows.partition_point(|index| *index < self.selected_row);
                    position.checked_sub(1).map_or(first, |index| rows[index])
                } else {
                    let position = rows.partition_point(|index| *index <= self.selected_row);
                    rows.get(position).copied().unwrap_or(last)
                };
                self.selected_row = selected;
                let target = Control::Row(selected);
                if self.cursor != target {
                    self.control_epoch = self.control_epoch.wrapping_add(1);
                }
                self.cursor = target;
                Some(target)
            }
            "Enter" | "Space" => {
                let target = if key == "Enter" && self.cursor == Control::Container {
                    Control::Start
                } else {
                    self.cursor
                };
                if !self.enabled_index.contains_key(&target) {
                    return None;
                }
                if key == "Enter" {
                    if let Control::Row(index) = target {
                        self.selected_row = index;
                    }
                }
                Some(target)
            }
            _ => None,
        }
    }

    fn enqueue(&mut self, stroke: Stroke) {
        if self.inbox.len() >= MAX_INBOX {
            // Dropping a down while retaining its terminal up would invent a
            // commit. Retire all queued work and keep only physical drains.
            self.retire();
        } else {
            self.inbox.push_back(stroke);
        }
    }
}

fn key_name(vk: u32) -> Option<&'static str> {
    match vk {
        0x09 => Some("Tab"),
        0x0D => Some("Enter"),
        0x1B => Some("Escape"),
        0x20 => Some("Space"),
        0x26 => Some("ArrowUp"),
        0x28 => Some("ArrowDown"),
        _ => None,
    }
}

fn modifiers_allowed(input: Input, key: &str) -> bool {
    !input.alt && !input.ctrl && !input.win && (!input.shift || key == "Tab")
}

fn set_input(stroke: &mut Stroke, input: Input) {
    stroke.down = input.down;
    stroke.repeat = input.down && input.repeat;
    stroke.alt = input.alt;
    stroke.ctrl = input.ctrl;
    stroke.shift = input.shift;
    stroke.win = input.win;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            generation: 5,
            focus_epoch: 7,
            enabled_order: vec![
                Control::Row(0),
                Control::Row(1),
                Control::Start,
                Control::Cancel,
            ],
            focused: Control::Container,
            selected_row: 0,
            composing: false,
            starting: false,
        }
    }
    fn input(vk: u32, down: bool, repeat: bool) -> Input {
        Input {
            vk,
            down,
            repeat,
            ..Default::default()
        }
    }
    fn bind() -> Ledger {
        let mut ledger = Ledger::default();
        ledger.bind(scope()).unwrap();
        ledger
    }
    fn drain(ledger: &mut Ledger) -> Vec<Stroke> {
        std::iter::from_fn(|| ledger.pop()).collect()
    }
    fn tap(ledger: &mut Ledger, vk: u32) {
        assert!(ledger.route(input(vk, true, false), true));
        assert!(ledger.route(input(vk, false, false), true));
    }

    #[test]
    fn fast_tab_space_uses_core_cursor_despite_lagging_ui_snapshot() {
        let mut ledger = bind();
        tap(&mut ledger, 0x09);
        // The UI has not replayed Tab and still reports Container/row zero.
        ledger.bind(scope()).unwrap();
        tap(&mut ledger, 0x20);
        let strokes = drain(&mut ledger);
        assert_eq!(
            strokes
                .iter()
                .map(|s| (s.key, s.down, s.control))
                .collect::<Vec<_>>(),
            vec![
                ("Tab", true, Control::Row(0)),
                ("Tab", false, Control::Row(0)),
                ("Space", true, Control::Row(0)),
                ("Space", false, Control::Row(0)),
            ]
        );
        assert_eq!(strokes[2].press_id, strokes[3].press_id);
        assert_ne!(strokes[0].press_id, strokes[2].press_id);
    }

    #[test]
    fn later_cursor_moves_preserve_accepted_tab_enter_and_space_up_order() {
        let mut ledger = bind();
        tap(&mut ledger, 0x09);
        tap(&mut ledger, 0x0D);
        tap(&mut ledger, 0x20);
        tap(&mut ledger, 0x09);
        let strokes = drain(&mut ledger);
        assert!(strokes.iter().all(|stroke| ledger.current(stroke)));
        assert_eq!(strokes[2].key, "Enter");
        assert_eq!(strokes[2].control, Control::Row(0));
        assert_eq!(strokes[5].key, "Space");
        assert!(!strokes[5].down);
        assert_eq!(strokes[5].control, Control::Row(0));
        assert_eq!(strokes[6].control, Control::Row(1));
    }

    #[test]
    fn space_control_aba_never_queues_activation_release() {
        let mut ledger = bind();
        tap(&mut ledger, 0x09);
        drain(&mut ledger);
        assert!(ledger.route(input(0x20, true, false), true));
        let down = ledger.pop().unwrap();
        assert_eq!(down.control, Control::Row(0));
        for _ in 0..4 {
            tap(&mut ledger, 0x09);
        }
        assert_eq!(ledger.cursor, Control::Row(0));
        assert!(ledger.route(input(0x20, false, false), true));
        assert!(drain(&mut ledger)
            .iter()
            .all(|stroke| stroke.key != "Space"));
        assert!(!ledger.route(input(0x20, false, false), true));
    }

    #[test]
    fn external_focus_and_generation_rebind_retire_queued_actions_but_drain_held() {
        for change_generation in [false, true] {
            let mut ledger = bind();
            assert!(ledger.route(input(0x0D, true, false), true));
            let old = ledger.pop().unwrap();
            let mut next = scope();
            if change_generation {
                next.generation += 1;
            } else {
                next.focus_epoch += 1;
            }
            next.focused = Control::Cancel;
            ledger.bind(next).unwrap();
            assert!(!ledger.current(&old));
            assert!(ledger.route(input(0x0D, true, true), true));
            assert!(ledger.route(input(0x0D, false, false), true));
            assert!(ledger.pop().is_none());
            assert!(!ledger.route(input(0x0D, false, false), true));
            tap(&mut ledger, 0x0D);
            assert_eq!(ledger.pop().unwrap().control, Control::Cancel);
        }
    }

    #[test]
    fn presentation_retirement_and_suspension_preserve_only_owned_release_drains() {
        for explicit in [false, true] {
            let mut ledger = bind();
            assert!(ledger.route(input(0x28, true, false), true));
            let accepted = ledger.pop().unwrap();
            if explicit {
                ledger.retire();
            }
            assert!(ledger.route(input(0x28, true, true), explicit));
            assert!(!ledger.current(&accepted));
            assert!(ledger.route(input(0x28, false, false), false));
            assert!(!ledger.route(input(0x28, true, false), false));
            assert!(!ledger.is_bound());
            assert!(ledger.pop().is_none());
        }
    }

    #[test]
    fn preheld_repeats_unmatched_ups_characters_and_ime_never_acquire() {
        let mut ledger = bind();
        for vk in [0x09, 0x0D, 0x1B, 0x20, 0x26, 0x28] {
            assert!(!ledger.route(input(vk, true, true), true));
            assert!(!ledger.route(input(vk, false, false), true));
        }
        for vk in [0x41, 0x31, 0x25, 0x27, 0xE5, 0xE7] {
            assert!(!ledger.route(input(vk, true, false), true));
            assert!(!ledger.route(input(vk, false, false), true));
        }
        assert!(ledger.pop().is_none());
    }

    #[test]
    fn composition_start_retires_actions_and_keeps_old_press_drain() {
        let mut ledger = bind();
        assert!(ledger.route(input(0x0D, true, false), true));
        let accepted = ledger.pop().unwrap();
        let mut composing = scope();
        composing.composing = true;
        ledger.bind(composing).unwrap();
        assert!(!ledger.current(&accepted));
        assert!(ledger.route(input(0x0D, true, true), true));
        assert!(ledger.route(input(0x0D, false, false), true));
        for vk in [0x09, 0x0D, 0x1B, 0x20, 0x26, 0x28] {
            assert!(!ledger.route(input(vk, true, false), true));
            assert!(!ledger.route(input(vk, false, false), true));
        }
        assert!(ledger.pop().is_none());
        ledger.bind(scope()).unwrap();
        tap(&mut ledger, 0x0D);
        assert_eq!(ledger.pop().unwrap().control, Control::Start);
    }

    #[test]
    fn modified_presses_pass_and_modified_space_up_consumes_without_activation() {
        for modifier in [0, 1, 2, 3] {
            let mut ledger = bind();
            for vk in [0x0D, 0x1B, 0x20, 0x26, 0x28] {
                let mut down = input(vk, true, false);
                match modifier {
                    0 => down.alt = true,
                    1 => down.ctrl = true,
                    2 => down.shift = true,
                    _ => down.win = true,
                }
                assert!(!ledger.route(down, true));
                assert!(!ledger.route(input(vk, false, false), true));
            }
            tap(&mut ledger, 0x09);
            drain(&mut ledger);
            assert!(ledger.route(input(0x20, true, false), true));
            ledger.pop().unwrap();
            let mut up = input(0x20, false, false);
            match modifier {
                0 => up.alt = true,
                1 => up.ctrl = true,
                2 => up.shift = true,
                _ => up.win = true,
            }
            assert!(ledger.route(up, true));
            assert!(ledger.pop().is_none());
            assert!(!ledger.route(up, true));
        }
    }

    #[test]
    fn tab_wrap_shift_tab_and_repeats_keep_exact_targets() {
        let mut ledger = bind();
        let mut back = input(0x09, true, false);
        back.shift = true;
        assert!(ledger.route(back, true));
        assert_eq!(ledger.pop().unwrap().control, Control::Cancel);
        back.repeat = true;
        assert!(ledger.route(back, true));
        let second = ledger.pop().unwrap();
        assert!(second.repeat && second.shift);
        assert_eq!(second.control, Control::Start);
        assert!(ledger.route(input(0x09, false, false), true));
        ledger.pop().unwrap();
        tap(&mut ledger, 0x09);
        assert_eq!(ledger.pop().unwrap().control, Control::Cancel);
        ledger.pop().unwrap();
        tap(&mut ledger, 0x09);
        assert_eq!(ledger.pop().unwrap().control, Control::Row(0));
    }

    #[test]
    fn navigation_repeats_clamp_enabled_rows_and_row_activation_updates_selection() {
        let mut ledger = Ledger::default();
        let mut bound = scope();
        bound.enabled_order = vec![
            Control::Row(0),
            Control::Row(2),
            Control::Start,
            Control::Cancel,
        ];
        bound.focused = Control::Row(2);
        ledger.bind(bound.clone()).unwrap();
        tap(&mut ledger, 0x0D);
        assert_eq!(ledger.selected_row, 2);
        drain(&mut ledger);
        for repeat in [false, true, true] {
            assert!(ledger.route(input(0x28, true, repeat), true));
        }
        let events = drain(&mut ledger);
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|event| event.control == Control::Row(2)));
        assert!(ledger.route(input(0x28, false, false), true));
        drain(&mut ledger);
        ledger.bind(bound).unwrap(); // selected_row=0 is still a lagging snapshot.
        tap(&mut ledger, 0x26);
        assert_eq!(ledger.pop().unwrap().control, Control::Row(0));
    }

    #[test]
    fn enter_escape_and_space_repeats_never_duplicate_activation() {
        let mut ledger = bind();
        tap(&mut ledger, 0x09);
        drain(&mut ledger);
        for vk in [0x0D, 0x1B, 0x20] {
            assert!(ledger.route(input(vk, true, false), true));
            assert!(ledger.route(input(vk, true, true), true));
            assert!(ledger.route(input(vk, true, true), true));
            assert!(ledger.route(input(vk, false, false), true));
            let events = drain(&mut ledger);
            assert_eq!(events.len(), 2);
            assert!(events[0].down && !events[0].repeat);
            assert!(!events[1].down);
            assert_eq!(events[0].press_id, events[1].press_id);
        }
    }

    #[test]
    fn container_space_and_disabled_controls_consume_pairs_without_action() {
        let mut ledger = bind();
        tap(&mut ledger, 0x20);
        assert!(ledger.pop().is_none());
        let mut disabled = scope();
        disabled.focus_epoch += 1;
        disabled.enabled_order = vec![Control::Cancel];
        disabled.focused = Control::Start;
        ledger.bind(disabled).unwrap();
        for vk in [0x0D, 0x20, 0x26, 0x28] {
            tap(&mut ledger, vk);
            assert!(ledger.pop().is_none());
        }
        tap(&mut ledger, 0x1B);
        assert_eq!(ledger.pop().unwrap().control, Control::Cancel);
    }

    #[test]
    fn starting_consumes_supported_pairs_without_actions_and_retires_previous() {
        let mut ledger = bind();
        tap(&mut ledger, 0x0D);
        let accepted = ledger.pop().unwrap();
        let mut starting = scope();
        starting.starting = true;
        ledger.bind(starting).unwrap();
        assert!(!ledger.current(&accepted));
        for vk in [0x09, 0x0D, 0x1B, 0x20, 0x26, 0x28] {
            assert!(ledger.route(input(vk, true, false), true));
            assert!(ledger.route(input(vk, true, true), true));
            assert!(ledger.route(input(vk, false, false), true));
        }
        assert!(ledger.pop().is_none());
        assert!(!ledger.route(input(0x41, true, false), true));
        ledger.bind(scope()).unwrap();
        tap(&mut ledger, 0x0D);
        assert_eq!(ledger.pop().unwrap().control, Control::Start);
    }

    #[test]
    fn empty_starting_order_consumes_pairs_and_old_held_cannot_activate_after_starting() {
        let mut ledger = Ledger::default();
        let mut starting = scope();
        starting.starting = true;
        starting.enabled_order.clear();
        ledger.bind(starting).unwrap();
        for vk in [0x09, 0x0D, 0x1B, 0x20, 0x26, 0x28] {
            assert!(ledger.route(input(vk, true, false), true));
            assert!(ledger.route(input(vk, true, true), true));
            assert!(ledger.route(input(vk, false, false), true));
        }
        assert!(ledger.pop().is_none());
        assert!(ledger.route(input(0x0D, true, false), true));
        ledger.bind(scope()).unwrap();
        assert!(ledger.route_held(input(0x0D, true, true), true));
        assert!(ledger.route_held(input(0x0D, false, false), true));
        assert!(ledger.pop().is_none());
        assert!(!ledger.route_held(input(0x0D, false, false), true));
        tap(&mut ledger, 0x0D);
        assert_eq!(ledger.pop().unwrap().control, Control::Start);
    }

    #[test]
    fn enabled_order_changes_invalidate_space_press_and_queued_actions() {
        let mut ledger = bind();
        tap(&mut ledger, 0x09);
        drain(&mut ledger);
        assert!(ledger.route(input(0x20, true, false), true));
        let accepted = ledger.pop().unwrap();
        let mut changed = scope();
        changed.enabled_order.remove(0);
        changed.focused = Control::Row(1);
        ledger.bind(changed).unwrap();
        assert!(!ledger.current(&accepted));
        assert!(ledger.route(input(0x20, false, false), true));
        assert!(ledger.pop().is_none());
        tap(&mut ledger, 0x20);
        assert_eq!(ledger.pop().unwrap().control, Control::Row(1));
    }

    #[test]
    fn bounded_inbox_overflow_retires_work_but_retains_original_up() {
        let mut ledger = bind();
        assert!(ledger.route(input(0x28, true, false), true));
        let accepted = ledger.inbox.front().unwrap().clone();
        for _ in 1..64 {
            assert!(ledger.route(input(0x28, true, true), true));
        }
        assert_eq!(ledger.inbox.len(), 64);
        assert!(ledger.route(input(0x28, true, true), true));
        assert!(!ledger.is_bound());
        assert!(!ledger.current(&accepted));
        assert!(ledger.pop().is_none());
        assert!(ledger.route(input(0x28, false, false), true));
        assert!(!ledger.route(input(0x28, false, false), true));
    }

    #[test]
    fn fresh_down_clears_stale_drain_and_invalid_order_retires_safely() {
        let mut ledger = bind();
        assert!(ledger.route(input(0x0D, true, false), true));
        let old = ledger.pop().unwrap();
        ledger.retire();
        assert!(!ledger.route(input(0x0D, true, false), true));
        assert!(!ledger.route(input(0x0D, false, false), true));
        ledger.bind(scope()).unwrap();
        tap(&mut ledger, 0x0D);
        let next = ledger.pop().unwrap();
        assert_ne!(next.press_id, old.press_id);
        for order in [
            vec![],
            vec![Control::Start],
            vec![Control::Container, Control::Cancel],
            vec![Control::Cancel, Control::Cancel],
        ] {
            let mut invalid = scope();
            invalid.enabled_order = order;
            assert!(ledger.bind(invalid).is_err());
            assert!(!ledger.is_bound());
            assert!(ledger.pop().is_none());
        }
    }

    #[test]
    fn early_held_route_clears_fresh_press_without_retiring_or_accepting_binding() {
        let mut ledger = bind();
        assert!(ledger.route(input(0x0D, true, false), true));
        let original = ledger.pop().unwrap();
        // Fresh QP/dialog routing has not yet been decided. The old drain
        // must disappear without taking this new down or dropping FIFO scope.
        assert!(!ledger.route_held(input(0x0D, true, false), false));
        assert!(ledger.is_bound());
        assert!(ledger.current(&original));
        assert!(ledger.pop().is_none());
        assert!(!ledger.route_held(input(0x0D, false, false), true));
        assert!(ledger.route(input(0x0D, true, false), true));
        let next = ledger.pop().unwrap();
        assert_ne!(next.press_id, original.press_id);
        assert!(ledger.route_held(input(0x0D, false, false), true));
        assert_eq!(ledger.pop().unwrap().press_id, next.press_id);
    }
}

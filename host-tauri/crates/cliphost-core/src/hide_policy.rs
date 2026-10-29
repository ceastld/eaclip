//! Same decision tree as C# `HostHidePolicy` / frontend `evaluateHostHidePolicy`.

use crate::model::DismissBehavior;

pub const EXPAND_GRACE_MS: i64 = 800;

/// C# `HostDismissController.AreaExtPx`. Fold and Hide inflate the HWND rect by this many
/// physical pixels on each side (`WindowHelper.IsMouseOverWindow(..., FoldPointerAreaExt)`).
pub const AREA_EXT_PX: i32 = 8;

/// C# `IsPointerEngagingHost`: Fold/Hide use outer HWND + AreaExt only (never WebView child
/// tree — child HWND can stay large after fold). EdgeHide uses `HostHitTest` tree.
pub fn pointer_area_ext_px(behavior: DismissBehavior) -> i32 {
    match behavior {
        DismissBehavior::Fold | DismissBehavior::Hide => AREA_EXT_PX,
        DismissBehavior::EdgeHide => 0,
    }
}

/// C# Fold/Hide skip `HostHitTest.IsScreenPointInHostTree`; EdgeHide uses it.
pub fn pointer_uses_host_tree(behavior: DismissBehavior) -> bool {
    behavior == DismissBehavior::EdgeHide
}

/// C# `Rectangle.Contains` after padding `GetWindowRect` (inclusive left/top, exclusive right/bottom).
pub fn point_in_window_rect(
    x: i32,
    y: i32,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    pad: i32,
) -> bool {
    x >= left - pad && x < right + pad && y >= top - pad && y < bottom + pad
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostHideTrigger {
    Poll,
    Pointer,
    OutsideClick,
    ForegroundChange,
    Deactivated,
    UserDismiss,
    Shown,
    Hidden,
    KeyDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostDismissAction {
    None,
    Hide,
    Fold,
    Expand,
    EdgeHide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostFakeFocusAction {
    Unchanged,
    Set,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyboardRouting {
    NotApplicable,
    PassThrough,
    SwallowDigit,
    SwallowNavigation,
    SwallowEscapeHide,
}

#[derive(Debug, Clone)]
pub struct HostHideSnapshot {
    pub behavior: DismissBehavior,
    pub visible: bool,
    pub is_pinned: bool,
    pub pointer_in_host: bool,
    pub has_fake_focus: bool,
    pub no_focus_session_active: bool,
    pub enable_no_focus_mode: bool,
    pub spa_modal_open: bool,
    pub is_collapsed: bool,
    pub is_layout_busy: bool,
    pub ignore_pointer_until_leave: bool,
    pub move_interaction_active: bool,
    pub is_active: bool,
    pub foreground_is_host_related: bool,
    pub can_apply_edge_tuck: bool,
    pub still_visible_after_edge_hide: bool,
    pub expand_grace_until_ms: i64,
    pub no_focus_show_grace_until_ms: i64,
    pub has_open_host_overlay: bool,
    pub web_view_ready: bool,
    pub modifier_blocking: bool,
    pub is_digit_key: bool,
    pub is_escape: bool,
    pub is_navigation_key: bool,
    pub foreground_is_owned_dialog: bool,
    pub defer_host_keyboard_routing: bool,
}

impl Default for HostHideSnapshot {
    fn default() -> Self {
        Self {
            behavior: DismissBehavior::Hide,
            visible: true,
            is_pinned: false,
            pointer_in_host: false,
            has_fake_focus: false,
            no_focus_session_active: true,
            enable_no_focus_mode: true,
            spa_modal_open: false,
            is_collapsed: false,
            is_layout_busy: false,
            ignore_pointer_until_leave: false,
            move_interaction_active: false,
            is_active: false,
            foreground_is_host_related: false,
            can_apply_edge_tuck: true,
            still_visible_after_edge_hide: false,
            expand_grace_until_ms: 0,
            no_focus_show_grace_until_ms: 0,
            has_open_host_overlay: false,
            web_view_ready: true,
            modifier_blocking: false,
            is_digit_key: false,
            is_escape: false,
            is_navigation_key: false,
            foreground_is_owned_dialog: false,
            defer_host_keyboard_routing: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostHideDecision {
    pub window_action: HostDismissAction,
    pub fake_focus_action: HostFakeFocusAction,
    pub keyboard_routing: HostKeyboardRouting,
    pub listen_for_outside_click: bool,
    pub next_ignore_pointer_until_leave: bool,
    pub path: Vec<String>,
    pub reason: String,
}

struct Facts {
    trigger: HostHideTrigger,
    snapshot: HostHideSnapshot,
    listen: bool,
    keyboard_engaged: bool,
    hide_should_stay: bool,
    fold_blocks_collapse: bool,
    edge_should_stay: bool,
    should_clear_fake_focus_on_deactivate: bool,
    edge_foreground_should_dismiss: bool,
}

pub fn evaluate(trigger: HostHideTrigger, snapshot: &HostHideSnapshot) -> HostHideDecision {
    evaluate_at(trigger, snapshot, chrono::Utc::now().timestamp_millis())
}

pub fn evaluate_at(trigger: HostHideTrigger, snapshot: &HostHideSnapshot, utc_now_ms: i64) -> HostHideDecision {
    let facts = build_facts(trigger, snapshot, utc_now_ms);
    let mut path = vec!["start".into(), trigger_node(trigger).into()];

    if let Some(life) = lifecycle(&facts, &mut path) {
        return life;
    }
    if trigger == HostHideTrigger::KeyDown {
        if facts.keyboard_engaged
            && facts.snapshot.no_focus_session_active
            && facts.snapshot.visible
            && !facts.snapshot.foreground_is_owned_dialog
            && !facts.snapshot.defer_host_keyboard_routing
            && !facts.snapshot.modifier_blocking
            && facts.snapshot.web_view_ready
        {
            path.push("key-engaged".into());
        }
        return keyboard(&facts, &mut path);
    }
    if let Some(shared) = shared_auto_gate(&facts, &mut path) {
        return shared;
    }
    if trigger == HostHideTrigger::ForegroundChange {
        path.push("foreground".into());
        if facts.snapshot.behavior != DismissBehavior::EdgeHide {
            return done(
                HostDismissAction::None,
                HostFakeFocusAction::Unchanged,
                facts.listen,
                facts.snapshot.ignore_pointer_until_leave,
                path,
                "foreground-not-edge",
                "前台变化只驱动贴边隐藏。",
            );
        }
        if facts.snapshot.pointer_in_host {
            return done(
                HostDismissAction::None,
                HostFakeFocusAction::Set,
                facts.listen,
                facts.snapshot.ignore_pointer_until_leave,
                path,
                "foreground-pointer-inside",
                "点在宿主上引起的前台变化（含 WebView 抢前台）：置假焦点，不贴边。",
            );
        }
        return edge_foreground(&facts, path);
    }
    if trigger == HostHideTrigger::OutsideClick {
        path.push("outside-click".into());
        if facts.snapshot.pointer_in_host {
            return done(
                HostDismissAction::None,
                HostFakeFocusAction::Set,
                facts.listen,
                facts.snapshot.ignore_pointer_until_leave,
                path,
                "pointer-inside",
                "点在宿主树上：置假焦点，不消失。",
            );
        }
        if facts.snapshot.is_pinned {
            return done(
                HostDismissAction::None,
                HostFakeFocusAction::Unchanged,
                false,
                facts.snapshot.ignore_pointer_until_leave,
                path,
                "pinned",
                "置顶：点窗外不消失，假焦点保持。",
            );
        }
        return dismiss(&facts, path);
    }
    maintain(&facts, path)
}

/// Fake focus or hover owns list input, including while the host is pinned.
pub fn keyboard_engaged(s: &HostHideSnapshot) -> bool {
    s.has_fake_focus || s.pointer_in_host
}

fn build_facts(trigger: HostHideTrigger, s: &HostHideSnapshot, utc_now_ms: i64) -> Facts {
    let in_expand_grace = utc_now_ms < s.expand_grace_until_ms;
    let in_show_grace = utc_now_ms < s.no_focus_show_grace_until_ms;
    Facts {
        trigger,
        snapshot: s.clone(),
        listen: s.visible && !s.is_pinned && s.no_focus_session_active,
        keyboard_engaged: keyboard_engaged(s),
        hide_should_stay: s.is_pinned
            || s.pointer_in_host
            || s.has_fake_focus
            || s.spa_modal_open
            || s.is_layout_busy
            || in_expand_grace,
        fold_blocks_collapse: s.is_layout_busy || s.is_pinned || s.is_collapsed || in_expand_grace,
        edge_should_stay: s.move_interaction_active
            || s.has_fake_focus
            || s.pointer_in_host
            || s.is_layout_busy
            || in_expand_grace
            || !(s.no_focus_session_active || !s.is_active),
        should_clear_fake_focus_on_deactivate: {
            if !s.enable_no_focus_mode || s.has_open_host_overlay || in_show_grace {
                false
            } else if s.no_focus_session_active {
                true
            } else {
                !s.is_active
            }
        },
        edge_foreground_should_dismiss: s.visible
            && !s.is_pinned
            && !s.spa_modal_open
            && !s.move_interaction_active
            && !s.foreground_is_host_related,
    }
}

fn lifecycle(facts: &Facts, path: &mut Vec<String>) -> Option<HostHideDecision> {
    match facts.trigger {
        HostHideTrigger::UserDismiss => Some(done(
            HostDismissAction::Hide,
            HostFakeFocusAction::Clear,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            take(path),
            "user-dismiss",
            "标题栏关闭 / 显式隐藏：置顶也整窗隐藏。",
        )),
        HostHideTrigger::Hidden => Some(done(
            HostDismissAction::None,
            HostFakeFocusAction::Clear,
            false,
            false,
            take(path),
            "hidden",
            "窗口已隐藏：清假焦点并停钩。",
        )),
        HostHideTrigger::Shown if facts.snapshot.enable_no_focus_mode => Some(done(
            HostDismissAction::None,
            HostFakeFocusAction::Set,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            take(path),
            "shown-nofocus",
            "显示且无焦点：置假焦点，键盘钩接管数字键。",
        )),
        HostHideTrigger::Shown => Some(done(
            HostDismissAction::None,
            HostFakeFocusAction::Clear,
            false,
            facts.snapshot.ignore_pointer_until_leave,
            take(path),
            "shown-activate",
            "显示且普通激活：不置假焦点，窗口走系统焦点。",
        )),
        HostHideTrigger::Deactivated if facts.should_clear_fake_focus_on_deactivate => Some(done(
            HostDismissAction::None,
            HostFakeFocusAction::Clear,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            take(path),
            "deactivated-clear",
            "失活：清假焦点，避免继续吞掉其它窗口的键。",
        )),
        HostHideTrigger::Deactivated => Some(done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            take(path),
            "deactivated-keep",
            "失活：宽限 / 浮层 / 未开无焦点 → 保持假焦点。",
        )),
        _ => None,
    }
}

fn keyboard(facts: &Facts, path: &mut Vec<String>) -> HostHideDecision {
    let s = &facts.snapshot;
    if !s.no_focus_session_active || !s.visible {
        return key(HostKeyboardRouting::PassThrough, take(path), "key-pass-session", "无焦点会话未开或窗口不可见：键交给前台。");
    }
    if s.foreground_is_owned_dialog {
        return key(HostKeyboardRouting::PassThrough, take(path), "key-pass-dialog", "宿主自有对话框：不吞键。");
    }
    if s.defer_host_keyboard_routing {
        return key(HostKeyboardRouting::PassThrough, take(path), "key-pass-defer", "宿主可编辑框已接管：低级钩放行。");
    }
    if s.modifier_blocking {
        return key(HostKeyboardRouting::PassThrough, take(path), "key-pass-mod", "Ctrl / Alt / Win 按下：放行给系统或其它程序。");
    }
    if !s.web_view_ready {
        return key(HostKeyboardRouting::PassThrough, take(path), "key-pass-ready", "WebView 未就绪：不吞键。");
    }
    if !facts.keyboard_engaged {
        return key(HostKeyboardRouting::PassThrough, take(path), "key-not-engaged", "无假焦点且指针不在宿主：不截获数字键 / 导航键。");
    }
    if s.is_digit_key {
        return key(HostKeyboardRouting::SwallowDigit, take(path), "key-digit", "已参与：吞数字键并路由到列表粘贴。");
    }
    if s.is_escape {
        return key(HostKeyboardRouting::SwallowEscapeHide, take(path), "key-esc", "已参与：Esc 整窗隐藏。");
    }
    if s.is_navigation_key {
        return key(HostKeyboardRouting::SwallowNavigation, take(path), "key-nav", "已参与：吞方向键 / Enter / Tab 等到前端。");
    }
    key(HostKeyboardRouting::PassThrough, take(path), "key-pass", "已参与但不是导航 / 数字 / Esc：放行。")
}

fn shared_auto_gate(facts: &Facts, path: &mut Vec<String>) -> Option<HostHideDecision> {
    match facts.trigger {
        HostHideTrigger::Poll
        | HostHideTrigger::Pointer
        | HostHideTrigger::OutsideClick
        | HostHideTrigger::ForegroundChange => {}
        _ => return None,
    }
    if !facts.snapshot.visible {
        return Some(done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            false,
            facts.snapshot.ignore_pointer_until_leave,
            take(path),
            "not-visible",
            "不可见：不自动消失。",
        ));
    }
    if facts.snapshot.move_interaction_active {
        return Some(done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            take(path),
            "move-active",
            "标题栏拖动 / 系统改尺寸：不自动消失。",
        ));
    }
    if facts.snapshot.spa_modal_open {
        return Some(done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            take(path),
            "spa-modal",
            "SPA 弹层打开：不自动消失。",
        ));
    }
    None
}

fn maintain(facts: &Facts, path: Vec<String>) -> HostHideDecision {
    match facts.snapshot.behavior {
        DismissBehavior::Fold => fold_maintain(facts, path),
        DismissBehavior::EdgeHide => edge_maintain(facts, path),
        DismissBehavior::Hide => hide_maintain(facts, path),
    }
}

fn dismiss(facts: &Facts, path: Vec<String>) -> HostHideDecision {
    match facts.snapshot.behavior {
        DismissBehavior::Fold => fold_dismiss(facts, path, true),
        DismissBehavior::EdgeHide => edge_dismiss(facts, path, true),
        DismissBehavior::Hide => hide_dismiss(facts, path, true),
    }
}

fn hide_maintain(facts: &Facts, mut path: Vec<String>) -> HostHideDecision {
    path.push("mode-hide".into());
    if facts.snapshot.is_pinned {
        return done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            path,
            "pinned",
            "置顶：Hide / EdgeHide 不自动隐藏。",
        );
    }
    if facts.hide_should_stay {
        return done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            path,
            "hide-engaged",
            "Hide 仍参与：指针 / 假焦点 / 置顶 / 弹层 / 宽限 任一成立则不藏。",
        );
    }
    done(
        HostDismissAction::Hide,
        HostFakeFocusAction::Clear,
        facts.listen,
        facts.snapshot.ignore_pointer_until_leave,
        path,
        "hide-hide",
        "Hide 已脱离：整窗隐藏并清假焦点。",
    )
}

fn hide_dismiss(facts: &Facts, mut path: Vec<String>, clear_fake: bool) -> HostHideDecision {
    path.push("mode-hide".into());
    done(
        HostDismissAction::Hide,
        if clear_fake {
            HostFakeFocusAction::Clear
        } else {
            HostFakeFocusAction::Unchanged
        },
        facts.listen,
        facts.snapshot.ignore_pointer_until_leave,
        path,
        "hide-hide",
        "点窗外：普通隐藏整窗。",
    )
}

fn fold_maintain(facts: &Facts, mut path: Vec<String>) -> HostHideDecision {
    path.push("mode-fold".into());
    let s = &facts.snapshot;
    if !s.pointer_in_host {
        if facts.fold_blocks_collapse {
            return done(
                HostDismissAction::None,
                HostFakeFocusAction::Unchanged,
                facts.listen,
                false,
                path,
                "fold-pointer-out-block",
                "指针在外，但置顶 / 已折叠 / 布局忙 / 展开宽限挡住收起。假焦点不挡 Fold。",
            );
        }
        return done(
            HostDismissAction::Fold,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            true,
            path,
            "fold-collapse",
            "指针离开展开窗：收到标题栏，并忽略下一次贴边展开直到指针真正离开。",
        );
    }
    if s.ignore_pointer_until_leave {
        return done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            true,
            path,
            "fold-ignore",
            "折叠后指针仍落在标题栏热区：先忽略展开，等指针离开一次。",
        );
    }
    if s.is_collapsed {
        return done(
            HostDismissAction::Expand,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            false,
            path,
            "fold-expand",
            "已折叠且指针在标题栏热区：展开。",
        );
    }
    done(
        HostDismissAction::None,
        HostFakeFocusAction::Unchanged,
        facts.listen,
        false,
        path,
        "fold-none",
        "已展开且指针仍在窗内：保持。",
    )
}

fn fold_dismiss(facts: &Facts, mut path: Vec<String>, clear_fake: bool) -> HostHideDecision {
    path.push("mode-fold".into());
    let fake = if clear_fake {
        HostFakeFocusAction::Clear
    } else {
        HostFakeFocusAction::Unchanged
    };
    if facts.snapshot.is_collapsed {
        return done(
            HostDismissAction::None,
            fake,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            path,
            "fold-outside-none",
            "点窗外但已经是标题栏：不再收。",
        );
    }
    done(
        HostDismissAction::Fold,
        fake,
        facts.listen,
        true,
        path,
        "fold-collapse",
        "点窗外：强制收到标题栏（绕过展开宽限），并忽略贴边回弹。",
    )
}

fn edge_maintain(facts: &Facts, mut path: Vec<String>) -> HostHideDecision {
    path.push("mode-edge".into());
    if facts.snapshot.is_pinned {
        return done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            path,
            "pinned",
            "置顶：Hide / EdgeHide 不自动隐藏。",
        );
    }
    if facts.edge_should_stay {
        return done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            path,
            "edge-keep",
            "贴边：假焦点 / 指针 / 布局忙 / 宽限 / 仍激活 → 保持展开。",
        );
    }
    if !facts.snapshot.can_apply_edge_tuck {
        return done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            path,
            "edge-no-tuck",
            "贴边轮询：窗口不在可贴边位置，不整窗藏（避免拖进屏内像消失）。",
        );
    }
    done(
        HostDismissAction::EdgeHide,
        HostFakeFocusAction::Unchanged,
        facts.listen,
        facts.snapshot.ignore_pointer_until_leave,
        path,
        "edge-tuck",
        "贴边：无假焦点且指针离开，收向屏幕边。",
    )
}

fn edge_dismiss(facts: &Facts, mut path: Vec<String>, clear_fake: bool) -> HostHideDecision {
    path.push("mode-edge".into());
    let fake = if clear_fake {
        HostFakeFocusAction::Clear
    } else {
        HostFakeFocusAction::Unchanged
    };
    if !facts.snapshot.can_apply_edge_tuck || facts.snapshot.still_visible_after_edge_hide {
        return done(
            HostDismissAction::Hide,
            fake,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            path,
            "edge-hide-fallback",
            "点窗外贴边失败或仍完全可见：回退普通隐藏。",
        );
    }
    done(
        HostDismissAction::EdgeHide,
        fake,
        facts.listen,
        facts.snapshot.ignore_pointer_until_leave,
        path,
        "edge-tuck",
        "点窗外：先贴边收起。",
    )
}

fn edge_foreground(facts: &Facts, path: Vec<String>) -> HostHideDecision {
    if !facts.edge_foreground_should_dismiss {
        return done(
            HostDismissAction::None,
            HostFakeFocusAction::Unchanged,
            facts.listen,
            facts.snapshot.ignore_pointer_until_leave,
            path,
            "foreground-keep",
            "前台仍属宿主相关窗口，或置顶 / 弹层 / 拖动挡住贴边。",
        );
    }
    edge_dismiss(facts, path, true)
}

fn trigger_node(trigger: HostHideTrigger) -> &'static str {
    match trigger {
        HostHideTrigger::Poll => "trigger-poll",
        HostHideTrigger::Pointer => "trigger-pointer",
        HostHideTrigger::OutsideClick => "trigger-outside",
        HostHideTrigger::ForegroundChange => "trigger-foreground",
        HostHideTrigger::Deactivated => "trigger-deactivated",
        HostHideTrigger::UserDismiss => "trigger-user",
        HostHideTrigger::Shown => "trigger-shown",
        HostHideTrigger::Hidden => "trigger-hidden",
        HostHideTrigger::KeyDown => "trigger-key",
    }
}

fn result_node(
    window: HostDismissAction,
    fake: HostFakeFocusAction,
    keyboard: HostKeyboardRouting,
) -> &'static str {
    match keyboard {
        HostKeyboardRouting::SwallowDigit
        | HostKeyboardRouting::SwallowNavigation
        | HostKeyboardRouting::SwallowEscapeHide
        | HostKeyboardRouting::PassThrough => "result-key",
        HostKeyboardRouting::NotApplicable => match (window, fake) {
            (HostDismissAction::Hide, _) => "result-hide",
            (HostDismissAction::Fold, _) => "result-collapse",
            (HostDismissAction::Expand, _) => "result-expand",
            (HostDismissAction::EdgeHide, _) => "result-edge",
            (_, HostFakeFocusAction::Set) => "result-ff-set",
            (_, HostFakeFocusAction::Clear) => "result-ff-clear",
            _ => "result-none",
        },
    }
}

fn done(
    window: HostDismissAction,
    fake: HostFakeFocusAction,
    listen: bool,
    next_ignore: bool,
    mut path: Vec<String>,
    leaf: &str,
    reason: &str,
) -> HostHideDecision {
    path.push(leaf.into());
    path.push(result_node(window, fake, HostKeyboardRouting::NotApplicable).into());
    HostHideDecision {
        window_action: window,
        fake_focus_action: fake,
        keyboard_routing: HostKeyboardRouting::NotApplicable,
        listen_for_outside_click: listen,
        next_ignore_pointer_until_leave: next_ignore,
        path,
        reason: reason.into(),
    }
}

fn key(routing: HostKeyboardRouting, mut path: Vec<String>, leaf: &str, reason: &str) -> HostHideDecision {
    path.push(leaf.into());
    path.push(result_node(
        if routing == HostKeyboardRouting::SwallowEscapeHide {
            HostDismissAction::Hide
        } else {
            HostDismissAction::None
        },
        if routing == HostKeyboardRouting::SwallowEscapeHide {
            HostFakeFocusAction::Clear
        } else {
            HostFakeFocusAction::Unchanged
        },
        routing,
    )
    .into());
    HostHideDecision {
        window_action: if routing == HostKeyboardRouting::SwallowEscapeHide {
            HostDismissAction::Hide
        } else {
            HostDismissAction::None
        },
        fake_focus_action: if routing == HostKeyboardRouting::SwallowEscapeHide {
            HostFakeFocusAction::Clear
        } else {
            HostFakeFocusAction::Unchanged
        },
        keyboard_routing: routing,
        listen_for_outside_click: true,
        next_ignore_pointer_until_leave: false,
        path,
        reason: reason.into(),
    }
}

fn take(path: &mut Vec<String>) -> Vec<String> {
    std::mem::take(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_dismiss_always_hides() {
        let mut snap = HostHideSnapshot::default();
        snap.is_pinned = true;
        let d = evaluate(HostHideTrigger::UserDismiss, &snap);
        assert_eq!(d.window_action, HostDismissAction::Hide);
        assert!(d.path.iter().any(|p| p == "user-dismiss"));
    }

    #[test]
    fn esc_hides_when_engaged() {
        let mut snap = HostHideSnapshot::default();
        snap.has_fake_focus = true;
        snap.is_escape = true;
        let d = evaluate(HostHideTrigger::KeyDown, &snap);
        assert_eq!(d.window_action, HostDismissAction::Hide);
        assert_eq!(d.keyboard_routing, HostKeyboardRouting::SwallowEscapeHide);
    }

    #[test]
    fn foreground_change_only_edge_hides() {
        let mut snap = HostHideSnapshot::default();
        snap.behavior = DismissBehavior::Hide;
        let stay = evaluate(HostHideTrigger::ForegroundChange, &snap);
        assert_eq!(stay.window_action, HostDismissAction::None);

        snap.behavior = DismissBehavior::EdgeHide;
        snap.pointer_in_host = false;
        snap.can_apply_edge_tuck = true;
        let tuck = evaluate(HostHideTrigger::ForegroundChange, &snap);
        assert_eq!(tuck.window_action, HostDismissAction::EdgeHide);
    }

    #[test]
    fn fold_poll_collapses_when_pointer_leaves() {
        let mut snap = HostHideSnapshot::default();
        snap.behavior = DismissBehavior::Fold;
        snap.pointer_in_host = false;
        snap.is_collapsed = false;
        snap.is_pinned = false;
        let d = evaluate(HostHideTrigger::Poll, &snap);
        assert_eq!(d.window_action, HostDismissAction::Fold);
        assert!(d.next_ignore_pointer_until_leave);
    }

    #[test]
    fn fold_poll_expands_when_pointer_returns() {
        let mut snap = HostHideSnapshot::default();
        snap.behavior = DismissBehavior::Fold;
        snap.pointer_in_host = true;
        snap.is_collapsed = true;
        snap.ignore_pointer_until_leave = false;
        let d = evaluate(HostHideTrigger::Poll, &snap);
        assert_eq!(d.window_action, HostDismissAction::Expand);
        assert!(d.path.iter().any(|p| p == "fold-expand"));
    }

    #[test]
    fn fold_ignores_expand_until_pointer_leaves() {
        let mut snap = HostHideSnapshot::default();
        snap.behavior = DismissBehavior::Fold;
        snap.pointer_in_host = true;
        snap.is_collapsed = true;
        snap.ignore_pointer_until_leave = true;
        let d = evaluate(HostHideTrigger::Poll, &snap);
        assert_eq!(d.window_action, HostDismissAction::None);
        assert!(d.next_ignore_pointer_until_leave);
        assert!(d.path.iter().any(|p| p == "fold-ignore"));
    }

    #[test]
    fn spa_modal_blocks_auto_dismiss() {
        let mut snap = HostHideSnapshot::default();
        snap.behavior = DismissBehavior::Hide;
        snap.spa_modal_open = true;
        snap.has_fake_focus = false;
        let d = evaluate(HostHideTrigger::Poll, &snap);
        assert_eq!(d.window_action, HostDismissAction::None);
        assert!(d.path.iter().any(|p| p == "spa-modal"));
    }

    #[test]
    fn hide_poll_hides_when_disengaged() {
        let mut snap = HostHideSnapshot::default();
        snap.behavior = DismissBehavior::Hide;
        snap.has_fake_focus = false;
        snap.pointer_in_host = false;
        snap.is_pinned = false;
        let d = evaluate(HostHideTrigger::Poll, &snap);
        assert_eq!(d.window_action, HostDismissAction::Hide);
        assert!(d.path.iter().any(|p| p == "hide-hide"));
    }

    #[test]
    fn pinned_outside_click_keeps_window_and_keyboard() {
        for behavior in [DismissBehavior::Hide, DismissBehavior::Fold, DismissBehavior::EdgeHide] {
            let mut snap = HostHideSnapshot::default();
            snap.behavior = behavior;
            snap.is_pinned = true;
            snap.has_fake_focus = true;
            let d = evaluate(HostHideTrigger::OutsideClick, &snap);
            assert_eq!(d.window_action, HostDismissAction::None, "{behavior:?}");
            assert_eq!(d.fake_focus_action, HostFakeFocusAction::Unchanged, "{behavior:?}");
            assert!(!d.listen_for_outside_click, "{behavior:?}");
        }
    }

    #[test]
    fn foreground_change_preserves_keyboard_except_original_edge_dismiss() {
        for behavior in [DismissBehavior::Hide, DismissBehavior::Fold, DismissBehavior::EdgeHide] {
            for pinned in [false, true] {
                let mut snap = HostHideSnapshot::default();
                snap.behavior = behavior;
                snap.is_pinned = pinned;
                snap.has_fake_focus = true;
                snap.is_digit_key = true;
                let key = evaluate(HostHideTrigger::KeyDown, &snap);
                assert_eq!(key.keyboard_routing, HostKeyboardRouting::SwallowDigit);
                let foreground = evaluate(HostHideTrigger::ForegroundChange, &snap);
                if behavior == DismissBehavior::EdgeHide && !pinned {
                    assert_eq!(foreground.window_action, HostDismissAction::EdgeHide);
                    assert_eq!(foreground.fake_focus_action, HostFakeFocusAction::Clear);
                } else {
                    assert_eq!(foreground.window_action, HostDismissAction::None);
                    assert_eq!(foreground.fake_focus_action, HostFakeFocusAction::Unchanged);
                }
            }
        }
    }

    #[test]
    fn pinned_hover_takes_keys_without_persistent_fake_focus() {
        let mut snap = HostHideSnapshot::default();
        snap.pointer_in_host = true;
        snap.is_digit_key = true;
        for pinned in [false, true] {
            snap.is_pinned = pinned;
            assert_eq!(evaluate(HostHideTrigger::KeyDown, &snap).keyboard_routing, HostKeyboardRouting::SwallowDigit);
        }
        snap.pointer_in_host = false;
        assert_eq!(evaluate(HostHideTrigger::KeyDown, &snap).keyboard_routing, HostKeyboardRouting::PassThrough);
    }

    #[test]
    fn fold_auto_collapse_preserves_digit_keyboard_ownership() {
        let mut snap = HostHideSnapshot::default();
        snap.behavior = DismissBehavior::Fold;
        snap.has_fake_focus = true;
        let collapse = evaluate(HostHideTrigger::Poll, &snap);
        assert_eq!(collapse.window_action, HostDismissAction::Fold);
        assert_eq!(collapse.fake_focus_action, HostFakeFocusAction::Unchanged);
        snap.is_collapsed = true;
        snap.is_digit_key = true;
        assert_eq!(evaluate(HostHideTrigger::KeyDown, &snap).keyboard_routing, HostKeyboardRouting::SwallowDigit);
        let outside = evaluate(HostHideTrigger::OutsideClick, &snap);
        assert_eq!(outside.fake_focus_action, HostFakeFocusAction::Clear);
    }

    #[test]
    fn fold_hide_pointer_pad_matches_csharp_area_ext() {
        assert_eq!(AREA_EXT_PX, 8);
        assert!(point_in_window_rect(92, 150, 100, 100, 200, 200, AREA_EXT_PX));
        assert!(!point_in_window_rect(91, 150, 100, 100, 200, 200, AREA_EXT_PX));
        assert!(point_in_window_rect(207, 150, 100, 100, 200, 200, AREA_EXT_PX));
        assert!(!point_in_window_rect(208, 150, 100, 100, 200, 200, AREA_EXT_PX));
        assert!(point_in_window_rect(150, 92, 100, 100, 200, 200, AREA_EXT_PX));
        assert!(!point_in_window_rect(150, 91, 100, 100, 200, 200, AREA_EXT_PX));
        assert!(point_in_window_rect(150, 150, 100, 100, 200, 200, 0));
        assert!(!point_in_window_rect(99, 150, 100, 100, 200, 200, 0));
    }

    #[test]
    fn pointer_hit_mode_matches_csharp_is_pointer_engaging_host() {
        assert_eq!(pointer_area_ext_px(DismissBehavior::Fold), AREA_EXT_PX);
        assert_eq!(pointer_area_ext_px(DismissBehavior::Hide), AREA_EXT_PX);
        assert_eq!(pointer_area_ext_px(DismissBehavior::EdgeHide), 0);
        assert!(!pointer_uses_host_tree(DismissBehavior::Fold));
        assert!(!pointer_uses_host_tree(DismissBehavior::Hide));
        assert!(pointer_uses_host_tree(DismissBehavior::EdgeHide));
    }
}

//! Main-history wiring for the shared native destination picker.
use gpui_rsx::rsx;
use super::*;
use crate::favorite_save::{FavoriteSaveFinish, FavoriteSavePopup, FinishHandler};
use gpui::{canvas, AnimationExt, AnyElement, FocusHandle, Hitbox, HitboxBehavior};

#[path = "favorite_feedback_clock.rs"]
mod favorite_feedback_clock;
use favorite_feedback_clock::FeedbackClock;

pub(super) struct FavoriteFeedback {
    title: String,
    path: Option<String>,
    clock: FeedbackClock,
    hitbox: Rc<std::cell::RefCell<Option<Hitbox>>>,
    focus: FocusHandle,
    #[cfg(debug_assertions)]
    visual_hold: bool,
}

impl ClipboardPreview {
    fn show_favorite_saved(
        &mut self,
        count: usize,
        path: String,
        undo_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.show_notice(
            cliphost_core::trf!("已收藏 {} 项到「{}」", "Saved {} items to {}", count, path),
            cx,
        );
        self.notice_task = None;
        self.favorite_feedback = Some(FavoriteFeedback {
            title: cliphost_core::trf!("已收藏 {} 项", "Saved {} items", count),
            path: Some(path),
            clock: FeedbackClock::new(crate::motion::FAVORITE_FEEDBACK_SAVE, Instant::now()),
            hitbox: Rc::default(),
            focus: cx.focus_handle(),
            #[cfg(debug_assertions)]
            visual_hold: false,
        });
        self.favorite_save_undo = undo_id;
    }

    fn show_favorite_undone(&mut self, cx: &mut Context<Self>) {
        let title = tr("已撤销收藏", "Favorite save undone").to_owned();
        self.show_notice(title.clone(), cx);
        self.notice_task = None;
        self.favorite_feedback = Some(FavoriteFeedback {
            title,
            path: None,
            clock: FeedbackClock::new(crate::motion::FAVORITE_FEEDBACK_UNDO, Instant::now()),
            hitbox: Rc::default(),
            focus: cx.focus_handle(),
            #[cfg(debug_assertions)]
            visual_hold: false,
        });
    }

    fn dismiss_favorite_feedback(&mut self, cx: &mut Context<Self>) {
        self.favorite_feedback_generation = self.favorite_feedback_generation.wrapping_add(1);
        self.favorite_feedback = None;
        self.favorite_save_undo = None;
        self.notice = None;
        self.notice_task = None;
        cx.notify();
    }

    fn watch_favorite_feedback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let generation = self.favorite_feedback_generation;
        self.notice_task = Some(cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(50)).await;
            let keep = this
                .update_in(cx, |this, window, cx| {
                    if this.favorite_feedback_generation != generation {
                        return false;
                    }
                    let bottom = this.tokens.layout.footer_height
                        + this.tokens.layout.content_padding_y
                        + 10.0;
                    let visible = !this.favorites_active
                        && f32::from(window.viewport_size().height)
                            >= bottom + this.tokens.layout.title_bar_height + 60.0;
                    let Some(feedback) = this.favorite_feedback.as_mut() else {
                        return false;
                    };
                    let engaged = visible
                        && ((window.is_window_hovered()
                            && feedback.hitbox.borrow().as_ref().is_some_and(|hitbox| {
                                hitbox.is_hovered(window)
                                    && hitbox.bounds.contains(&window.mouse_position())
                            }))
                            || (window.is_window_active()
                                && feedback.focus.contains_focused(window, cx)));
                    #[cfg(debug_assertions)]
                    let engaged = engaged || feedback.visual_hold;
                    if feedback.clock.observe(Instant::now(), engaged) {
                        this.dismiss_favorite_feedback(cx);
                        return false;
                    }
                    true
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        }));
    }

    pub(super) fn favorite_feedback_view(
        &mut self,
        window: &mut Window,
        tokens: &DesignTokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.favorite_feedback.as_ref()?;
        if self.notice_task.is_none() {
            self.watch_favorite_feedback(window, cx);
        }
        let feedback = self.favorite_feedback.as_ref()?;
        let t = tokens.theme(self.dark);
        let bottom = tokens.layout.footer_height + tokens.layout.content_padding_y + 10.0;
        let height = f32::from(window.viewport_size().height);
        if self.favorites_active || height < bottom + tokens.layout.title_bar_height + 60.0 {
            return None;
        }
        let width = (f32::from(window.viewport_size().width) - 20.0).clamp(0.0, 260.0);
        let generation = self.favorite_feedback_generation;
        let painted_hitbox = feedback.hitbox.clone();
        let header = rsx! {
            <div flex items_center gap={px(6.0)}>
                {self.icon("star", 14.0, color(&t.text_secondary))}
                <div flex_1 min_w_0 text_size={px(12.0)} font_weight={FontWeight::MEDIUM}
                    whitespace_nowrap line_height={relative(1.5)}>
                    {feedback.title.clone()}
                </div>
            </div>
        }
            .when(self.favorite_save_undo.is_some(), |row| {
                row.child(
                    rsx! {
                        <div id={"history-undo-favorite"} flex_none h={px(22.0)} px={px(6.0)}
                            flex items_center rounded={px(4.0)} text_size={px(11.0)} border_1
                            border_color={color(&t.border)} bg={color(&t.secondary)}
                            cursor_pointer text_color={color(&t.text)} />
                    }
                        .hover(|style| style.bg(color(&t.hover)))
                        .child(tr("撤销", "Undo"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if this.favorite_feedback_generation == generation {
                                this.undo_favorite_save(cx);
                            }
                        })),
                )
            })
            .child(
                rsx! {
                    <div id={"favorite-feedback-close"} flex_none size={px(18.0)} flex
                        items_center justify_center rounded={px(3.0)} cursor_pointer />
                }
                    .hover(|style| style.bg(color(&t.hover)))
                    .child(self.icon("close", 12.0, color(&t.text_muted)))
                    .tooltip(preference_tooltip(
                        tr("关闭收藏提示", "Dismiss favorite notification").into(),
                        t.clone(),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if this.favorite_feedback_generation == generation {
                            this.dismiss_favorite_feedback(cx);
                        }
                    })),
            );
        let card = rsx! {
            <div id={"favorite-feedback-card"} relative w_full px={px(10.0)} py={px(8.0)} flex
                flex_col gap={px(2.0)} rounded={px(6.0)} border_1
                border_color={color(&t.border_secondary)} bg={color(&t.surface)}
                text_size={px(12.0)} text_color={color(&t.text)} shadow_lg occlude />
        }
            .track_focus(&feedback.focus)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(header)
            .when_some(feedback.path.clone(), |card, path| {
                card.child(
                    rsx! {
                        <div id={"favorite-feedback-path"} ml={px(20.0)} min_w_0
                            text_size={px(11.0)} truncate line_height={relative(1.5)}
                            text_color={color(&t.text_secondary)}>
                            {path.clone()}
                        </div>
                    }
                        .tooltip(preference_tooltip(path.into(), t.clone())),
                )
            })
            .child(
                canvas(
                    |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |_, hitbox, _, _| {
                        *painted_hitbox.borrow_mut() = Some(hitbox);
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        Some(
            rsx! {
                <div id={"favorite-save-feedback"} absolute right={px(10.0)} bottom={px(bottom)}
                    w={px(width)} />
            }
                .child(card.with_animation(
                    ("favorite-feedback-enter", generation),
                    crate::motion::enter(crate::motion::POPUP_ENTER, crate::motion::EASE_OUT),
                    |card, progress| card.opacity(progress).top(px(5.0 * (1.0 - progress))),
                ))
                .into_any_element(),
        )
    }

    #[cfg(debug_assertions)]
    pub fn hold_favorite_feedback_visual(&mut self, cx: &mut Context<Self>) -> bool {
        if !matches!(self.model, UiModel::Fixture(_)) {
            return false;
        }
        let Some(feedback) = self.favorite_feedback.as_mut() else {
            return false;
        };
        feedback.visual_hold = true;
        cx.notify();
        true
    }

    #[cfg(debug_assertions)]
    pub fn seed_favorite_feedback_visual(
        &mut self,
        target: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !matches!(self.model, UiModel::Fixture(_)) {
            return false;
        }
        if let Some(path) = target {
            self.show_favorite_saved(3, path.to_owned(), Some("fixture-only-no-save".into()), cx);
        } else {
            self.show_favorite_undone(cx);
        }
        cx.notify();
        true
    }

    pub(super) fn cancel_favorite_hover(&mut self) {
        self.favorite_hover_task = None;
        self.favorite_hover_pending = None;
        self.favorite_hover_epoch = self.favorite_hover_epoch.wrapping_add(1);
    }

    pub(super) fn favorite_save_invoked(
        &mut self,
        id: String,
        anchor: Option<(i32, i32, i32, i32)>,
        hovered: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        if hovered == Some(false) {
            if self
                .favorite_hover_pending
                .as_ref()
                .is_some_and(|(pending, _)| pending == &id)
            {
                self.cancel_favorite_hover();
            }
            return;
        }
        let ids = if self.selected_ids.len() > 1 && self.selected_ids.contains(&id) {
            self.selection()
        } else {
            vec![id.clone()]
        };
        if hovered.is_none() {
            self.cancel_favorite_hover();
            self.save_favorite_to_root(ids, cx);
            return;
        }
        if self.favorite_root_saving || self.favorite_trigger_id.as_ref() == Some(&id) {
            return;
        }
        self.cancel_favorite_hover();
        let epoch = self.favorite_hover_epoch;
        let scope = (self.generation, self.native_session);
        self.favorite_hover_pending = Some((id.clone(), epoch));
        self.favorite_hover_task = Some(cx.spawn(async move |this, cx| {
            Timer::after(crate::motion::FAVORITE_SAVE_OPEN).await;
            let _ = this.update(cx, |this, cx| {
                if this.favorite_hover_pending.as_ref() != Some(&(id.clone(), epoch))
                    || this.favorite_hover_epoch != epoch
                    || scope != (this.generation, this.native_session)
                    || this.generation != this.page_generation
                    || this.favorite_root_saving
                {
                    return;
                }
                this.favorite_hover_pending = None;
                this.favorite_hover_task = None;
                this.open_favorite_save_at(ids, Some(id), anchor, cx);
            });
        }));
    }

    fn save_favorite_to_root(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        if self.favorite_root_saving
            || self.exit_pending
            || self.favorites_active
            || self.history_dialog
            || self.process_editor.is_some()
            || self.process_open_pending
            || self.search.read(cx).is_composing()
            || ids.is_empty()
        {
            return;
        }
        let Some(host) = self.model.host() else {
            return;
        };
        self.close_menu(cx);
        self.cancel_native_hover();
        self.favorite_root_saving = true;
        self.action_reply_sequence = self.action_reply_sequence.wrapping_add(1);
        let reply_scope = self.action_reply_sequence;
        let count = ids.len();
        let operation = format!("favorite-root-{}", uuid::Uuid::new_v4());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let tree = host.favor_tree()?;
                    host.favorite_save_history_to_root(&json!({"ids":ids,
                    "expectedRevision":tree["dragRevision"],"operationId":operation}))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.favorite_root_saving = false;
                if this.exit_pending || this.action_reply_sequence != reply_scope {
                    return;
                }
                match result {
                    Ok(result) => {
                        this.error = None;
                        let label = result["savedFolder"]["pathLabel"]
                            .as_str()
                            .unwrap_or(tr("总目录", "Root"));
                        let undo_id = result["undoId"]
                            .as_str()
                            .filter(|id| !id.is_empty())
                            .map(str::to_owned);
                        this.show_favorite_saved(count, label.to_owned(), undo_id, cx);
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn open_favorite_save(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        self.open_favorite_save_at(ids, None, None, cx);
    }

    fn open_favorite_save_at(
        &mut self,
        ids: Vec<String>,
        trigger_id: Option<String>,
        anchor: Option<(i32, i32, i32, i32)>,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.model.host() else {
            return;
        };
        if self.exit_pending
            || self.favorites_active
            || self.history_dialog
            || self.process_editor.is_some()
            || self.process_open_pending
            || self.search.read(cx).is_composing()
            || self.favorite_root_saving
            || ids.is_empty()
        {
            return;
        }
        if trigger_id.is_some() && anchor.is_none() {
            self.error = Some(
                tr(
                    "无法定位收藏入口，请重试。",
                    "Could not locate the favorite button. Please retry.",
                )
                .into(),
            );
            cx.notify();
            return;
        }
        let Some(point) = anchor
            .map(|(x, y, width, height)| (x + width, y + height))
            .or(self.menu_anchor)
            .or_else(platform::cursor_position)
        else {
            return;
        };
        self.close_fifo(cx);
        self.close_menu(cx);
        self.list_settings = None;
        self.filter_panel = None;
        self.more_menu = false;
        let lease = crate::menu::next_lease();
        if !host.acquire_native_context_menu(lease, "clipboard") {
            return;
        }
        if anchor.is_some() && !host.set_native_context_menu_trigger(lease, anchor) {
            host.release_native_context_menu(lease);
            self.error = Some(
                tr(
                    "无法定位收藏入口，请重试。",
                    "Could not locate the favorite button. Please retry.",
                )
                .into(),
            );
            cx.notify();
            return;
        }
        self.favorite_lease = Some(lease);
        self.favorite_trigger_id = trigger_id;
        self.action_reply_sequence = self.action_reply_sequence.wrapping_add(1);
        let reply_scope = self.action_reply_sequence;
        self.cancel_native_hover();
        #[cfg(windows)]
        if let Some(native) = self.interactions.borrow().as_ref() {
            let _ = native.set_menu_open(true);
        }
        let owner = cx.weak_entity();
        let on_finish: FinishHandler = Rc::new(move |lease, finish, cx| {
            let _ = owner.update(cx, |this, cx| {
                // A committed batch still reports after an outside dismissal,
                // but an older reply must never close or overwrite a new menu.
                if this.favorite_lease == Some(lease) {
                    this.close_menu(cx);
                }
                if this.exit_pending || reply_scope != this.action_reply_sequence {
                    return;
                }
                if let FavoriteSaveFinish::Saved {
                    count,
                    path_label,
                    undo_id,
                } = finish
                {
                    this.error = None;
                    this.show_favorite_saved(count, path_label, undo_id, cx);
                }
                cx.notify();
            });
        });
        let tokens = self.tokens.clone();
        let dark = self.dark;
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |this, cx| {
                if this.favorite_lease != Some(lease) || !host.native_context_menu_is_current(lease)
                {
                    return;
                }
                let mut popup = this.favorite_popup;
                let result = FavoriteSavePopup::show(
                    &mut popup,
                    &host,
                    host.host_hwnd(),
                    lease,
                    point,
                    anchor,
                    ids,
                    tokens,
                    dark,
                    on_finish,
                    cx,
                );
                this.favorite_popup = popup;
                if let Err(error) = result {
                    this.close_menu(cx);
                    this.error = Some(error);
                    cx.notify();
                }
            });
        });
        cx.notify();
    }

    pub(super) fn undo_favorite_save(&mut self, cx: &mut Context<Self>) {
        let Some(undo_id) = self.favorite_save_undo.take() else {
            return;
        };
        let Some(host) = self.model.host() else {
            return;
        };
        self.action_reply_sequence = self.action_reply_sequence.wrapping_add(1);
        let reply_scope = self.action_reply_sequence;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.favorite_drag_undo(&json!({"undoId":undo_id})) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.exit_pending || this.action_reply_sequence != reply_scope {
                    return;
                }
                match result {
                    Ok(_) => this.show_favorite_undone(cx),
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

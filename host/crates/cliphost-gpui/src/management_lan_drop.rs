//! History-only LAN drops. Native callbacks capture a target and enqueue work;
//! they never borrow GPUI or read arbitrary external drop payloads as messages.
use super::*;
use crate::favorite_drag::{ContentDrag, DragSource};

fn history_admitted(
    active: bool,
    companion: bool,
    running: bool,
    target: bool,
    blocked: bool,
    drag: &ContentDrag,
) -> bool {
    active
        && companion
        && running
        && target
        && !blocked
        && drag.active()
        && matches!(drag.source, DragSource::History { .. })
        && !drag.item_ids.is_empty()
        && drag.item_ids.iter().all(|id| !id.is_empty())
}

#[cfg(windows)]
struct PendingReply {
    ticket: crate::drag_drop::DropTicket,
    conversation: String,
    reply: tokio::sync::oneshot::Receiver<Result<Value, String>>,
}

pub(super) struct IncomingHistory {
    #[cfg(windows)]
    adapter: crate::drag_drop::DragDropWindow,
    #[cfg(windows)]
    replies: mpsc::Receiver<PendingReply>,
    #[cfg(windows)]
    pending: Vec<PendingReply>,
    #[cfg(windows)]
    errors: mpsc::Receiver<crate::drag_drop::DropError>,
}

impl IncomingHistory {
    #[cfg(windows)]
    fn attach(hwnd: isize) -> Result<Self, String> {
        use std::rc::Rc;
        let (sender, replies) = mpsc::channel();
        let (errors_sender, errors) = mpsc::channel();
        let adapter = crate::drag_drop::DragDropWindow::attach_stamped(
            hwnd,
            Rc::new(move |event| {
                event.ticket.verify()?;
                let token = event
                    .formats
                    .internal_drag_token
                    .as_deref()
                    .ok_or("Drag a clipboard history item into the conversation.")?;
                let accepted = crate::favorite_drag::accept_native(
                    token,
                    event.ticket.hwnd(),
                    event.screen_point,
                    false,
                )?;
                let conversation = accepted
                    .lan_conversation
                    .ok_or("The drop is not a LAN conversation.")?;
                if !accepted.drag.claim() {
                    return Err("This history drag was already accepted or cancelled.".into());
                }
                accepted.drag.mark_native_accepted();
                let (_, reply) = SharedHistorySend::accept_to(
                    &accepted.host,
                    accepted.drag.item_ids,
                    &conversation,
                );
                let _ = sender.send(PendingReply {
                    ticket: event.ticket,
                    conversation,
                    reply,
                });
                Ok(())
            }),
            Rc::new(|_| {}),
            Rc::new(move |error| {
                let _ = errors_sender.send(error);
            }),
        )?;
        Ok(Self {
            adapter,
            replies,
            pending: Vec::new(),
            errors,
        })
    }

    fn sync(&self, session: u64, active: bool) {
        #[cfg(windows)]
        self.adapter.sync_session(session, active);
        #[cfg(not(windows))]
        let _ = (session, active);
    }

    fn drain(&mut self) -> Vec<(String, Result<Value, String>)> {
        let mut results = Vec::new();
        #[cfg(windows)]
        {
            self.pending.extend(self.replies.try_iter());
            self.pending.retain_mut(|pending| {
                let result = match pending.reply.try_recv() {
                    Ok(result) => result,
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return true,
                    Err(_) => Err("LAN send worker ended without a reply.".into()),
                };
                if pending.ticket.is_current() {
                    results.push((pending.conversation.clone(), result));
                }
                false
            });
            for error in self
                .errors
                .try_iter()
                .filter(|error| error.ticket.is_current())
            {
                results.push((String::new(), Err(error.message)));
            }
        }
        results
    }
}

impl LanManagementPage {
    pub(crate) fn focus_composer(&self, window: &mut Window, cx: &gpui::App) {
        self.composer.read(cx).focus_handle().focus(window);
    }

    pub(crate) fn set_companion(&mut self, companion: bool, cx: &mut Context<Self>) {
        self.companion = companion;
        self.history_drop_hovered = false;
        self.sync_history_drop();
        cx.notify();
    }

    pub(super) fn can_receive_history(&self) -> bool {
        (self.active || self.fixture)
            && self.companion
            && !self.exit_pending
            && self.status["running"] == true
            && conversation_has_target(&self.peers, &self.selected)
            && !self.devices
            && !self.sessions
            && self.preview.is_none()
    }

    /// Read under the caller's existing App borrow, then enqueue against this
    /// owned destination immediately. Never defer a click to read it later.
    pub(crate) fn history_click_target(&self) -> Result<String, String> {
        if !self.can_receive_history() || self.attach_menu || self.selected.trim().is_empty() {
            return Err(tr(
                "互传暂未就绪，请先完成右侧操作。",
                "LAN transfer is not ready. Finish the action in the side pane first.",
            ).into());
        }
        Ok(self.selected.clone())
    }

    pub(super) fn sync_history_drop(&mut self) {
        let accepting = self.can_receive_history();
        if !accepting {
            self.history_drop_hovered = false;
            crate::favorite_drag::clear_target_hwnd(self.hwnd);
        }
        #[cfg(windows)]
        if self.companion
            && !self.fixture
            && !self.incoming_history_failed
            && self.incoming_history.is_none()
            && self.hwnd != 0
            && !cliphost_core::is_headless()
        {
            match IncomingHistory::attach(self.hwnd) {
                Ok(incoming) => self.incoming_history = Some(incoming),
                Err(error) => {
                    self.incoming_history_failed = true;
                    log::warn!("LAN history drop adapter: {error}");
                }
            }
        }
        if let Some(incoming) = &self.incoming_history {
            incoming.sync(self.incoming_history_session, accepting);
        }
    }

    pub(super) fn history_drop_allowed(&self, drag: &ContentDrag) -> bool {
        history_admitted(
            self.active || self.fixture,
            self.companion,
            self.status["running"] == true,
            conversation_has_target(&self.peers, &self.selected),
            self.exit_pending || self.devices || self.sessions || self.preview.is_some(),
            drag,
        )
    }

    pub(super) fn offer_history_drop(
        &self,
        drag: &ContentDrag,
        bounds: gpui::Bounds<gpui::Pixels>,
        window: &Window,
    ) {
        if self.history_drop_allowed(drag) {
            crate::favorite_drag::offer_lan_target(
                drag,
                &self.selected,
                bounds,
                window,
                self.host.clone(),
            );
        } else {
            crate::favorite_drag::clear_target(window);
        }
    }

    pub(super) fn accept_history_drop(
        &mut self,
        drag: &ContentDrag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history_drop_hovered = false;
        crate::favorite_drag::clear_target(window);
        if !self.history_drop_allowed(drag) || !drag.claim() {
            cx.notify();
            return;
        }
        let conversation = self.selected.clone();
        if self.fixture {
            self.notice = Some(format!("{} · {}", conversation, drag.item_ids.len()));
            cx.notify();
            return;
        }
        let (_, reply) =
            SharedHistorySend::accept_to(&self.host, drag.item_ids.clone(), &conversation);
        cx.spawn_in(window, async move |this, cx| {
            let result = reply
                .await
                .unwrap_or_else(|_| Err("LAN history send worker unavailable.".into()));
            let _ = this.update_in(cx, |this, window, cx| {
                if this.active && this.selected == conversation {
                    match result {
                        Ok(_) => this.refresh_chat(window, cx, true),
                        Err(error) => this.report_error(error, cx),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn drain_history_drop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let results = self
            .incoming_history
            .as_mut()
            .map(|incoming| incoming.drain())
            .unwrap_or_default();
        for (conversation, result) in results {
            if !self.active || !conversation.is_empty() && conversation != self.selected {
                continue;
            }
            self.history_drop_hovered = false;
            match result {
                Ok(_) => self.refresh_chat(window, cx, true),
                Err(error) => self.report_error(error, cx),
            }
            cx.notify();
        }
        if self.history_drop_hovered && !crate::favorite_drag::active(cx) {
            self.history_drop_hovered = false;
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drop_admission_requires_history_active_target_and_no_modal() {
        let drag = ContentDrag::history(vec!["item".into()], "rev".into(), 1, 2, "item".into());
        assert!(history_admitted(true, true, true, true, false, &drag));
        for (active, dock, running, target, blocked) in [
            (false, true, true, true, false),
            (true, false, true, true, false),
            (true, true, false, true, false),
            (true, true, true, false, false),
            (true, true, true, true, true),
        ] {
            assert!(!history_admitted(
                active, dock, running, target, blocked, &drag
            ));
        }
        assert!(drag.claim());
        assert!(!history_admitted(true, true, true, true, false, &drag));
        let favorite = ContentDrag::favorite(
            None,
            vec!["item".into()],
            vec![],
            "rev".into(),
            1,
            2,
            "item".into(),
        );
        assert!(!history_admitted(true, true, true, true, false, &favorite));
    }
}

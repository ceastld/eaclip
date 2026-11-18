use super::*;
use crate::favorite_drag::{ContentDrag, HistoryFolderDrop};

impl ClipboardPreview {
    fn phrase_drop_folder(&self) -> &str {
        if self.phrases.group.is_empty() {
            &self.phrases.source
        } else {
            &self.phrases.group
        }
    }

    fn accepts_phrase_drop(&self, drag: &ContentDrag) -> bool {
        self.phrase_enabled()
            && !self.favorites_active
            && !self.exit_pending
            && !self.list_overlay_open()
            && self.menu_lease.is_none()
            && self.favorite_lease.is_none()
            && drag.active()
            && drag.session == self.native_session
            && self
                .history_content_drag
                .as_ref()
                .is_some_and(|source| source.operation_id == drag.operation_id)
            && self.phrases.ready
            && self.phrases.tree.folder_exists(self.phrase_drop_folder())
    }

    pub(super) fn phrase_drop_zone(
        &self,
        t: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let drag = self
            .history_content_drag
            .as_ref()
            .filter(|drag| self.accepts_phrase_drop(drag))?
            .clone();
        let folder = self.phrase_drop_folder();
        let label = if folder.is_empty() {
            tr("总目录", "Root")
        } else {
            self.phrases.tree.path_label(folder)
        };
        let destination = HistoryFolderDrop {
            folder: folder.to_owned(),
            revision: self.phrases.tree.revision.clone(),
        };
        let painted = destination.clone();
        let host = self.model.host()?;
        let accent = color(&t.accent);
        let hint = cliphost_core::trf!("松开以收藏到「{}」", "Drop to save to {}", label);
        Some(rsx! {
            <div id={"phrase-history-drop"} absolute inset_0 occlude flex items_center justify_center
                p_2 rounded_md border_2 border_color={accent.opacity(0.4)} bg={accent.opacity(0.05)}>
                <div max_w_full min_w_0 flex items_center gap_1 px_2 py_1 rounded_md shadow_sm
                    bg={color(&t.surface)} text_color={color(&t.text)} text_size={px(12.)}>
                    {self.icon("star", 14., accent)}
                    <div min_w_0 line_clamp={2}>{hint}</div>
                </div>
            </div>
        }
        .drag_over::<ContentDrag>(move |style, _, _, _| style.border_color(accent).bg(accent.opacity(0.15)))
        .child(gpui::canvas(|_, _, _| (), move |bounds, _, window, _| {
            painted.offer(&drag, bounds, window, host.clone());
        }).absolute().inset_0())
        .on_drop(cx.listener(move |this, drag: &ContentDrag, window, cx| {
            if !this.accepts_phrase_drop(drag) || this.phrase_drop_folder() != destination.folder || !drag.claim() {
                return;
            }
            HistoryFolderDrop::clear_offer(window);
            let Some(host) = this.model.host() else { return };
            let accepted = match crate::native_interactions::accept_content_write() {
                Ok(accepted) => accepted,
                Err(error) => { this.error = Some(error); cx.notify(); return; }
            };
            let drag = drag.clone();
            let target = destination.clone();
            this.action_reply_sequence = this.action_reply_sequence.wrapping_add(1);
            let scope = this.action_reply_sequence;
            cx.spawn(async move |this, cx| {
                let result = cx.background_executor().spawn(async move {
                    let result = target.save(&host, &drag);
                    accepted.finish(&result);
                    result
                }).await;
                let _ = this.update(cx, |this, cx| {
                    if this.exit_pending || this.action_reply_sequence != scope { return; }
                    match result {
                        Ok(saved) => this.phrase_drop_saved(saved, cx),
                        Err(error) => { this.error = Some(error); this.invalidate_phrases(cx); }
                    }
                    cx.notify();
                });
            }).detach();
            cx.notify();
        })).into_any_element())
    }

    pub(in crate::view) fn phrase_drop_saved(&mut self, saved: Value, cx: &mut Context<Self>) {
        if let Some(ids) = saved["createdIds"].as_array() {
            self.phrases.reveal = ids.first().and_then(Value::as_str).map(str::to_owned);
            let path = saved["savedFolder"]["pathLabel"]
                .as_str()
                .unwrap_or(tr("收藏", "Favorites"));
            self.show_favorite_saved(
                ids.len(),
                path.to_owned(),
                saved["undoId"].as_str().map(str::to_owned),
                cx,
            );
        }
        self.error = None;
        self.invalidate_phrases(cx);
    }
}

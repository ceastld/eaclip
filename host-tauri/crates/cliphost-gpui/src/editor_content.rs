//! The confirmed editor workbench content layouts. Controls and events retain
//! the native editor's existing source documents, sessions and IME guards.

use gpui_rsx::rsx;
use super::*;

impl EditorWindow {
    fn content_note(&self, label: impl Into<gpui::SharedString>) -> gpui::Div {
        rsx! {
            <div flex_none text_size={px(self.tokens.typography.small_font_size)}
                text_color={color(&self.tokens.theme(self.dark).text_muted)}>
                {label.into()}
            </div>
        }
    }

    fn content_icon(&self, name: &str, width: f32) -> crate::crisp_svg::CrispSvg {
        crate::crisp_svg::svg()
            .path(self.tokens.icons.get(name).cloned().unwrap_or_default())
            .size(px(width))
            .flex_none()
            .text_color(color(&self.tokens.theme(self.dark).text_muted))
    }

    fn content_text_field(&self, label: &'static str, cx: &App) -> AnyElement {
        let theme = self.tokens.theme(self.dark);
        let value = self.body_input.read(cx).text();
        let (characters, lines) = style::text_stats(value);
        rsx! {
            <div flex flex_col flex_1 min_h={px(0.)} min_w={px(0.)}
                gap={px(style::metric("contentFieldGap", 7.))}>
                <div flex flex_none items_center justify_between
                    gap={px(style::metric("contentFieldGap", 7.))}
                    text_size={px(self.tokens.typography.small_font_size)}
                    text_color={color(&theme.text_secondary)}>
                    {label}
                    <div text_color={color(&theme.text_muted)}>
                        {format!(
                        "{characters} {} · {lines} {}",
                        tr("字符", "characters"),
                        tr("行", "lines")
                    )}
                    </div>
                </div>
                <div flex flex_col flex_1 min_h={px(style::metric("contentBodyMinHeight", 72.))}
                    min_w={px(0.)} border_1 border_color={color(&theme.border)}
                    rounded={px(style::metric("contentControlRadius", 5.))} overflow_hidden>
                    {self.body_input.clone()}
                </div>
            </div>
        }
            .into_any_element()
    }

    fn content_reference(&self, icon: &str, title: String, subtitle: &'static str) -> AnyElement {
        let theme = self.tokens.theme(self.dark);
        rsx! {
            <div flex flex_none items_center gap={px(style::metric("contentGap", 10.))}
                p={px(style::metric("referencePadding", 11.))} min_w={px(0.)} border_1
                border_color={color(&theme.border)}
                rounded={px(style::metric("contentCardRadius", 6.))} bg={color(&theme.secondary)}>
                <div flex items_center justify_center flex_none
                    size={px(style::metric("referenceIconSize", 32.))} border_1
                    border_color={color(&theme.border)}
                    rounded={px(style::metric("contentCardRadius", 6.))} bg={color(&theme.surface)}>
                    {self.content_icon(icon, 16.)}
                </div>
                <div flex flex_col flex_1 min_w={px(0.)} gap={px(style::metric("fieldGap", 4.))}>
                    <div font_weight={FontWeight::MEDIUM}>
                        {title}
                    </div>
                    {self.content_note(subtitle)}
                </div>
            </div>
        }
            .into_any_element()
    }

    pub(super) fn content_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.tokens.theme(self.dark).clone();
        let kind = self.kind().to_string();
        let enabled = self.detail.is_some()
            && !self.loading
            && !self.released
            && !self.composing(cx)
            && self.rich_input.read(cx).unapplied_error().is_none();
        let mut content = rsx! {
            <div flex flex_col flex_1 min_h={px(0.)} min_w={px(0.)}
                gap={px(style::metric("contentGap", 10.))} />
        };

        if self
            .detail
            .as_ref()
            .is_some_and(|detail| detail["isFolder"] == true)
            && self.target.is_favorite()
        {
            return self.folder_content_view();
        }
        if matches!(kind.as_str(), "HtmlClipItem" | "QQRichTextItem") {
            let tabs = if kind == "HtmlClipItem" {
                vec![
                    ("editor-rich-tab", tr("可视化", "Visual"), ContentTab::Rich),
                    (
                        "editor-html-tab",
                        tr("HTML 源码", "HTML source"),
                        ContentTab::Html,
                    ),
                    (
                        "editor-text-tab",
                        tr("纯文本", "Plain text"),
                        ContentTab::Body,
                    ),
                ]
            } else {
                vec![
                    (
                        "editor-im-rich-tab",
                        tr("图文", "Rich content"),
                        ContentTab::Rich,
                    ),
                    (
                        "editor-im-source-tab",
                        tr("XML 源码", "XML source"),
                        ContentTab::Qq,
                    ),
                ]
            };
            let mut bar = rsx! {
                <div flex flex_none gap={px(style::metric("contentTabGap", 2.))}
                    p={px(style::metric("contentTabPadding", 3.))} border_1
                    border_color={color(&theme.border)}
                    rounded={px(style::metric("contentCardRadius", 6.))}
                    bg={color(&theme.secondary)} />
            };
            for (id, label, tab) in tabs {
                let selected = self.tab == tab;
                let hovered = color(if selected {
                    &theme.surface
                } else {
                    &theme.hover
                });
                bar = bar.child(
                    self.button_with_background(
                        id,
                        label,
                        color(if selected {
                            &theme.surface
                        } else {
                            &theme.secondary
                        }),
                        hovered,
                    )
                    .relative()
                    .child(self.paint_metadata.control(id, enabled))
                    .h(px(style::metric("contentTabHeight", 27.)))
                    .py(px(0.))
                    .px(px(style::metric("controlPaddingX", 10.)))
                    .flex()
                    .items_center()
                    .text_color(color(if selected {
                        &theme.text
                    } else {
                        &theme.text_secondary
                    }))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.change_tab(tab, window, cx)),
                    ),
                );
            }
            content = content.child(rsx! {
                <div flex flex_none>
                    {bar}
                </div>
            });
        }

        match kind.as_str() {
            "FileClipItem" => return self.paths_content_view(cx),
            "ImageClipItem" => return self.image_content_view(window, cx),
            "TextClipItem" => {
                content = content.child(self.content_text_field(tr("文本内容", "Text content"), cx))
            }
            "HtmlClipItem" | "QQRichTextItem" => {
                if self.tab == ContentTab::Rich {
                    content = content.child(
                        rsx! {
                            <div flex flex_col flex_1 min_h={px(0.)} min_w={px(0.)}>
                                {self.rich_input.clone()}
                            </div>
                        },
                    );
                } else {
                    let label = match self.tab {
                        ContentTab::Html => tr("HTML 源码", "HTML source"),
                        ContentTab::Qq => tr("XML 源码", "XML source"),
                        _ => tr("纯文本内容", "Plain text content"),
                    };
                    content = content.child(self.content_text_field(label, cx));
                    if kind == "HtmlClipItem" && self.tab == ContentTab::Body {
                        content = content.child(self.content_note(tr(
                            "修改纯文本后，将不再保留 HTML 格式与来源页面。",
                            "Editing plain text removes HTML formatting and its source page.",
                        )));
                    }
                }
            }
            "LinkClipItem" if self.target.is_favorite() => {
                content = content
                    .child(
                        rsx! {
                            <div flex flex_none flex_col
                                gap={px(style::metric("contentFieldGap", 7.))}
                                text_color={color(&theme.text_secondary)}>
                                {"URL"}
                                <div h={px(style::metric("contentInputHeight", 34.))} border_1
                                    border_color={color(&theme.border)}
                                    rounded={px(style::metric("contentControlRadius", 5.))}
                                    overflow_hidden>
                                    {self.body_input.clone()}
                                </div>
                            </div>
                        },
                    )
                    .child(self.content_note(tr(
                        "上方标题用作链接名称。",
                        "The title above is used as the link name.",
                    )));
            }
            "CmdClipItem" if self.target.is_favorite() => {
                content = content.child(self.content_text_field(tr("命令", "Command"), cx));
            }
            "ActionClipItem" if self.target.is_favorite() => {
                let name = self
                    .detail
                    .as_ref()
                    .map(|detail| text(detail, "actionTitle"))
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| tr("Quicker 动作", "Quicker action").into());
                content = content
                    .child(self.content_reference(
                        "play",
                        name,
                        tr("动作引用 · 只读", "Action reference · Read only"),
                    ))
                    .child(self.content_text_field(tr("运行参数", "Execution parameters"), cx));
            }
            "StepClipItem" if self.target.is_favorite() => {
                let count = serde_json::from_str::<Value>(&self.draft.step_data)
                    .ok()
                    .and_then(|value| value.as_array().map(Vec::len));
                let label = count
                    .map(|count| format!("{count} {}", tr("个步骤", "steps")))
                    .unwrap_or_else(|| tr("步骤数据", "Step data").into());
                content = content
                    .child(self.content_reference(
                        "list",
                        label,
                        tr(
                            "编辑下方数据，保留步骤原有结构",
                            "Edit the data below, retaining the step structure",
                        ),
                    ))
                    .child(self.content_text_field(tr("步骤数据", "Step data"), cx));
            }
            _ => {
                content = content
                    .child(self.content_note(tr(
                        "此类型当前仅支持修改标题。",
                        "This type currently supports title edits only.",
                    )))
                    .child(self.content_text_field(
                        tr("原始信息 · 只读", "Original data · Read only"),
                        cx,
                    ));
            }
        }
        content.into_any_element()
    }

    fn folder_content_view(&self) -> AnyElement {
        let theme = self.tokens.theme(self.dark);
        let children = self
            .detail
            .as_ref()
            .and_then(|detail| detail["children"].as_array());
        let count = children.map_or(0, Vec::len);
        let mut list = rsx! {
            <div id={"editor-folder-children"} flex flex_col flex_1 min_h={px(0.)} min_w={px(0.)} />
        }
            .overflow_y_scroll()
            .border_1()
            .border_color(color(&theme.border))
            .rounded(px(style::metric("contentCardRadius", 6.)))
            .bg(color(&theme.surface));
        if let Some(children) = children.filter(|children| !children.is_empty()) {
            for child in children {
                let id = child.as_str().unwrap_or_default().to_string();
                let summary = self
                    .folder_children
                    .iter()
                    .find(|value| value["id"].as_str() == Some(id.as_str()));
                let name = summary
                    .and_then(|value| value["title"].as_str().or_else(|| value["name"].as_str()))
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&id)
                    .to_string();
                let icon = if summary.is_some_and(|value| value["isFolder"] == true) {
                    "folder-closed"
                } else {
                    match summary
                        .and_then(|value| value["kind"].as_str())
                        .unwrap_or_default()
                    {
                        "FavoriteClipItem" | "Favorites" => "folder-closed",
                        "ImageClipItem" | "Image" => "image",
                        "FileClipItem" | "File" => "file",
                        "Directory" => "folder-open",
                        "LinkClipItem" | "Link" => "link",
                        "CmdClipItem" | "Cmd" => "keyboard",
                        "ActionClipItem" | "QuickerAction" => "play",
                        "StepClipItem" | "QuickerActionSteps" => "list",
                        "HtmlClipItem" | "Html" => "code",
                        "QQRichTextItem" | "QQRichText" => "clipboard",
                        _ => "text",
                    }
                };
                list = list.child(
                    rsx! {
                        <div flex flex_none items_center gap={px(style::metric("contentGap", 10.))}
                            min_h={px(style::metric("folderRowHeight", 44.))}
                            py={px(style::metric("folderRowPaddingY", 7.))}
                            px={px(style::metric("folderRowPaddingX", 12.))} border_b_1
                            border_color={color(&theme.border)}>
                            {self.content_icon(icon, 16.)}
                            <div flex_1 min_w={px(0.)}>
                                {name}
                            </div>
                            {self.content_note(tr("只读", "Read only"))}
                        </div>
                    },
                );
            }
        } else {
            list = list.child(
                rsx! {
                    <div flex flex_col flex_1
                        min_h={px(style::metric("contentEmptyMinHeight", 90.))} items_center
                        justify_center gap={px(style::metric("contentEmptyGap", 12.))}>
                        {self.content_icon("folder-open", 28.)}
                        {self.content_note(tr("收藏夹为空", "This folder is empty"))}
                    </div>
                },
            );
        }
        rsx! {
            <div flex flex_col flex_1 min_h={px(0.)} min_w={px(0.)}
                gap={px(style::metric("contentGap", 10.))}>
                <div flex flex_none justify_between>
                    {tr("收藏夹内容", "Folder contents")}
                    {self.content_note(format!("{count} {}", tr("项", "items")))}
                </div>
                {list}
            </div>
        }
            .child(self.content_note(tr(
                "此处可修改收藏夹标题；内容顺序请在收藏列表中整理。",
                "Edit the folder title here; organize its contents in Favorites.",
            )))
            .into_any_element()
    }

    pub(super) fn paths_content_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = self.tokens.clone();
        let theme = tokens.theme(self.dark);
        let count = self
            .draft
            .file_paths
            .iter()
            .filter(|path| !path.trim().is_empty())
            .count();
        let toolbar = rsx! {
            <div flex flex_none flex_wrap items_center justify_between
                gap={px(style::metric("contentControlGap", 5.))}>
                <div text_color={color(&theme.text_secondary)}>
                    {format!(
                        "{}  {count} {}",
                        tr("文件路径", "File paths"),
                        tr("项", "items")
                    )}
                </div>
            </div>
        }
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(style::metric("contentControlGap", 5.)))
                    .child(
                        self.button("paths-sort", tr("排序", "Sort"), false)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.composing(cx) {
                                    return;
                                }
                                this.draft.file_paths.sort_by_key(|path| {
                                    path.rsplit(['/', '\\'])
                                        .next()
                                        .unwrap_or(path)
                                        .to_lowercase()
                                });
                                this.changed(cx);
                                this.rebuild_paths(window, cx);
                            })),
                    )
                    .child(
                        self.button("paths-bulk", tr("批量输入", "Bulk input"), false)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.composing(cx) {
                                    return;
                                }
                                this.file_bulk_open = !this.file_bulk_open;
                                this.sync_body(window, cx);
                                cx.notify();
                            })),
                    )
                    .child(self.button("path-add", tr("添加", "Add"), false).on_click(
                        cx.listener(|this, _, window, cx| {
                            if this.composing(cx) {
                                return;
                            }
                            this.draft.file_paths.push(String::new());
                            this.changed(cx);
                            this.rebuild_paths(window, cx);
                            if let Some(input) = this.path_inputs.last() {
                                window.focus(&input.read(cx).focus_handle());
                            }
                        }),
                    )),
            );
        let import = rsx! {
            <div flex flex_none flex_wrap gap={px(style::metric("contentControlGap", 5.))} />
        }
            .child(
                self.button(
                    "paths-clipboard",
                    tr("从剪贴板导入", "Import clipboard"),
                    false,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.composing(cx) {
                        return;
                    }
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        this.append_paths(parse_file_path_lines(&text), window, cx);
                    }
                })),
            )
            .child(
                self.button(
                    "paths-pick",
                    if self.path_picker.is_pending() {
                        tr("选择中…", "Choosing…")
                    } else {
                        tr("选择文件", "Choose files")
                    },
                    false,
                )
                .on_click(cx.listener(|this, _, window, cx| this.pick_paths(window, cx))),
            );
        let mut content = rsx! {
            <div flex flex_col flex_1 min_h={px(0.)} min_w={px(0.)}
                gap={px(style::metric("contentGap", 10.))}>
                {toolbar}
                {import}
            </div>
        };
        if self.file_bulk_open {
            content = content
                .child(self.content_text_field(tr("批量添加路径", "Add paths in bulk"), cx))
                .child(
                    rsx! {
                        <div flex flex_none flex_wrap justify_between items_center
                            gap={px(style::metric("fileBulkGap", 9.))}>
                            {self.content_note(tr(
                            "每行一条，重复路径会自动跳过。",
                            "One path per line; duplicates are skipped.",
                        ))}
                        </div>
                    }
                        .child(
                            self.button("paths-apply-bulk", tr("添加到列表", "Add to list"), false)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if this.composing(cx) {
                                        return;
                                    }
                                    this.append_paths(
                                        parse_file_path_lines(&this.file_bulk),
                                        window,
                                        cx,
                                    );
                                })),
                        ),
                );
        } else {
            let mut rows = rsx! {
                <div id={"editor-file-path-list"} flex flex_col flex_1 min_h={px(0.)}
                    min_w={px(0.)} />
            }
                .overflow_y_scroll()
                .track_scroll(&self.path_scroll)
                .smooth_wheel("editor-file-path-list", &self.path_scroll, cx)
                .gap(px(style::metric("fileListGap", 8.)))
                .p(px(1.));
            if self.path_inputs.is_empty() {
                rows = rows.child(
                    rsx! {
                        <div flex flex_col flex_1
                            min_h={px(style::metric("contentEmptyMinHeight", 90.))} items_center
                            justify_center gap={px(style::metric("contentEmptyGap", 12.))}>
                            {self.content_icon("file", 26.)}
                            {self.content_note(tr("尚未添加文件路径", "No file paths yet"))}
                        </div>
                    },
                );
            }
            for (index, input) in self.path_inputs.iter().cloned().enumerate() {
                let path = self.draft.file_paths[index].clone();
                let name = path
                    .rsplit(['/', '\\'])
                    .next()
                    .filter(|name| !name.is_empty())
                    .unwrap_or(tr("新路径", "New path"))
                    .to_string();
                let drag = FilePathDrag {
                    target: self.target.clone(),
                    generation: self.generation,
                    version: self.edit_version,
                    index,
                    label: path,
                    position: gpui::point(px(0.), px(0.)),
                    background: color(&theme.secondary),
                    foreground: color(&theme.text),
                };
                let mut row = rsx! {
                    <div id={("file-path-row", index)} flex flex_col flex_none min_w={px(0.)}
                        py={px(style::metric("contentInputPaddingY", 9.))}
                        px={px(style::metric("controlPaddingX", 10.))}
                        gap={px(style::metric("fileHeadGap", 6.))} border_1
                        border_color={color(&theme.border)}
                        rounded={px(style::metric("contentCardRadius", 6.))}
                        bg={color(&theme.surface)} />
                }
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if this.composing(cx) || !event.keystroke.modifiers.alt {
                            return;
                        }
                        let next = match event.keystroke.key.as_str() {
                            "up" if index > 0 => index - 1,
                            "down" if index + 1 < this.draft.file_paths.len() => index + 1,
                            _ => return,
                        };
                        cx.stop_propagation();
                        this.draft.file_paths.swap(index, next);
                        this.changed(cx);
                        this.rebuild_paths(window, cx);
                        window.focus(&this.path_inputs[next].read(cx).focus_handle());
                    }))
                    .on_drag_move(cx.listener(
                        move |this, event: &gpui::DragMoveEvent<FilePathDrag>, _, cx| {
                            if !event.bounds.contains(&event.event.position) {
                                return;
                            }
                            let before = event.event.position.y
                                < event.bounds.origin.y + event.bounds.size.height / 2.;
                            if this.path_drop != Some((index, before)) {
                                this.path_drop = Some((index, before));
                                cx.notify();
                            }
                        },
                    ))
                    .on_drop(cx.listener(move |this, drag: &FilePathDrag, window, cx| {
                        if this.composing(cx)
                            || drag.target != this.target
                            || drag.generation != this.generation
                            || drag.version != this.edit_version
                        {
                            return;
                        }
                        let before = this
                            .path_drop
                            .take()
                            .filter(|(target, _)| *target == index)
                            .map(|(_, before)| before)
                            .unwrap_or(true);
                        if let Some(next) =
                            reorder_file_path(&mut this.draft.file_paths, drag.index, index, before)
                        {
                            this.changed(cx);
                            this.rebuild_paths(window, cx);
                            window.focus(&this.path_inputs[next].read(cx).focus_handle());
                        }
                    }));
                if let Some((target, before)) =
                    self.path_drop.filter(|(target, _)| *target == index)
                {
                    let _ = target;
                    row = row
                        .when(before, |row| row.border_t_2())
                        .when(!before, |row| row.border_b_2())
                        .border_color(color(&theme.accent));
                }
                let mut actions = div()
                    .flex()
                    .flex_none()
                    .gap(px(style::metric("fileActionGap", 2.)));
                for (id, label, direction) in [
                    ("path-up", "↑", -1isize),
                    ("path-down", "↓", 1isize),
                    ("path-remove", "×", 0isize),
                ] {
                    actions = actions.child(
                        self.button_with_background(
                            (id, index),
                            label,
                            color(&theme.surface),
                            color(&theme.hover),
                        )
                        .size(px(style::metric("fileActionSize", 23.)))
                        .p(px(0.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(color(&theme.text_muted))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if this.composing(cx) {
                                    return;
                                }
                                if direction == 0 {
                                    if index < this.draft.file_paths.len() {
                                        this.draft.file_paths.remove(index);
                                        this.changed(cx);
                                        this.rebuild_paths(window, cx);
                                    }
                                } else {
                                    let next = index as isize + direction;
                                    if next >= 0 && (next as usize) < this.draft.file_paths.len() {
                                        this.draft.file_paths.swap(index, next as usize);
                                        this.changed(cx);
                                        this.rebuild_paths(window, cx);
                                    }
                                }
                            },
                        )),
                    );
                }
                rows = rows.child(
                    row.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(style::metric("contentFieldGap", 7.)))
                            .child(
                                self.button_with_background(
                                    ("path-drag", index),
                                    "≡",
                                    color(&theme.surface),
                                    color(&theme.hover),
                                )
                                .size(px(style::metric("fileActionSize", 23.)))
                                .p(px(0.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor(gpui::CursorStyle::OpenHand)
                                .on_drag(
                                    drag,
                                    |drag: &FilePathDrag, position, _, cx| {
                                        let mut drag = drag.clone();
                                        drag.position = position;
                                        cx.new(|_| drag)
                                    },
                                ),
                            )
                            .child(self.content_icon("file", 15.))
                            .child(
                                rsx! {
                                    <div flex_1 min_w={px(0.)} text_ellipsis
                                        font_weight={FontWeight::MEDIUM}>
                                        {name}
                                    </div>
                                },
                            )
                            .child(actions),
                    )
                    .child(
                        rsx! {
                            <div min_w={px(0.)} h={px(style::metric("filePathHeight", 29.))}
                                rounded={px(style::metric("contentControlRadius", 5.))}
                                bg={color(&theme.secondary)} overflow_hidden>
                                {input}
                            </div>
                        },
                    ),
                );
            }
            content = content.child(rows);
        }
        content
            .child(self.content_note(tr(
                "修改或移除路径只影响本条记录，不会移动或删除文件。",
                "Changing paths affects this record; it does not move or delete files.",
            )))
            .into_any_element()
    }

    pub(super) fn image_content_view(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.tokens.theme(self.dark).clone();
        let note = self.content_note(tr(
            "滚轮缩放，拖动查看，双击适配。图片仅支持修改标题。",
            "Scroll to zoom, drag to pan, double-click to fit. Only the title is editable.",
        ));
        let Some(path) = self.image.clone() else {
            return rsx! {
                <div flex flex_col flex_1 min_h={px(0.)} gap={px(style::metric("contentGap", 10.))}>
                    <div flex flex_col flex_1
                        min_h={px(style::metric("contentEmptyMinHeight", 90.))} items_center
                        justify_center gap={px(style::metric("contentEmptyGap", 12.))}>
                        {self.content_icon("image", 28.)}
                        {self.content_note(tr(
                            "图片无法读取，原始内容仍会保留。",
                            "Image unavailable; the original content is retained.",
                        ))}
                    </div>
                    {note}
                </div>
            }
                .into_any_element();
        };
        let viewport_width = (f32::from(window.viewport_size().width)
            - style::metric("contentPaddingX", 16.) * 2.)
            .max(100.);
        let viewport_height = (f32::from(window.viewport_size().height)
            - style::metric("headerHeight", 36.)
            - style::metric("pageTabsHeight", 40.)
            - style::metric("footerHeight", 50.)
            - style::metric("contentPaddingY", 12.) * 2.
            - style::metric("controlHeight", 30.)
            - self.tokens.typography.small_font_size * 3.5
            - style::metric("contentGap", 10.) * 3.)
            .max(style::metric("imageViewportMinHeight", 100.));
        let Some((width, height)) = self
            .image_size
            .filter(|(width, height)| *width > 0 && *height > 0)
        else {
            return rsx! {
                <div flex flex_col flex_1 min_h={px(0.)} gap={px(style::metric("contentGap", 10.))}>
                    {img(path)
                        .w_full()
                        .h(px(viewport_height))
                        .object_fit(gpui::ObjectFit::Contain)}
                    {note}
                </div>
            }
                .into_any_element();
        };
        let fit = (viewport_width / width as f32)
            .min(viewport_height / height as f32)
            .min(1.);
        let scale = *self.image_scale.get_or_insert(fit);
        let rendered_width = width as f32 * scale;
        let rendered_height = height as f32 * scale;
        let bounds = self.image_bounds.clone();
        let checker_size = style::metric("imageCheckerSize", 20.) / 2.;
        let checker_color = color(&theme.secondary);
        let mut toolbar = rsx! {
            <div flex items_center gap={px(style::metric("imageToolbarGap", 3.))}
                py={px(style::metric("imageToolbarPaddingY", 5.))}
                px={px(style::metric("imageToolbarPaddingX", 6.))} border_1
                border_color={color(&theme.border)}
                rounded={px(style::metric("contentCardRadius", 6.))} bg={color(&theme.surface)}
                occlude />
        };
        for (id, label, mode) in [
            ("image-fit", tr("适配", "Fit"), 0),
            ("image-native", "1:1", 1),
            ("image-minus", "−", 2),
            ("image-plus", "+", 3),
        ] {
            toolbar = toolbar.child(
                self.button_with_background(id, label, color(&theme.surface), color(&theme.hover))
                    .h(px(style::metric("contentTabHeight", 27.)))
                    .py(px(0.))
                    .px(px(style::metric("imageToolPaddingX", 7.)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        match mode {
                            0 => {
                                this.image_scale = Some(fit);
                                this.image_pan = (0., 0.);
                            }
                            1 => {
                                this.image_scale = Some(1.);
                                this.image_pan = (0., 0.);
                            }
                            2 => {
                                this.image_scale =
                                    Some((this.image_scale.unwrap_or(1.) / 1.2).max(0.02))
                            }
                            _ => {
                                this.image_scale =
                                    Some((this.image_scale.unwrap_or(1.) * 1.2).min(32.))
                            }
                        }
                        cx.notify();
                    })),
            );
        }
        toolbar = toolbar.child(
            rsx! {
                <div min_w={px(style::metric("imagePercentMinWidth", 38.))}
                    text_size={px(self.tokens.typography.small_font_size)}
                    text_color={color(&theme.text_muted)} text_center>
                    {format!("{:.0}%", scale * 100.)}
                </div>
            },
        );
        let viewport = rsx! {
            <div id={"image-pan-zoom"} relative w_full h={px(viewport_height)} min_h={px(0.)}
                border_1 border_color={color(&theme.border)}
                rounded={px(style::metric("contentCardRadius", 6.))} bg={color(&theme.surface)}
                overflow_hidden />
        }
            .cursor(gpui::CursorStyle::OpenHand)
            .child(
                gpui::canvas(
                    move |area, _, _| {
                        if let Ok(mut bounds) = bounds.lock() {
                            *bounds = Some(area);
                        }
                    },
                    move |area, _, window, _| {
                        let columns = (f32::from(area.size.width) / checker_size).ceil() as usize;
                        let rows = (f32::from(area.size.height) / checker_size).ceil() as usize;
                        window.with_content_mask(
                            Some(gpui::ContentMask { bounds: area }),
                            |window| {
                                for y in 0..rows {
                                    for x in 0..columns {
                                        if (x + y) % 2 == 0 {
                                            window.paint_quad(gpui::fill(
                                                Bounds::new(
                                                    area.origin
                                                        + gpui::point(
                                                            px(x as f32 * checker_size),
                                                            px(y as f32 * checker_size),
                                                        ),
                                                    size(px(checker_size), px(checker_size)),
                                                ),
                                                checker_color,
                                            ));
                                        }
                                    }
                                }
                            },
                        );
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(
                img(path)
                    .absolute()
                    .left(px((viewport_width - rendered_width) / 2. + self.image_pan.0))
                    .top(px(
                        (viewport_height - rendered_height) / 2. + self.image_pan.1
                    ))
                    .w(px(rendered_width))
                    .h(px(rendered_height)),
            )
            .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                cx.stop_propagation();
                let delta = f32::from(event.delta.pixel_delta(px(16.)).y);
                let old = this.image_scale.unwrap_or(1.);
                let new = (old * (-delta * 0.002).exp()).clamp(0.02, 32.);
                if let Some(bounds) = this.image_bounds.lock().ok().and_then(|bounds| *bounds) {
                    let anchor = (
                        f32::from(event.position.x - bounds.origin.x - bounds.size.width / 2.),
                        f32::from(event.position.y - bounds.origin.y - bounds.size.height / 2.),
                    );
                    this.image_pan = zoom_image_pan(this.image_pan, anchor, new / old);
                }
                this.image_scale = Some(new);
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    if event.click_count == 2 {
                        this.image_scale = Some(fit);
                        this.image_pan = (0., 0.);
                        this.image_drag = None;
                    } else {
                        this.image_drag = Some(event.position);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if !event.dragging() {
                    this.image_drag = None;
                    return;
                }
                if let Some(previous) = this.image_drag.replace(event.position) {
                    this.image_pan.0 += f32::from(event.position.x - previous.x);
                    this.image_pan.1 += f32::from(event.position.y - previous.y);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.image_drag = None),
            )
            .child(
                rsx! {
                    <div absolute bottom={px(style::metric("imageToolbarOffset", 10.))}
                        left={px(0.)} right={px(0.)} flex justify_center>
                        {toolbar}
                    </div>
                },
            );
        rsx! {
            <div flex flex_col flex_1 min_h={px(0.)} min_w={px(0.)}
                gap={px(style::metric("contentGap", 10.))}>
                {viewport}
                {note}
            </div>
        }
            .into_any_element()
    }
}

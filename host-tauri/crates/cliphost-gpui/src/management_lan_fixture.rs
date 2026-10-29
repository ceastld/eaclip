//! Debug-only LAN visual harness. All displayed DTOs are fixed offline samples;
//! the production page owns rendering. No NativeHost, tray, API, RPC, singleton,
//! clipboard monitoring, hooks or LAN transport is started by this entry point.

#[cfg(windows)]
pub use native::run;

#[cfg(windows)]
mod native {
    use std::{
        fs::{self, File, OpenOptions},
        io::{Seek, SeekFrom, Write},
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant},
    };

    use anyhow::{anyhow, ensure, Context as _};
    use cliphost_core::{
        i18n::{self, LanguageMode},
        AppPaths, HostState,
    };
    use gpui::{
        div, prelude::*, px, size, Application, Bounds, Context, Entity, KeyDownEvent,
        KeyUpEvent, Keystroke, Modifiers, PlatformInput, Timer, Window, WindowBounds,
        WindowOptions,
    };
    use serde_json::{json, Value};
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    use super::super::lan::{LanFixtureData, LanManagementPage};
    use crate::{
        platform,
        preview::{DesignTokens, TOKENS_JSON},
        view::color,
        DesignAssets,
    };

    const IMAGE: &[u8] = include_bytes!("../../../../ui-design/assets/clip-main-thumbnail.png");
    // Metadata only. HostState::open does not bind this or any other API port.
    const UNBOUND_API_PORT: u16 = 29065;
    const PERFORMANCE_WARMUP_MS: u64 = 2_000;
    const PERFORMANCE_PULSE_MS: u64 = 50;
    const PERFORMANCE_PULSES: u64 = 20;
    const PERFORMANCE_IDLE_MS: u64 = 2_000;
    const KEYBOARD_WARMUP_MS: u64 = 1_000;
    const KEYBOARD_STEP_MS: u64 = 100;
    const SHORT_TEXT_CASES: [(&str, &str); 5] = [
        ("short-cjk-ascii", "测试123"),
        ("short-cjk", "测"),
        ("short-ascii", "Test123"),
        ("short-emoji", "😀👍🏽🙂"),
        (
            "short-multiline",
            "这是一条较长的中文消息，包含 ASCII 12345，用于确认自动换行。\n第二行 END。",
        ),
    ];

    #[derive(Clone)]
    struct Options {
        output: PathBuf,
        scenario: String,
        theme: String,
        width: f32,
        height: f32,
        devices: bool,
        devices_bottom: bool,
        companion: bool,
        icon_override: String,
        conversation: String,
        content: String,
        exit_after_ms: u64,
        performance_pulse: bool,
        keyboard_contract: bool,
        scroll_probe: bool,
    }

    impl Options {
        fn parse(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<Option<Self>> {
            let mut options = Self {
                output: PathBuf::new(),
                scenario: "ready".into(),
                theme: "dark".into(),
                width: 900.,
                height: 640.,
                devices: false,
                devices_bottom: false,
                companion: false,
                icon_override: "auto".into(),
                conversation: "work".into(),
                content: "full".into(),
                exit_after_ms: 4_000,
                performance_pulse: false,
                keyboard_contract: false,
                scroll_probe: false,
            };
            let mut arguments = arguments.into_iter();
            while let Some(argument) = arguments.next() {
                if matches!(argument.as_str(), "--help" | "-h") {
                    println!(
                        "Offline production LAN page preview (Windows Debug)\n\
                        --output EXISTING_ABSOLUTE_REPO_TMP_RUN (required; fresh run/data)\n\
                        --scenario ready|unpaired|disabled|error --theme dark|light\n\
                        --width 320..1600 DIP --height DIP --devices --devices-bottom --companion\n\
                        --icon-override auto|desktop|laptop|phone\n\
                        --conversation work|home|all --content full|media|batch|short-text|import-error\n\
                        short-text requires ready and height >= 640 (all five messages painted)\n\
                        --exit-after-ms 1000..60000\n\
                        --performance-pulse (requires ready, no devices, exit >= 8000 ms)\n\
                        --keyboard-contract (offline GPUI event dispatch; ready, no devices, exit >= 8000 ms)\n\
                        Always no-activate; real sends/configuration actions are blocked."
                    );
                    return Ok(None);
                }
                if argument == "--devices" {
                    options.devices = true;
                    continue;
                }
                if argument == "--devices-bottom" {
                    options.devices = true;
                    options.devices_bottom = true;
                    continue;
                }
                if argument == "--companion" {
                    options.companion = true;
                    continue;
                }
                if argument == "--performance-pulse" {
                    options.performance_pulse = true;
                    continue;
                }
                if argument == "--scroll-probe" {
                    options.scroll_probe = true;
                    continue;
                }
                if argument == "--keyboard-contract" {
                    options.keyboard_contract = true;
                    continue;
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| anyhow!("{argument} needs a value"))?;
                match argument.as_str() {
                    "--output" => options.output = PathBuf::from(value),
                    "--scenario" => {
                        ensure!(
                            ["ready", "unpaired", "disabled", "error"].contains(&value.as_str()),
                            "Invalid scenario"
                        );
                        options.scenario = value;
                    }
                    "--theme" => {
                        ensure!(["dark", "light"].contains(&value.as_str()), "Invalid theme");
                        options.theme = value;
                    }
                    "--width" => {
                        options.width = value.parse()?;
                        ensure!((320.0..=1_600.0).contains(&options.width), "Invalid width");
                    }
                    "--height" => {
                        options.height = value.parse()?;
                        ensure!(
                            (360.0..=1_500.0).contains(&options.height),
                            "Invalid height"
                        );
                    }
                    "--icon-override" => {
                        ensure!(
                            ["auto", "desktop", "laptop", "phone"].contains(&value.as_str()),
                            "Invalid icon override"
                        );
                        options.icon_override = value;
                    }
                    "--conversation" => {
                        ensure!(
                            ["work", "home", "all"].contains(&value.as_str()),
                            "Invalid conversation"
                        );
                        options.conversation = value;
                    }
                    "--content" => {
                        ensure!(
                            ["full", "media", "batch", "short-text", "import-error"].contains(&value.as_str()),
                            "Invalid content"
                        );
                        options.content = value;
                    }
                    "--exit-after-ms" => {
                        options.exit_after_ms = value.parse()?;
                        ensure!(
                            (1_000..=60_000).contains(&options.exit_after_ms),
                            "Invalid exit interval"
                        );
                    }
                    _ => return Err(anyhow!("Unknown option: {argument}")),
                }
            }
            ensure!(
                !options.output.as_os_str().is_empty(),
                "--output is required"
            );
            ensure!(options.output.is_absolute(), "--output must be absolute");
            ensure!(
                options.content != "short-text"
                    || (options.scenario == "ready" && options.height >= 640.),
                "Short-text regression requires ready scenario and height >= 640 so all five messages paint"
            );
            ensure!(
                !options.performance_pulse
                    || (options.scenario == "ready"
                        && !options.devices
                        && options.exit_after_ms >= 8_000),
                "Performance pulse requires ready scenario, no devices overlay and exit >= 8000 ms"
            );
            ensure!(
                !options.keyboard_contract
                    || (options.scenario == "ready"
                        && !options.devices
                        && !options.performance_pulse
                        && options.exit_after_ms >= 8_000),
                "Keyboard contract requires ready scenario, no devices overlay, no performance pulse and exit >= 8000 ms"
            );
            Ok(Some(options))
        }
    }

    struct Locations {
        run: PathBuf,
        data: PathBuf,
        image: PathBuf,
        report: File,
    }

    fn locations(requested: &Path) -> anyhow::Result<Locations> {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .ok_or_else(|| anyhow!("Source repository is unavailable"))?
            .canonicalize()?;
        let scratch = repository
            .join(".tmp")
            .canonicalize()
            .context("Create the repository .tmp run first")?;
        ensure!(
            scratch.parent() == Some(repository.as_path())
                && scratch.file_name().is_some_and(|name| name == ".tmp"),
            "Repository .tmp must not escape through a junction"
        );
        let run = requested
            .canonicalize()
            .context("--output must be an existing directory")?;
        ensure!(
            run.is_dir() && run != scratch && run.starts_with(&scratch),
            "--output must be a run directory inside repository .tmp"
        );
        // create_dir (not create_dir_all) refuses previous data, junctions and
        // SQLite sidecars. Never reopen a prior fixture or product database.
        let data = run.join("data");
        fs::create_dir(&data).context("The fixture requires a fresh run/data")?;
        ensure!(
            data.canonicalize()?.parent() == Some(run.as_path()),
            "Fixture data escaped its run"
        );
        let profile = run.join("profile");
        fs::create_dir(&profile).context("The fixture requires a fresh run/profile")?;
        let roaming = profile.join("Roaming");
        let local = profile.join("Local");
        fs::create_dir(&roaming)?;
        fs::create_dir(&local)?;
        // Called before HostState/Application creates any worker. In particular,
        // the headless core's initial recent-files snapshot stays in this run.
        std::env::set_var("APPDATA", &roaming);
        std::env::set_var("LOCALAPPDATA", &local);
        std::env::set_var("BETTERCLIP_TAURI_DATA_ROOT", &data);
        let image = run.join("fixture-image.png");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&image)?
            .write_all(IMAGE)?;
        let report = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(run.join("window-report.json"))?;
        Ok(Locations {
            run,
            data,
            image,
            report,
        })
    }

    fn peer(id: &str, alias: &str, kind: &str, paired: bool, online: bool, icon: &str) -> Value {
        json!({"deviceId":id, "alias":alias, "deviceKind":if icon == "auto" {kind} else {icon},
            "detectedDeviceKind":kind, "deviceKindOverride":icon,
            "paired":paired, "online":online, "addresses":[], "port":29766})
    }

    fn attachment(id: &str, name: &str, mime: &str, size: u64, path: &str) -> Value {
        json!({"id":id,"name":name,"mimeType":mime,"sizeBytes":size,"localPath":path,
            "sha256":"offline-fixture","status":"ready"})
    }

    fn delivery(
        peer: &str,
        alias: &str,
        status: &str,
        transferred: u64,
        total: u64,
        error: &str,
    ) -> Value {
        json!({"targetDeviceId":peer,"targetAlias":alias,"status":status,
            "transferredBytes":transferred,"totalBytes":total,"lastError":error})
    }

    fn message(
        id: &str,
        conversation: &str,
        direction: &str,
        at: &str,
        text: &str,
        status: &str,
        attachments: Vec<Value>,
        deliveries: Vec<Value>,
    ) -> Value {
        json!({"id":id,"conversationId":conversation,"direction":direction,
            "targetKind":if conversation == "all" { "group" } else { "direct" },
            "senderDeviceId":if direction == "incoming" {"fixture-work"} else {"fixture-local"},
            "senderAlias":if direction == "incoming" {"工作笔记本"} else {"办公台式机"},
            "createdAtUtc":at,"text":text,"status":status,"isRead":true,
            "attachments":attachments,"deliveries":deliveries})
    }

    fn short_text_messages(conversation: &str) -> Vec<Value> {
        let (target, alias) = if conversation == "dm:fixture-home" {
            ("fixture-home", "家中电脑")
        } else {
            ("fixture-work", "工作笔记本")
        };
        SHORT_TEXT_CASES
            .iter()
            .enumerate()
            .map(|(index, (id, text))| {
                message(
                    id,
                    conversation,
                    "outgoing",
                    &format!("2026-10-19T21:22:{:02}Z", index * 8),
                    text,
                    "delivered",
                    vec![],
                    vec![delivery(target, alias, "delivered", 0, 0, "")],
                )
            })
            .collect()
    }

    fn fixture(options: &Options, locations: &Locations) -> LanFixtureData {
        let enabled = options.scenario != "disabled";
        let paired = options.scenario != "unpaired";
        let mut status = json!({"enabled":enabled,"running":enabled && options.scenario != "error",
            "deviceId":"fixture-local","deviceAlias":"办公台式机",
            "deviceKind":if options.icon_override == "auto" { "desktop" } else { &options.icon_override },
            "detectedDeviceKind":"desktop", "deviceKindOverride":options.icon_override,
            "port":29766,"inboxFolder":locations.data.join("LanShare/Inbox"),
            "maxIncomingFileSizeBytes":2147483648u64,"maxParallelDeliveries":3,"receivedFilesToHistory":true,
            "pairingCode":{"code":"731 904","expiresAtUtc":(chrono::Utc::now()+chrono::Duration::minutes(5)).to_rfc3339()},
            "certificateFingerprint":"OFFLINE FIXTURE · 38:AF:42:9D:6A:11"});
        if options.scenario == "error" {
            status["error"] = json!("端口 29766 被占用，请更换端口后重试。");
        }
        let peers = vec![
            peer(
                "fixture-work",
                "工作笔记本",
                "laptop",
                paired,
                true,
                &options.icon_override,
            ),
            peer("fixture-home", "家中电脑", "desktop", paired, false, "auto"),
            peer(
                "fixture-meeting",
                "会议室电脑",
                "desktop",
                false,
                true,
                "auto",
            ),
            peer("fixture-phone", "我的手机", "phone", false, true, "auto"),
        ];
        let conversations = if paired {
            vec![
                json!({"id":"all","kind":"group","title":"全部已配对设备","online":true,"unreadCount":0}),
                json!({"id":"dm:fixture-work","kind":"direct","title":"工作笔记本","deviceId":"fixture-work","online":true,"unreadCount":0,"lastMessagePreview":"图文、独立文件与传输状态"}),
                json!({"id":"dm:fixture-home","kind":"direct","title":"家中电脑","deviceId":"fixture-home","online":false,"unreadCount":0,"lastMessagePreview":"设备离线，等待重新连接"}),
            ]
        } else {
            vec![
                json!({"id":"all","kind":"group","title":"全部已配对设备","online":false,"unreadCount":0}),
            ]
        };
        let selected = if !paired || options.conversation == "all" {
            "all".to_string()
        } else {
            format!("dm:fixture-{}", options.conversation)
        };
        let image_path = locations.image.display().to_string();
        let image = attachment(
            "fixture-image",
            "界面草图.png",
            "image/png",
            IMAGE.len() as u64,
            &image_path,
        );
        let pdf = attachment(
            "fixture-pdf",
            "讨论稿.pdf",
            "application/pdf",
            2_411_725,
            "",
        );
        let archive = attachment(
            "fixture-zip",
            "设计资源包.zip",
            "application/zip",
            18_874_368,
            "",
        );
        let messages = if !paired {
            vec![]
        } else if options.content == "short-text" {
            short_text_messages(&selected)
        } else if options.content == "import-error" {
            let mut failed = message("fixture-history-import", &selected, "incoming", "2026-10-19T21:22:00Z",
                "文件已接收，可以先打开或定位。", "received", vec![pdf.clone(), archive.clone()], vec![]);
            failed["historyImportRequested"] = json!(true);
            failed["historyImportCompleted"] = json!(false);
            failed["historyImportError"] = json!("剪贴板历史暂时无法写入。修复后可重试加入，无需重新传输文件。");
            vec![failed]
        } else if options.content == "media" {
            vec![
                message(
                    "media-1",
                    &selected,
                    "incoming",
                    "2026-10-17T21:22:00Z",
                    "图片和文件独立排列。",
                    "received",
                    vec![image.clone()],
                    vec![],
                ),
                message(
                    "media-2",
                    &selected,
                    "incoming",
                    "2026-10-17T21:22:08Z",
                    "",
                    "received",
                    vec![image.clone()],
                    vec![],
                ),
                message(
                    "media-3",
                    &selected,
                    "outgoing",
                    "2026-10-17T21:22:16Z",
                    "",
                    "delivered",
                    vec![pdf.clone()],
                    vec![delivery(
                        "fixture-work",
                        "工作笔记本",
                        "delivered",
                        2_411_725,
                        2_411_725,
                        "",
                    )],
                ),
            ]
        } else if options.content == "batch" {
            vec![message(
                "batch-1",
                &selected,
                "outgoing",
                "2026-10-17T21:24:00Z",
                "两份附件使用本次传输的共同状态。",
                "sending",
                vec![pdf, archive],
                vec![delivery(
                    "fixture-work",
                    "工作笔记本",
                    "sending",
                    9_152_020,
                    21_286_093,
                    "",
                )],
            )]
        } else if options.conversation == "home" {
            vec![
                message(
                    "home-1",
                    &selected,
                    "outgoing",
                    "2026-10-16T02:40:00Z",
                    "明天把文件带回家。",
                    "delivered",
                    vec![],
                    vec![delivery("fixture-home", "家中电脑", "delivered", 0, 0, "")],
                ),
                message(
                    "home-2",
                    &selected,
                    "outgoing",
                    "2026-10-17T21:18:00Z",
                    "设备上线后自动继续。",
                    "queued",
                    vec![pdf],
                    vec![delivery(
                        "fixture-home",
                        "家中电脑",
                        "queued",
                        0,
                        2_411_725,
                        "",
                    )],
                ),
            ]
        } else if options.conversation == "all" {
            vec![message(
                "all-1",
                &selected,
                "outgoing",
                "2026-10-17T21:10:00Z",
                "给大家的讨论稿。",
                "partial",
                vec![pdf],
                vec![
                    delivery(
                        "fixture-work",
                        "工作笔记本",
                        "delivered",
                        2_411_725,
                        2_411_725,
                        "",
                    ),
                    delivery(
                        "fixture-home",
                        "家中电脑",
                        "failed",
                        0,
                        2_411_725,
                        "设备离线",
                    ),
                ],
            )]
        } else {
            vec![
                message(
                    "work-1",
                    &selected,
                    "incoming",
                    "2026-10-17T21:20:00Z",
                    "今天的界面草图发你了。",
                    "received",
                    vec![],
                    vec![],
                ),
                message(
                    "work-2",
                    &selected,
                    "outgoing",
                    "2026-10-17T21:21:00Z",
                    "收到了，稍后看。",
                    "delivered",
                    vec![],
                    vec![delivery(
                        "fixture-work",
                        "工作笔记本",
                        "delivered",
                        0,
                        0,
                        "",
                    )],
                ),
                message(
                    "work-3",
                    &selected,
                    "incoming",
                    "2026-10-17T21:22:00Z",
                    "图片在这里。",
                    "received",
                    vec![image],
                    vec![],
                ),
                message(
                    "work-4",
                    &selected,
                    "outgoing",
                    "2026-10-17T21:24:00Z",
                    "",
                    "failed",
                    vec![pdf],
                    vec![delivery(
                        "fixture-work",
                        "工作笔记本",
                        "failed",
                        0,
                        2_411_725,
                        "连接中断，请重试。",
                    )],
                ),
                message(
                    "work-5",
                    &selected,
                    "outgoing",
                    "2026-10-17T21:24:08Z",
                    "这份是完整资源包。",
                    "sending",
                    vec![archive],
                    vec![delivery(
                        "fixture-work",
                        "工作笔记本",
                        "sending",
                        8_116_978,
                        18_874_368,
                        "",
                    )],
                ),
            ]
        };
        LanFixtureData {
            status,
            device_sync: json!({"enabled":enabled, "textEnabled":true, "imageEnabled":true, "peers":[
                {"deviceId":"fixture-work", "sendEnabled":true, "receiveEnabled":true,
                    "textEnabled":true, "imageEnabled":true, "applyClipboard":true, "saveHistory":true,
                    "online":true,"lastError":""},
                {"deviceId":"fixture-home", "sendEnabled":true, "receiveEnabled":true,
                    "textEnabled":true, "imageEnabled":true, "applyClipboard":true, "saveHistory":true,
                    "online":false,"lastError":""}
            ]}),
            peers,
            conversations,
            messages,
            selected,
            devices: options.devices,
        }
    }

    #[derive(Default)]
    struct PerformancePulse {
        sent: u64,
        last_pulse: Option<Instant>,
        typing_sent: u64,
        last_typing: Option<Instant>,
        input_dispatch_total: Duration,
        input_dispatch_max: Duration,
        after_typing_at: Option<Instant>,
        warm: Option<Value>,
        after: Option<Value>,
        after_typing: Option<Value>,
        idle: Option<Value>,
    }

    impl PerformancePulse {
        fn report(&self, enabled: bool, current: Value) -> Value {
            let warm = self.warm.as_ref().unwrap_or(&Value::Null);
            let after = self.after.as_ref().unwrap_or(&Value::Null);
            let after_typing = self.after_typing.as_ref().unwrap_or(&Value::Null);
            let idle = self.idle.as_ref().unwrap_or(&Value::Null);
            let complete = self.idle.is_some();
            let typing_parent_renders = after_typing["pageRenders"].as_u64()
                .zip(after["pageRenders"].as_u64())
                .map(|(typing, before)| typing.saturating_sub(before));
            // GPUI marks an input's ancestors dirty. One parent render per
            // edit is normal; an initially empty composer can also change the
            // send button's enabled state. This is not a zero-render promise.
            let allowed_typing_parent_renders = PERFORMANCE_PULSES
                + u64::from(after["composerCharacters"] == 0);
            let checks = json!({
                "allPulsesSent": self.sent == PERFORMANCE_PULSES,
                "allTypingEditsSent": self.typing_sent == PERFORMANCE_PULSES,
                "textWasMeasured": warm["messageLayoutMisses"].as_u64().is_some_and(|v| v > 0),
                "layoutMissesStable": complete && warm["messageLayoutMisses"] == after["messageLayoutMisses"]
                    && after["messageLayoutMisses"] == after_typing["messageLayoutMisses"]
                    && after_typing["messageLayoutMisses"] == idle["messageLayoutMisses"],
                // ListState can reuse an already measured row without calling
                // message_view at all. Cache hits are diagnostic, not required.
                "warmTextLayoutCached": warm["cachedTextLayouts"].as_u64().is_some_and(|v| v > 0),
                "pageRenderedDuringPulses": after["pageRenders"].as_u64().zip(warm["pageRenders"].as_u64())
                    .is_some_and(|(after, warm)| after > warm),
                "typingParentRendersBounded": typing_parent_renders.is_some_and(|renders| {
                    renders <= allowed_typing_parent_renders
                }),
                "composerCharactersIncreased": after_typing["composerCharacters"].as_u64()
                    .zip(after["composerCharacters"].as_u64())
                    .is_some_and(|(typing, before)| typing == before + PERFORMANCE_PULSES),
                "composerRevisionIncreased": after_typing["composerRevision"].as_u64()
                    .zip(after["composerRevision"].as_u64())
                    .is_some_and(|(typing, before)| typing >= before + PERFORMANCE_PULSES),
                "idlePageRendersStable": complete && after_typing["pageRenders"] == idle["pageRenders"],
                "noChatReads": complete && warm["chatReadStarts"] == 0
                    && after["chatReadStarts"] == 0 && after_typing["chatReadStarts"] == 0
                    && idle["chatReadStarts"] == 0,
                "noImageRowInvalidations": complete && warm["imageRowInvalidations"] == after["imageRowInvalidations"]
                    && after["imageRowInvalidations"] == after_typing["imageRowInvalidations"]
                    && after_typing["imageRowInvalidations"] == idle["imageRowInvalidations"],
            });
            let verified = enabled && complete && checks.as_object().is_some_and(|checks| {
                checks.values().all(|check| check == true)
            });
            json!({
                "enabled": enabled, "complete": complete, "verified": verified,
                "warmupMs": PERFORMANCE_WARMUP_MS, "pulseIntervalMs": PERFORMANCE_PULSE_MS,
                "requestedPulses": PERFORMANCE_PULSES, "sentPulses": self.sent,
                "requestedTypingEdits": PERFORMANCE_PULSES, "sentTypingEdits": self.typing_sent,
                "typingParentRenders": typing_parent_renders,
                "allowedTypingParentRenders": allowed_typing_parent_renders,
                "inputDispatchTotalMs": self.input_dispatch_total.as_secs_f64() * 1_000.,
                "inputDispatchMaxMs": self.input_dispatch_max.as_secs_f64() * 1_000.,
                "inputDispatchMeasurement": "Synchronous edit dispatch only; excludes event delivery, layout and painting",
                "systemClipboardRead": false, "systemClipboardWritten": false,
                "idleObservationMs": PERFORMANCE_IDLE_MS,
                "warm": self.warm, "afterPulses": self.after, "afterTyping": self.after_typing,
                "idle": self.idle,
                "current": current, "checks": checks,
            })
        }
    }

    struct KeyboardCase {
        name: &'static str,
        modifiers: Modifiers,
        held: bool,
        composing: bool,
        read_only: bool,
        submits: u64,
        newline: bool,
    }

    fn keyboard_cases() -> Vec<KeyboardCase> {
        let mut cases = Vec::new();
        for (name, modifiers, held, composing, read_only, submits, newline) in [
            ("enter", Modifiers::default(), false, false, false, 1, false),
            ("shift-enter", Modifiers { shift: true, ..Default::default() }, false, false, false, 0, true),
            ("held-enter", Modifiers::default(), true, false, false, 0, false),
            ("marked-ime-enter", Modifiers::default(), false, true, false, 0, false),
            ("read-only-enter", Modifiers::default(), false, false, true, 0, false),
            ("control-enter", Modifiers { control: true, ..Default::default() }, false, false, false, 1, false),
            ("alt-enter", Modifiers { alt: true, ..Default::default() }, false, false, false, 0, false),
            ("platform-enter", Modifiers { platform: true, ..Default::default() }, false, false, false, 0, false),
            ("function-enter", Modifiers { function: true, ..Default::default() }, false, false, false, 0, false),
            ("control-shift-enter", Modifiers { control: true, shift: true, ..Default::default() }, false, false, false, 0, false),
            ("alt-shift-enter", Modifiers { alt: true, shift: true, ..Default::default() }, false, false, false, 0, false),
        ] {
            cases.push(KeyboardCase { name, modifiers, held, composing, read_only, submits, newline });
        }
        cases
    }

    #[derive(Default)]
    struct KeyboardContract {
        step: usize,
        before: Option<Value>,
        after_down: Option<Value>,
        cases: Vec<Value>,
        complete: bool,
        error: Option<String>,
    }

    /// Wheel input through the rendered tree; records list geometry before and after.
    #[derive(Default)]
    struct ScrollProbe {
        step: usize,
        before: Option<Value>,
        after_move: Option<Value>,
        after: Option<Value>,
    }

    impl ScrollProbe {
        fn report(&self, enabled: bool) -> Value {
            let moved = self.before.as_ref().zip(self.after.as_ref())
                .is_some_and(|(before, after)| before["scrollPx"] != after["scrollPx"]);
            json!({"enabled": enabled, "complete": self.after.is_some(),
                "dispatchPath": "Window::dispatch_input MouseMove + one notched ScrollWheel (3 lines) at the list viewport centre",
                "systemInputInjected": false, "scrolled": moved,
                "before": self.before, "afterMove": self.after_move, "after": self.after})
        }
    }

    impl KeyboardContract {
        fn report(&self, enabled: bool) -> Value {
            let verified = enabled && self.complete && self.error.is_none()
                && self.cases.len() == keyboard_cases().len() + 2
                && self.cases.iter().all(|case| case["verified"] == true);
            json!({
                "enabled": enabled, "complete": self.complete, "verified": verified,
                "dispatchPath": "Window::dispatch_input through the rendered production GPUI event tree",
                "systemInputInjected": false, "nativeMessagesSent": false,
                "systemClipboardRead": false, "systemClipboardWritten": false,
                "realImeVerified": false, "realSendVerified": false,
                "expectedCases": keyboard_cases().len() + 2, "cases": self.cases,
                "error": self.error,
            })
        }
    }

    struct FixtureRoot {
        page: Entity<LanManagementPage>,
        devices_bottom_issued: bool,
        host: Arc<HostState>,
        options: Options,
        data: PathBuf,
        run: PathBuf,
        background: String,
        font_family: String,
        renders: u64,
        metadata: Value,
        foreground_before: usize,
        ever_foreground: bool,
        performance: PerformancePulse,
        keyboard: KeyboardContract,
        scroll: ScrollProbe,
    }

    impl FixtureRoot {
        fn scroll_tick(&mut self, elapsed: Duration, window: &mut Window, cx: &mut Context<Self>) {
            if !self.options.scroll_probe || elapsed < Duration::from_millis(KEYBOARD_WARMUP_MS) || self.scroll.after.is_some() {
                return;
            }
            let snapshot = self.page.update(cx, |page, _| page.fixture_scroll_snapshot());
            let viewport = &snapshot["viewport"];
            let at = |index: usize| viewport[index].as_f64().unwrap_or_default() as f32;
            let centre = gpui::point(px(at(0) + at(2) / 2.), px(at(1) + at(3) / 2.));
            match self.scroll.step {
                0 => {
                    self.scroll.before = Some(snapshot);
                    window.dispatch_input(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                        position: centre, pressed_button: None, modifiers: Modifiers::default(),
                    }), cx);
                }
                1 => {
                    self.scroll.after_move = Some(snapshot);
                    window.dispatch_input(PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                        position: centre, delta: gpui::ScrollDelta::Lines(gpui::point(0., 3.)),
                        modifiers: Modifiers::default(), touch_phase: gpui::TouchPhase::Moved,
                    }), cx);
                }
                // Let the smooth-wheel glide settle before reading the result.
                2..=4 => {}
                _ => self.scroll.after = Some(snapshot),
            }
            self.scroll.step += 1;
        }

        fn keyboard_tick(&mut self, elapsed: Duration, window: &mut Window, cx: &mut Context<Self>) {
            if !self.options.keyboard_contract
                || elapsed < Duration::from_millis(KEYBOARD_WARMUP_MS)
                || self.keyboard.complete
            {
                return;
            }
            let cases = keyboard_cases();
            let composer_steps = cases.len() * 4;
            if self.keyboard.step < composer_steps {
                let case = &cases[self.keyboard.step / 4];
                let keystroke = Keystroke { key: "enter".into(), modifiers: case.modifiers, ..Default::default() };
                match self.keyboard.step % 4 {
                    0 => self.page.update(cx, |page, cx| {
                        page.fixture_keyboard_prepare("离线键盘契约", case.read_only, case.composing, window, cx);
                    }),
                    1 => {
                        self.keyboard.before = Some(self.page.update(cx, |page, cx| page.fixture_keyboard_snapshot(cx)));
                        window.dispatch_input(PlatformInput::KeyDown(KeyDownEvent { keystroke, is_held: case.held }), cx);
                    }
                    2 => {
                        self.keyboard.after_down = Some(self.page.update(cx, |page, cx| page.fixture_keyboard_snapshot(cx)));
                        window.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }), cx);
                    }
                    _ => {
                        let after = self.page.update(cx, |page, cx| page.fixture_keyboard_snapshot(cx));
                        let before = self.keyboard.before.take().unwrap_or(Value::Null);
                        let down = self.keyboard.after_down.take().unwrap_or(Value::Null);
                        let expected_text = format!("{}{}", before["text"].as_str().unwrap_or_default(), if case.newline { "\n" } else { "" });
                        let checks = json!({
                            "syntheticTextPrepared": before["text"].as_str().is_some_and(|text| !text.is_empty()),
                            "submitCounterReset": before["submitCount"] == 0,
                            "markedRangePrepared": before["isComposing"] == case.composing,
                            "readOnlyPrepared": before["readOnly"] == case.read_only,
                            "keyDownSubmitCount": down["submitCount"] == case.submits,
                            "keyUpDidNotSubmitAgain": after["submitCount"] == case.submits,
                            "keyDownText": down["text"] == expected_text,
                            "keyUpText": after["text"] == expected_text,
                        });
                        let verified = checks.as_object().is_some_and(|checks| checks.values().all(|check| check == true));
                        self.keyboard.cases.push(json!({"id":case.name,"verified":verified,
                            "before":before,"afterKeyDown":down,"afterKeyUp":after,"checks":checks}));
                    }
                }
            } else {
                let button_step = self.keyboard.step - composer_steps;
                match button_step {
                    0 => self.page.update(cx, |page, cx| {
                        page.fixture_keyboard_prepare("离线按钮契约", false, false, window, cx);
                        page.fixture_keyboard_open_devices(window, cx);
                    }),
                    1 => {
                        if !self.page.update(cx, |page, cx| page.fixture_focus_control("overlay:advanced", window, cx)) {
                            self.keyboard.error = Some("Rendered advanced settings button was not found".into());
                            self.keyboard.complete = true;
                        }
                    }
                    2 | 5 => {
                        self.keyboard.before = Some(self.page.update(cx, |page, cx| page.fixture_keyboard_snapshot(cx)));
                        window.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
                            keystroke: Keystroke { key: if button_step == 2 { "enter" } else { "space" }.into(), ..Default::default() },
                            is_held: false,
                        }), cx);
                    }
                    3 | 6 => {
                        self.keyboard.after_down = Some(self.page.update(cx, |page, cx| page.fixture_keyboard_snapshot(cx)));
                        window.dispatch_input(PlatformInput::KeyUp(KeyUpEvent {
                            keystroke: Keystroke { key: if button_step == 3 { "enter" } else { "space" }.into(), ..Default::default() },
                        }), cx);
                    }
                    4 | 7 => {
                        let after = self.page.update(cx, |page, cx| page.fixture_keyboard_snapshot(cx));
                        let before = self.keyboard.before.take().unwrap_or(Value::Null);
                        let down = self.keyboard.after_down.take().unwrap_or(Value::Null);
                        let checks = json!({
                            "stateWasBoolean": before["advanced"].is_boolean(),
                            "noKeyDownActivation": down["advanced"] == before["advanced"],
                            "oneKeyUpToggle": after["advanced"].as_bool().zip(before["advanced"].as_bool()).is_some_and(|(after,before)| after != before),
                            "noComposerSubmit": before["submitCount"] == 0 && down["submitCount"] == 0 && after["submitCount"] == 0,
                        });
                        let verified = checks.as_object().is_some_and(|checks| checks.values().all(|check| check == true));
                        self.keyboard.cases.push(json!({"id":if button_step == 4 {"button-enter"} else {"button-space"},
                            "verified":verified,"before":before,"afterKeyDown":down,"afterKeyUp":after,"checks":checks}));
                        if button_step == 7 {
                            self.keyboard.complete = true;
                        }
                    }
                    _ => unreachable!(),
                }
            }
            self.keyboard.step += 1;
        }

        fn performance_tick(&mut self, elapsed: Duration, window: &mut Window, cx: &mut Context<Self>) {
            if !self.options.performance_pulse
                || elapsed < Duration::from_millis(PERFORMANCE_WARMUP_MS)
                || self.performance.idle.is_some()
            {
                return;
            }
            let snapshot = self.page.read(cx).fixture_performance_snapshot();
            if self.performance.warm.is_none() {
                self.performance.warm = Some(snapshot.clone());
            }
            if self.performance.sent < PERFORMANCE_PULSES {
                self.page.update(cx, |_, cx| cx.notify());
                self.performance.sent += 1;
                self.performance.last_pulse = Some(Instant::now());
            } else if self.performance.after.is_none() {
                // Allow the final notification to paint before freezing the
                // counters; the report timer itself never notifies the page.
                if self.performance.last_pulse.is_some_and(|last| {
                    last.elapsed() >= Duration::from_millis(250)
                }) {
                    self.performance.after = Some(snapshot);
                }
            } else if self.performance.typing_sent < PERFORMANCE_PULSES {
                // Drive the same text-edit path as normal native input, using
                // synthetic characters and preserving the fixed attachments.
                let dispatched = Instant::now();
                self.page.update(cx, |page, cx| {
                    page.fixture_append_composer("测", window, cx);
                });
                let elapsed = dispatched.elapsed();
                self.performance.input_dispatch_total += elapsed;
                self.performance.input_dispatch_max = self.performance.input_dispatch_max.max(elapsed);
                self.performance.typing_sent += 1;
                self.performance.last_typing = Some(Instant::now());
            } else if self.performance.after_typing.is_none() {
                if self.performance.last_typing.is_some_and(|last| {
                    last.elapsed() >= Duration::from_millis(250)
                }) {
                    self.performance.after_typing = Some(snapshot);
                    self.performance.after_typing_at = Some(Instant::now());
                }
            } else if self.performance.after_typing_at.is_some_and(|after| {
                after.elapsed() >= Duration::from_millis(PERFORMANCE_IDLE_MS)
            }) {
                self.performance.idle = Some(snapshot);
            }
        }

        fn report(&mut self, window: &Window, cx: &mut Context<Self>) -> Value {
            // Initial hidden-window layout has no scroll bounds. The report
            // timer runs after presentation; request the bottom once ready.
            if self.options.devices_bottom && !self.devices_bottom_issued && self.renders > 0 {
                self.devices_bottom_issued = self.page.update(cx, |page, cx| page.fixture_devices_bottom(cx));
            }
            let hwnd = platform::hwnd(window)
                .map(|hwnd| hwnd.0 as usize)
                .unwrap_or_default();
            let foreground = unsafe { GetForegroundWindow().0 as usize };
            self.ever_foreground |= hwnd != 0 && hwnd == foreground;
            let actual = self.host.lan_status();
            let actual_sync = self.host.device_sync_status();
            let composer_layout = self.page.read(cx).fixture_composer_layout();
            let performance = self.performance.report(
                self.options.performance_pulse,
                self.page.read(cx).fixture_performance_snapshot(),
            );
            let isolated = cliphost_core::is_headless()
                && actual["enabled"] == false
                && actual["running"] == false
                // A default-on master with no paired peers has no external
                // effects. Isolation is about services/capture and peers,
                // rather than the user's configured switch value.
                && actual_sync["peers"].as_array().is_some_and(|peers| peers.is_empty())
                && self.host.lan_listener_port().is_none();
            let mut report = json!({"mode":"lan-offline-fixture","renderer":"LanManagementPage",
                "pid":std::process::id(),"hwnd":hwnd,"theme":self.options.theme,
                "scenario":self.options.scenario,"devices":self.options.devices,
                "iconOverride":self.options.icon_override,"conversation":self.options.conversation,
                "content":self.options.content,"width":f32::from(window.viewport_size().width),
                "height":f32::from(window.viewport_size().height),"scaleFactor":window.scale_factor(),
                "viewRenderCount":self.renders,"noFocus":true,"isolated":isolated,
                "productionHostStarted":false,"coreStateOpened":true,"dataRoot":self.data,"runRoot":self.run,
                "headless":cliphost_core::is_headless(),"captureStarted":false,"hooksStarted":false,
                "apiStarted":false,"rpcStarted":false,"lanStarted":false,
                "actualCoreLanStatus":actual,"fixture":self.metadata,"composerLayout":composer_layout,
                "performance":performance,
                "keyboardContract":self.keyboard.report(self.options.keyboard_contract),
                "scrollProbe":self.scroll.report(self.options.scroll_probe),
                "foregroundBefore":self.foreground_before,"foregroundAfter":foreground,
                "foregroundPreserved":self.foreground_before == foreground,"ownWindowWasForeground":self.ever_foreground,
                "actionsBlocked":true,"realSendVerified":false,"imeVerified":false,
                "visualAlignmentVerified":false,"error":if isolated {Value::Null} else {json!("Core isolation invariant failed")}});
            report["companion"] = json!(self.options.companion);
            report["devicesBottom"] = json!(self.options.devices_bottom);
            report["actualCoreDeviceSyncStatus"] = actual_sync;
            report["clipboardSyncControls"] = self.page.read(cx).fixture_device_sync_snapshot();
            report["devicesBottomIssued"] = json!(self.devices_bottom_issued);
            report["devicesBottomApplied"] = json!(self.devices_bottom_issued
                && report["clipboardSyncControls"]["deviceScrollAtBottom"] == true);
            report["fontFamily"] = json!(self.font_family);
            report["companionVerification"] = json!(
                "Production page layout only; no clipboard owner or companion lifecycle is started"
            );
            if self.options.content == "short-text" {
                report["messageTextLayouts"] = self.page.read(cx).fixture_message_layouts(cx);
            }
            report
        }
    }

    impl Render for FixtureRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.renders += 1;
            div()
                .size_full()
                .font_family(self.font_family.clone())
                .bg(color(&self.background))
                .child(self.page.clone())
        }
    }

    fn write_report(file: &mut File, report: &Value) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(report)?;
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(&bytes)?;
        file.flush()?;
        Ok(())
    }

    pub fn run(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
        let Some(options) = Options::parse(arguments)? else {
            return Ok(());
        };
        let locations = locations(&options.output)?;
        cliphost_core::force_headless(true);
        ensure!(
            cliphost_core::is_headless(),
            "Headless core gate unavailable"
        );
        let host = Arc::new(
            HostState::open(
                AppPaths::from_data_root(locations.data.clone()),
                UNBOUND_API_PORT,
            )
            .map_err(|error| anyhow!(error))?,
        );
        let actual = host.lan_status();
        ensure!(
            actual["enabled"] == false
                && actual["running"] == false
                && host.lan_listener_port().is_none(),
            "Fresh fixture core unexpectedly enabled LAN"
        );
        i18n::set_language(LanguageMode::Chinese);
        let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        let data = fixture(&options, &locations);
        let metadata = json!({"selected":data.selected,"peerCount":data.peers.len(),
            "pairedPeerCount":data.peers.iter().filter(|peer|peer["paired"] == true).count(),
            "conversationCount":data.conversations.len(),"messageCount":data.messages.len(),
            "displayedStatus":data.status,"fixtureImage":locations.image});
        let mut metadata = metadata;
        if options.content == "short-text" {
            metadata["expectedTextMessages"] = json!(SHORT_TEXT_CASES
                .iter()
                .map(|(id, text)| json!({"id":id,"text":text,"singleLine":!text.contains('\n')}))
                .collect::<Vec<_>>());
        }
        let background = tokens.theme(options.theme == "dark").background.clone();
        let font_family = tokens.typography.font_family.clone();
        let started = Instant::now();
        let foreground_before = unsafe { GetForegroundWindow().0 as usize };
        let mut report_file = locations.report;
        let exit_after_ms = options.exit_after_ms;
        let performance_pulse = options.performance_pulse;
        let keyboard_contract = options.keyboard_contract;
        let scroll_probe = options.scroll_probe;
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let bounds = Bounds::centered(None, size(px(options.width), px(options.height)), cx);
            let handle = cx
                .open_window(
                    WindowOptions {
                        titlebar: None,
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        focus: false,
                        show: false,
                        window_min_size: Some(size(px(320.), px(360.))),
                        ..Default::default()
                    },
                    move |window, cx| {
                        window.set_window_title("ClipHost · LAN offline fixture");
                        cx.new(|cx| {
                            let page = cx.new(|cx| {
                                LanManagementPage::new_fixture(
                                    host.clone(),
                                    tokens,
                                    options.theme == "dark",
                                    data,
                                    window,
                                    cx,
                                )
                            });
                            page.update(cx, |page, cx| page.set_companion(options.companion, cx));
                            if options.devices_bottom {
                                page.update(cx, |page, cx| page.fixture_devices_bottom(cx));
                            }
                            if options.scenario == "ready" && options.content == "media" {
                                page.update(cx, |page, cx| page.set_fixture_draft(json!({
                                    "text":"这张图片和刚才复制的内容一起发给你。",
                                    "filePaths":[locations.image],
                                    "recentItems":[{"id":"fixture-recent","kind":"text",
                                        "label":"会议记录：周五确认最终设计稿。",
                                        "text":"会议记录：周五确认最终设计稿。","filePaths":[]}]
                                }), window, cx));
                            }
                            FixtureRoot {
                                page,
                                devices_bottom_issued: false,
                                host,
                                options,
                                data: locations.data,
                                run: locations.run,
                                background,
                                font_family,
                                renders: 0,
                                metadata,
                                foreground_before,
                                ever_foreground: false,
                                performance: PerformancePulse::default(),
                                keyboard: KeyboardContract::default(),
                                scroll: ScrollProbe::default(),
                            }
                        })
                    },
                )
                .expect("create isolated LAN fixture window");
            handle
                .update(cx, |view, window, _| {
                    platform::set_no_activate(window, true).expect("set fixture no-activate");
                    // Hidden GPUI creation defers its initial placement. The
                    // no-activate Win32 show path must size the client itself.
                    platform::set_geometry(window, None, Some((
                        f64::from(view.options.width), f64::from(view.options.height)
                    ))).expect("set fixture client DIP dimensions");
                    platform::show_no_activate(window).expect("show fixture without activation");
                })
                .expect("configure fixture HWND");
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            if performance_pulse {
                cx.spawn(async move |cx| loop {
                    Timer::after(Duration::from_millis(PERFORMANCE_PULSE_MS)).await;
                    if handle.update(cx, |view, window, cx| {
                        view.performance_tick(started.elapsed(), window, cx);
                    }).is_err() {
                        break;
                    }
                })
                .detach();
            }
            if scroll_probe {
                cx.spawn(async move |cx| loop {
                    Timer::after(Duration::from_millis(300)).await;
                    if handle.update(cx, |view, window, cx| view.scroll_tick(started.elapsed(), window, cx)).is_err() {
                        break;
                    }
                })
                .detach();
            }
            if keyboard_contract {
                cx.spawn(async move |cx| loop {
                    Timer::after(Duration::from_millis(KEYBOARD_STEP_MS)).await;
                    if handle.update(cx, |view, window, cx| {
                        view.keyboard_tick(started.elapsed(), window, cx);
                    }).is_err() {
                        break;
                    }
                })
                .detach();
            }
            cx.spawn(async move |cx| loop {
                Timer::after(Duration::from_millis(250)).await;
                let Ok(mut report) = handle.update(cx, |view, window, cx| view.report(window, cx))
                else {
                    break;
                };
                report["elapsedMs"] = json!(started.elapsed().as_millis() as u64);
                if let Err(error) = write_report(&mut report_file, &report) {
                    eprintln!("LAN fixture report failed: {error}");
                    let _ = cx.update(|cx| cx.quit());
                    break;
                }
                if report["isolated"] != true
                    || started.elapsed() >= Duration::from_millis(exit_after_ms)
                {
                    let _ = cx.update(|cx| cx.quit());
                    break;
                }
            })
            .detach();
        });
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn options(extra: &[&str]) -> anyhow::Result<Options> {
            let mut arguments = vec![
                "--output".to_string(),
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("fixture-options-only")
                    .display()
                    .to_string(),
            ];
            arguments.extend(extra.iter().map(|argument| (*argument).to_string()));
            Options::parse(arguments)?.ok_or_else(|| anyhow!("Unexpected help result"))
        }

        #[test]
        fn standalone_preview_defaults_remain_unchanged() {
            let options = options(&[]).unwrap();
            assert!(!options.companion);
            assert!(!options.devices);
            assert_eq!((options.width, options.height), (900., 640.));
            assert_eq!(options.theme, "dark");
        }

        #[test]
        fn compact_companion_preview_accepts_devices_at_its_minimum_width() {
            let options = options(&[
                "--companion", "--devices", "--width", "320", "--height", "420", "--theme", "light",
            ])
            .unwrap();
            assert!(options.companion && options.devices);
            assert_eq!((options.width, options.height), (320., 420.));
            assert_eq!(options.theme, "light");
        }

        #[test]
        fn compact_companion_preview_still_rejects_widths_below_the_minimum() {
            assert!(options(&["--companion", "--width", "319"]).is_err());
        }

        #[test]
        fn short_text_preview_is_explicit_and_preserves_successful_message_bodies() {
            let options = options(&["--companion", "--content", "short-text"]).unwrap();
            assert_eq!(options.content, "short-text");
            let messages = short_text_messages("dm:fixture-work");
            assert_eq!(messages.len(), 5);
            for (message, (id, text)) in messages.iter().zip(SHORT_TEXT_CASES) {
                assert_eq!(message["id"], id);
                assert_eq!(message["text"], text);
                assert_eq!(message["status"], "delivered");
                assert_eq!(message["direction"], "outgoing");
                assert!(message["attachments"].as_array().unwrap().is_empty());
            }
            assert_eq!(messages[0]["text"], "测试123");
            assert_eq!(messages[1]["text"], "测");
            assert!(messages[4]["text"].as_str().unwrap().contains('\n'));
        }
    }
}

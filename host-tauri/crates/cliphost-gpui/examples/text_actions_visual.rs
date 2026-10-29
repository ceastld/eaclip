//! Production text-action launcher/manager with disposable synthetic rules.
//! No NativeHost, API, RPC, clipboard capture, hooks or system input.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("text_actions_visual requires a Windows Debug build.");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() {
    if let Err(error) = fixture::run() {
        eprintln!("text_actions_visual: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(all(windows, debug_assertions))]
mod fixture {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use anyhow::{anyhow, bail, ensure, Context as _, Result};
    use cliphost_core::{
        extra_settings::GlobalAppConfig,
        i18n::{set_language, LanguageMode},
        AppPaths, HostState,
    };
    use cliphost_gpui::{
        platform,
        preview::{DesignTokens, TOKENS_JSON},
        settings::SettingsWindow,
        text_action_launcher::TextActionLauncher,
        DesignAssets,
    };
    use gpui::{prelude::*, px, size, Application, Bounds, Timer, WindowBounds, WindowOptions};
    use serde_json::{json, Value};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
    };

    struct Options {
        mode: String,
        dark: bool,
        width: f32,
        height: f32,
        output: Option<PathBuf>,
    }

    fn options() -> Result<Option<Options>> {
        let mut mode = "use".to_string();
        let mut dark = true;
        let (mut width, mut height, mut output) = (None, None, None);
        let mut arguments = std::env::args().skip(1);
        while let Some(key) = arguments.next() {
            if matches!(key.as_str(), "--help" | "-h") {
                println!("text_actions_visual [--mode use|manage] [--theme dark|light] [--width 560] [--height 500] [--output FRESH_REPOSITORY_TMP_DIRECTORY]");
                println!("Default output: .tmp/text-actions-native-20261021/<fresh case>. Own-window PrintWindow PNG only; no activation or external input.");
                return Ok(None);
            }
            let value = arguments
                .next()
                .with_context(|| format!("{key} requires a value"))?;
            match key.as_str() {
                "--mode" => {
                    ensure!(["use", "manage"].contains(&value.as_str()), "Invalid mode");
                    mode = value;
                }
                "--theme" => {
                    ensure!(["dark", "light"].contains(&value.as_str()), "Invalid theme");
                    dark = value == "dark";
                }
                "--width" => width = Some(value.parse::<f32>()?),
                "--height" => height = Some(value.parse::<f32>()?),
                "--output" => output = Some(PathBuf::from(value)),
                _ => bail!("Unknown argument: {key}"),
            }
        }
        let width = width.unwrap_or(if mode == "use" { 560.0 } else { 940.0 });
        let height = height.unwrap_or(if mode == "use" { 500.0 } else { 660.0 });
        ensure!(
            width.is_finite() && (440.0..=1400.0).contains(&width),
            "Width must be 440–1400 DIP"
        );
        ensure!(
            height.is_finite() && (400.0..=1200.0).contains(&height),
            "Height must be 400–1200 DIP"
        );
        ensure!(
            mode != "manage" || width >= 560.0,
            "Manager minimum fixture width is 560 DIP"
        );
        Ok(Some(Options {
            mode,
            dark,
            width,
            height,
            output,
        }))
    }

    fn output_directory(options: &Options) -> Result<PathBuf> {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()?;
        let scratch = repository.join(".tmp");
        fs::create_dir_all(&scratch)?;
        let scratch = scratch.canonicalize()?;
        ensure!(
            scratch.parent() == Some(repository.as_path()),
            "Repository .tmp escaped repository"
        );
        let output = if let Some(output) = &options.output {
            if output.is_absolute() {
                output.clone()
            } else {
                repository.join(output)
            }
        } else {
            let base = scratch.join("text-actions-native-20261021");
            fs::create_dir_all(&base)?;
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            base.join(format!(
                "{}-{}-{}-{}-{nonce}",
                options.mode,
                if options.dark { "dark" } else { "light" },
                options.width as u32,
                std::process::id()
            ))
        };
        let parent = output
            .parent()
            .context("Missing output parent")?
            .canonicalize()?;
        ensure!(
            parent.starts_with(&scratch),
            "Output must stay under repository .tmp"
        );
        let run = parent.join(output.file_name().context("Missing output name")?);
        fs::create_dir(&run).context("Output must be a fresh nonexistent directory")?;
        let run = run.canonicalize()?;
        ensure!(run.starts_with(&scratch), "Output escaped repository .tmp");
        for name in ["data", "profile", "profile/Local", "profile/Roaming"] {
            fs::create_dir(run.join(name))?;
        }
        std::env::set_var("APPDATA", run.join("profile/Roaming"));
        std::env::set_var("LOCALAPPDATA", run.join("profile/Local"));
        std::env::set_var("BETTERCLIP_TAURI_DATA_ROOT", run.join("data"));
        Ok(run)
    }

    fn rules() -> Vec<Value> {
        let seeds = [
            ("去除空行", "清理", "Internal", "保留正文分行，去掉多余空行"),
            (
                "去除空白行",
                "清理",
                "Internal",
                "同时去掉只有空格或制表符的行",
            ),
            (
                "规范粘贴",
                "清理",
                "Internal",
                "整理从 PDF / 论文复制的断行与空白",
            ),
            (
                "去掉行首空格",
                "清理",
                "SingleReplace",
                "清理每行开头的多余空格",
            ),
            (
                "去掉行尾空格",
                "清理",
                "SingleReplace",
                "清理每行末尾的空白字符",
            ),
            (
                "合并连续空格",
                "清理",
                "SingleReplace",
                "将连续空格替换为一个空格",
            ),
            (
                "移除零宽字符",
                "清理",
                "SingleReplace",
                "清理复制内容中的不可见字符",
            ),
            (
                "去掉网页引用标记",
                "清理",
                "SingleReplace",
                "移除正文中的引用序号",
            ),
            (
                "中文逗号转英文",
                "替换",
                "SingleReplace",
                "将中文逗号替换为英文逗号",
            ),
            (
                "英文逗号转中文",
                "替换",
                "SingleReplace",
                "将英文逗号替换为中文逗号",
            ),
            (
                "制表符转空格",
                "替换",
                "SingleReplace",
                "将制表符替换为四个空格",
            ),
            (
                "换行转空格",
                "替换",
                "SingleReplace",
                "将多行文字合并为一段",
            ),
            (
                "空格转换行",
                "替换",
                "SingleReplace",
                "将空格分隔的内容逐项分行",
            ),
            (
                "替换部门名称",
                "替换",
                "SingleReplace",
                "将示例部门名称更新为新名称",
            ),
            (
                "替换文档域名",
                "替换",
                "SingleReplace",
                "将旧链接改为 docs.example.com",
            ),
            (
                "删除示例前缀",
                "替换",
                "SingleReplace",
                "去掉每项开头的固定文字",
            ),
            (
                "格式化 JSON",
                "开发",
                "Internal",
                "展开缩进，让 JSON 更容易阅读",
            ),
            ("压缩 JSON", "开发", "Internal", "移除 JSON 中的格式空白"),
            ("添加代码反引号", "开发", "Pair", "将文字包裹为行内代码"),
            ("添加代码块", "开发", "Pair", "将多行文本包裹为代码块"),
            ("添加双引号", "开发", "Pair", "将文本包裹为双引号字符串"),
            ("添加单引号", "开发", "Pair", "将文本包裹为单引号字符串"),
            (
                "生成环境变量名",
                "开发",
                "ZExpression",
                "通过表达式转换变量名称",
            ),
            (
                "打开网址处理动作",
                "开发",
                "QuickerAction",
                "启动已有动作，后续由该动作自行处理",
            ),
            ("加粗文字", "格式", "Pair", "给文本加上 Markdown 加粗标记"),
            ("斜体文字", "格式", "Pair", "给文本加上 Markdown 斜体标记"),
            ("删除线", "格式", "Pair", "给文本加上 Markdown 删除线标记"),
            ("添加引用", "格式", "Pair", "给文本加上引用前缀"),
            ("添加无序列表", "格式", "Pair", "给每项加上列表前缀"),
            ("添加任务框", "格式", "Pair", "将内容变为待办事项"),
            ("添加中文括号", "格式", "Pair", "用中文全角括号包裹内容"),
            ("添加书名号", "格式", "Pair", "用书名号包裹选中文字"),
            ("会议记录框架", "模板", "Send", "发送固定的会议记录结构"),
            ("问题反馈框架", "模板", "Send", "发送现象与复现步骤模板"),
            ("每日工作记录", "模板", "Send", "发送今日计划与完成事项结构"),
            ("邮件落款", "模板", "Send", "发送固定的邮件结束语"),
            (
                "打开翻译动作",
                "工作流",
                "QuickerAction",
                "启动已有翻译动作，不返回处理文本",
            ),
            (
                "整理为标题",
                "工作流",
                "ZExpression",
                "用已有表达式整理标题文字",
            ),
            (
                "提取链接",
                "工作流",
                "ZExpression",
                "通过表达式提取文本中的链接",
            ),
            (
                "打开笔记动作",
                "工作流",
                "QuickerAction",
                "启动已有笔记动作，由动作自身处理",
            ),
        ];
        seeds.into_iter().enumerate().map(|(index, (title, group, kind, description))| {
            let data = match kind {
                "Internal" => json!({"ObjType":1,"Type":match index {1=>"RemoveWhiteSpaceLine",2=>"StandardText",16=>"FormatJson",17=>"ZipJson",_=>"RemoveEmptyLine"}}),
                "SingleReplace" => json!({"ObjType":2,"Pattern":if index==8 {"，"} else {" +"},"ReplaceMent":if index==8 {","} else {" "},"UseRegex":index!=8,"Options":2,"EscapePattern":false,"EscapeReplaceMent":true}),
                "Pair" => json!({"ObjType":6,"Prefix":if index==18 {"`"} else {"**"},"Suffix":if index==18 {"`"} else {"**"},"MultiLines":false,"Trim":false}),
                "Send" => json!({"ObjType":7,"Text":"议题：\n结论：\n待办："}),
                "ZExpression" => json!({"ObjType":4,"Expresstion":"$={text}.Trim()"}),
                _ => json!({"ObjType":5,"Id":"fixture-action-not-executed","WaitResult":false}),
            };
            json!({"Title":title,"Description":description,"Group":group,"Type":kind,
                "Data":data,"Common":([0,5,8,16,18,24].contains(&index)),"Simulate":false,"SimulateDelay":0})
        }).collect()
    }

    fn native_report(window: &gpui::Window) -> Result<Value> {
        let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
        let foreground = unsafe { GetForegroundWindow().0 as usize };
        let no_activate =
            unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } & WS_EX_NOACTIVATE.0 as isize != 0;
        ensure!(
            no_activate && foreground != hwnd.0 as usize,
            "Fixture activated or lost no-activate"
        );
        Ok(
            json!({"hwnd":hwnd.0 as usize,"foreground":foreground,"noActivate":no_activate,
            "viewportDip":[f32::from(window.viewport_size().width),f32::from(window.viewport_size().height)],
            "scaleFactor":window.scale_factor()}),
        )
    }

    fn capture(hwnd_value: isize, path: &Path) -> Result<Value> {
        use windows::Win32::{
            Foundation::{HWND, RECT},
            Graphics::Gdi::*,
            UI::WindowsAndMessaging::{GetClientRect, GetWindowThreadProcessId},
        };
        #[link(name = "user32")]
        extern "system" {
            #[link_name = "PrintWindow"]
            fn print_window(hwnd: HWND, dc: HDC, flags: u32) -> i32;
        }
        let hwnd = HWND(hwnd_value as *mut std::ffi::c_void);
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            ensure!(
                pid == std::process::id(),
                "Capture only this fixture's HWND"
            );
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect)?;
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            ensure!(
                (1..=4096).contains(&width) && (1..=4096).contains(&height),
                "Invalid capture geometry"
            );
            let source = GetDC(Some(hwnd));
            ensure!(!source.is_invalid(), "GetDC failed");
            let dc = CreateCompatibleDC(Some(source));
            let bitmap = CreateCompatibleBitmap(source, width, height);
            if dc.is_invalid() || bitmap.is_invalid() {
                if !dc.is_invalid() {
                    let _ = DeleteDC(dc);
                }
                if !bitmap.is_invalid() {
                    let _ = DeleteObject(bitmap.into());
                }
                ReleaseDC(Some(hwnd), source);
                bail!("Cannot allocate capture bitmap");
            }
            let previous = SelectObject(dc, bitmap.into());
            let printed = print_window(hwnd, dc, 3) != 0;
            SelectObject(dc, previous);
            let mut pixels = vec![0; width as usize * height as usize * 4];
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let rows = if printed {
                GetDIBits(
                    dc,
                    bitmap,
                    0,
                    height as u32,
                    Some(pixels.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS,
                )
            } else {
                0
            };
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(Some(hwnd), source);
            ensure!(printed && rows == height, "PrintWindow/GetDIBits failed");
            let first = [pixels[0], pixels[1], pixels[2]];
            let nonuniform = pixels.chunks_exact(4).any(|pixel| pixel[..3] != first);
            ensure!(nonuniform, "Capture contained no rendered detail");
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            image::save_buffer(
                path,
                &pixels,
                width as u32,
                height as u32,
                image::ColorType::Rgba8,
            )?;
            Ok(
                json!({"method":"PrintWindow(PW_CLIENTONLY|PW_RENDERFULLCONTENT)","path":path,
                "width":width,"height":height,"nonuniform":nonuniform,"desktopRead":false}),
            )
        }
    }

    pub fn run() -> Result<()> {
        let Some(options) = options()? else {
            return Ok(());
        };
        let run = output_directory(&options)?;
        let paths = AppPaths::from_data_root(run.join("data"));
        fs::create_dir(&paths.configs)?;
        let configuration = GlobalAppConfig {
            ui_theme_mode: if options.dark { "dark" } else { "light" }.into(),
            ui_language: "zh-CN".into(),
            launch_at_startup: false,
            window_background_frosted_glass: false,
            ..GlobalAppConfig::default()
        };
        fs::write(
            paths.configs.join("GlobalConfig.json"),
            serde_json::to_vec_pretty(&configuration)?,
        )?;
        fs::write(
            paths.configs.join("TextActionData.json"),
            serde_json::to_vec_pretty(&json!({"Actions":rules()}))?,
        )?;
        cliphost_core::force_headless(true);
        set_language(LanguageMode::Chinese);
        let host = Arc::new(HostState::open(paths, 29065).map_err(|error| anyhow!(error))?);
        let actions = host.text_action_list();
        ensure!(
            actions["actions"]
                .as_array()
                .is_some_and(|actions| actions.len() == 40),
            "Synthetic rule loading failed"
        );
        let preview = host
            .text_action_preview(
                &json!({"action":actions["actions"][0],"input":"第一行\n\n第二行"}),
            )
            .map_err(|error| anyhow!(error))?;
        ensure!(
            preview["supported"] == true && preview["output"] == "第一行\n第二行",
            "Local sample runner failed"
        );
        let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        let report_path = run.join("report.json");
        let screenshot = run.join("text-actions.png");
        let before = unsafe { GetForegroundWindow().0 as usize };
        let report = Arc::new(Mutex::new(
            json!({"complete":false,"passed":false,"mode":options.mode,
            "theme":if options.dark {"dark"} else {"light"},"width":options.width,"height":options.height,
            "pid":std::process::id(),"dataRoot":run.join("data"),"syntheticRules":40,
            "localPreviewChecked":true,"nativeHostStarted":false,"apiStarted":false,"rpcStarted":false,
            "captureStarted":false,"hooksStarted":false,"inputInjected":false,"focusRequested":false}),
        ));
        fs::write(
            &report_path,
            serde_json::to_vec_pretty(&*report.lock().unwrap())?,
        )?;
        let final_report = report_path.clone();
        let completed = report.clone();
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let handle = if options.mode == "use" {
                let host = host.clone();
                let bounds =
                    Bounds::centered(None, size(px(options.width), px(options.height)), cx);
                let handle = cx
                    .open_window(
                        WindowOptions {
                            titlebar: None,
                            focus: false,
                            show: false,
                            is_resizable: false,
                            is_minimizable: false,
                            window_bounds: Some(WindowBounds::Windowed(bounds)),
                            window_background: gpui::WindowBackgroundAppearance::Opaque,
                            ..Default::default()
                        },
                        move |window, cx| {
                            cx.new(|cx| {
                                TextActionLauncher::new(host, tokens, options.dark, window, cx)
                            })
                        },
                    )
                    .expect("Open disposable launcher");
                gpui::AnyWindowHandle::from(handle)
            } else {
                let mut handle = None;
                SettingsWindow::show(
                    &mut handle,
                    host.clone(),
                    tokens,
                    options.dark,
                    Some("textAction".into()),
                    cx,
                )
                .expect("Open disposable text-action manager");
                gpui::AnyWindowHandle::from(handle.expect("Manager handle"))
            };
            let setup = handle.update(cx, |_, window, _| -> Result<()> {
                platform::set_no_activate(window, true).map_err(|error| anyhow!(error))?;
                platform::set_geometry(
                    window,
                    None,
                    Some((options.width as f64, options.height as f64)),
                )
                .map_err(|error| anyhow!(error))?;
                platform::show_no_activate(window).map_err(|error| anyhow!(error))?;
                Ok(())
            });
            if !matches!(setup, Ok(Ok(()))) {
                let mut value = report.lock().unwrap();
                value["complete"] = json!(true);
                value["error"] = json!(format!("{setup:?}"));
                let _ = fs::write(&report_path, serde_json::to_vec_pretty(&*value).unwrap());
                cx.quit();
                return;
            }
            cx.spawn(async move |cx| {
                Timer::after(Duration::from_millis(1800)).await;
                let native = handle.update(cx, |_, window, _| native_report(window));
                // PrintWindow may enter the native dispatcher: release all GPUI
                // entity/window borrows before asking for this own HWND's pixels.
                let result = (|| -> Result<Value> {
                    let mut native = native??;
                    let hwnd = native["hwnd"].as_u64().context("Missing fixture HWND")?;
                    native["screenshot"] = capture(hwnd as isize, &screenshot)?;
                    native["foregroundBefore"] = json!(before);
                    native["foregroundPreserved"] =
                        json!(unsafe { GetForegroundWindow().0 as usize } == before);
                    ensure!(
                        cliphost_core::is_headless()
                            && host.lan_status()["enabled"] == false
                            && host.lan_listener_port().is_none(),
                        "Core isolation failed"
                    );
                    Ok(native)
                })();
                let mut value = report.lock().unwrap();
                value["complete"] = json!(true);
                match result {
                    Ok(detail) => {
                        value["passed"] = json!(true);
                        value["detail"] = detail;
                    }
                    Err(error) => value["error"] = json!(format!("{error:#}")),
                }
                let _ = fs::write(&report_path, serde_json::to_vec_pretty(&*value).unwrap());
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
        ensure!(
            completed.lock().unwrap()["passed"] == true,
            "Visual fixture failed; see {}",
            final_report.display()
        );
        println!("Text actions capture passed: {}", final_report.display());
        Ok(())
    }
}

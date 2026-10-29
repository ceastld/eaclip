//! Isolated visual check of the hover card with generated sample files.
//! Disposable headless core, own-HWND PNG, no input or native services.
use super::{CardLayout, HoverCard, Slot, CASCADE_GAP, FILES_WIDTH, MARGIN, MAX_HEIGHT, PREVIEW_WIDTH,
    RICH_WIDTH, TEXT_CARD_WIDTH, TEXT_PREVIEW_WIDTH};
use crate::{
    hover::{Content, FileCard, RichCard, RichPart},
    platform,
    preview::{DesignTokens, TOKENS_JSON},
    super_menu::style_fixture::{capture, output_directory},
    DesignAssets,
};
use anyhow::{anyhow, ensure, Context as _};
use cliphost_core::{AppPaths, HostState};
use gpui::{prelude::*, px, size, Application, Bounds, Timer, WindowBounds, WindowOptions};
use serde_json::json;
use std::{fs, path::{Path, PathBuf}, sync::{Arc, Mutex}, time::Duration};

const SCENARIOS: [&str; 4] = ["rich", "files", "files-text", "text"];

fn scene(path: &Path, top: [u8; 3], bottom: [u8; 3], width: u32, height: u32) -> anyhow::Result<()> {
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        let t = y as f32 / height as f32;
        let ridge = (height as f32 * (0.62 + 0.12 * ((x as f32 / width as f32) * 6.0).sin())) as u32;
        if y > ridge {
            image::Rgb([52, 84, 94])
        } else {
            image::Rgb(std::array::from_fn(|i| (top[i] as f32 * (1.0 - t) + bottom[i] as f32 * t) as u8))
        }
    });
    image.save(path)?;
    Ok(())
}

fn samples(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    fs::create_dir_all(dir)?;
    let sunset = dir.join("山间日落.png");
    let night = dir.join("夜景.png");
    scene(&sunset, [170, 200, 205], [234, 201, 173], 640, 400)?;
    scene(&night, [43, 58, 103], [142, 124, 195], 480, 360)?;
    let code = dir.join("hover_state.rs");
    fs::write(&code, "//! Warm row switches skip the cold delay.\nuse std::time::{Duration, Instant};\n\npub const WARM_GRACE: Duration = Duration::from_millis(400);\n\n#[derive(Clone, Copy)]\npub struct Ledger {\n    warm_until: Option<Instant>,\n}\n\nimpl Ledger {\n    /* Returns true while a preview is still visible. */\n    pub fn warm(&self, now: Instant) -> bool {\n        self.warm_until.is_some_and(|until| now <= until)\n    }\n}\n\nfn main() {\n    let label = \"hover\";\n    println!(\"{label}: {}\", 42);\n}\n")?;
    let notes = dir.join("发布说明.md");
    fs::write(&notes, "# 0.33 发布说明\n\n- 条目悬浮预览显示后切换不再等待\n- 文件悬浮支持 `文本` 与代码预览\n\n```rust\nlet ready = true;\n```\n\n> 只读取文件开头，不加载整个文件。\n")?;
    let doc = dir.join("评审纪要.docx");
    fs::write(&doc, [0x50, 0x4b, 0x03, 0x04, 0, 0, 0, 0])?;
    Ok(vec![sunset, night, code, notes, doc])
}

pub fn run(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let (mut output, mut dark, mut scenario, mut card_top) = (None, true, "files".to_owned(), MARGIN);
    let mut args = arguments.into_iter();
    while let Some(key) = args.next() {
        if matches!(key.as_str(), "--help" | "-h") {
            println!("hover_card_visual --output FRESH_ABSOLUTE_REPO_TMP_DIRECTORY [--theme dark|light] [--scenario rich|files|files-text|text] [--card-top LOGICAL_PX]");
            return Ok(());
        }
        let value = args.next().with_context(|| format!("{key} requires a value"))?;
        match key.as_str() {
            "--output" => output = Some(PathBuf::from(value)),
            "--scenario" => {
                ensure!(SCENARIOS.contains(&value.as_str()), "Invalid scenario");
                scenario = value;
            }
            "--card-top" => card_top = value.parse::<f32>().context("Invalid card top")?.clamp(MARGIN, MARGIN + MAX_HEIGHT),
            "--theme" => {
                ensure!(["dark", "light"].contains(&value.as_str()), "Invalid theme");
                dark = value == "dark";
            }
            _ => return Err(anyhow!("Unknown argument: {key}")),
        }
    }
    let run = output_directory(&output.context("--output is required")?)?;
    let report_path = run.join("hover-card-report.json");
    let screenshot = run.join("hover-card.png");
    let files = samples(&run.join("samples"))?;
    cliphost_core::force_headless(true);
    let host = HostState::open(AppPaths::from_data_root(run.join("data")), 29065).map_err(|error| anyhow!(error))?;
    let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
    let paths: Vec<String> = files.iter().map(|path| path.display().to_string()).collect();
    let (content, focus, card_width, preview_width) = match scenario.as_str() {
        "rich" => (Content::Rich(RichCard {
            parts: vec![
                RichPart::Text("@全体成员 新版窗口菜单的效果图，大家看看：".into()),
                RichPart::Image(paths[0].clone()),
                RichPart::Text("左边是现在的，右边是改完的。分组模式下同一个程序的窗口会折叠到一起。".into()),
                RichPart::Image(paths[1].clone()),
                RichPart::Text("有问题直接在群里回复～".into()),
            ],
            images: 2,
        }), None, RICH_WIDTH, 0.0),
        "files" => (Content::Files(FileCard { paths: paths.clone(), focus: Some(0) }), Some(0), FILES_WIDTH, PREVIEW_WIDTH),
        "files-text" => (Content::Files(FileCard { paths: paths.clone(), focus: Some(2) }), Some(2), FILES_WIDTH, TEXT_PREVIEW_WIDTH),
        _ => (Content::Files(FileCard { paths: vec![paths[3].clone()], focus: Some(0) }), Some(0), TEXT_CARD_WIDTH, 0.0),
    };
    let width = 2.0 * MARGIN + card_width + if preview_width > 0.0 { CASCADE_GAP + preview_width } else { 0.0 };
    let height = MAX_HEIGHT + 2.0 * MARGIN;
    let layout = CardLayout {
        right: true,
        window: (0, 0, width as i32, height as i32),
        card_x: MARGIN,
        card_width,
        preview_x: MARGIN + card_width + CASCADE_GAP,
        preview_width,
        height: MAX_HEIGHT,
        card_top,
        scale: 1.0,
    };
    // Decode up front so the capture shows ready resources, not placeholders.
    let mut images = std::collections::HashMap::new();
    let mut texts = std::collections::HashMap::new();
    let mut sizes = std::collections::HashMap::new();
    for path in &paths {
        sizes.insert(path.clone(), fs::metadata(path).ok().map(|meta| meta.len()));
        if crate::hover::content::local_raster_path(path) {
            let image = crate::native_content::list_image(fs::read(path)?).context("Decode sample image")?;
            images.insert(path.clone(), Slot::Ready(image));
        } else if crate::hover::content::local_text_path(path) {
            texts.insert(path.clone(), Slot::Ready(crate::text_preview::read_preview(Path::new(path)).map_err(|error| anyhow!(error))?));
        }
    }
    let report = Arc::new(Mutex::new(json!({"complete":false,"passed":false,"scenario":scenario,
        "theme":if dark {"dark"} else {"light"}})));
    let completed = report.clone();
    Application::new().with_assets(DesignAssets).run(move |cx| {
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let handle = cx.open_window(WindowOptions { titlebar: None, focus: false, show: false,
            is_resizable: false, is_minimizable: false, window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_background: gpui::WindowBackgroundAppearance::Opaque, ..Default::default()
        }, move |_, cx| cx.new(|_| {
            let mut card = HoverCard::new(host, tokens);
            card.dark = dark;
            card.epoch = 1;
            card.visible = true;
            card.layout = Some(layout);
            card.content = Some(content);
            card.focus = focus;
            card.images = images;
            card.texts = texts;
            card.sizes = sizes;
            card.backdrop = true;
            card
        })).expect("Open hover card fixture");
        let setup = handle.update(cx, |_, window, _| -> anyhow::Result<()> {
            platform::set_no_activate(window, true).map_err(|error| anyhow!(error))?;
            platform::set_geometry(window, None, Some((width as f64, height as f64))).map_err(|error| anyhow!(error))?;
            platform::show_no_activate(window).map_err(|error| anyhow!(error))?;
            Ok(())
        });
        let report_path = report_path.clone();
        let screenshot = screenshot.clone();
        cx.spawn(async move |cx| {
            Timer::after(Duration::from_millis(900)).await;
            let hwnd = handle.update(cx, |_, window, _| platform::hwnd(window).map(|hwnd| hwnd.0 as isize));
            let outcome = (|| -> anyhow::Result<serde_json::Value> {
                setup.map_err(|error| anyhow!(error.to_string()))??;
                let hwnd = hwnd.map_err(|error| anyhow!(error.to_string()))?.map_err(|error| anyhow!(error))?;
                capture(hwnd, &screenshot)
            })();
            let mut value = report.lock().unwrap();
            value["complete"] = json!(true);
            match outcome {
                Ok(detail) => { value["passed"] = json!(true); value["screenshot"] = detail; }
                Err(error) => value["error"] = json!(format!("{error:?}")),
            }
            let _ = fs::write(&report_path, serde_json::to_vec_pretty(&*value).unwrap());
            let _ = cx.update(|cx| cx.quit());
        }).detach();
    });
    ensure!(completed.lock().unwrap()["passed"] == true, "Hover card fixture failed");
    println!("Hover card capture passed");
    Ok(())
}

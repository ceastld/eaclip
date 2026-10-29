//! Seed explicit synthetic drag fixtures in a new workspace .tmp data root.
use cliphost_core::{model::{ClipKind, ClipRecord}, paths::AppPaths, store::AppStore};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::path::absolute(PathBuf::from(std::env::args_os().nth(1).ok_or("data root required")?))?;
    let temporary = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors().nth(5).ok_or("workspace root missing")?.join(".tmp").canonicalize()?;
    if !root.canonicalize()?.starts_with(&temporary) { return Err("Only workspace .tmp fixtures are allowed".into()); }
    let paths = AppPaths::from_data_root(root);
    paths.ensure()?;
    if paths.db.exists() { return Err("Use a new data root; database already exists".into()); }
    let store = AppStore::open(&paths.db)?;
    let text = ClipRecord::new_text("Drag 文本 🦀\r\nSecond line", "ole-fixture");
    let mut html = ClipRecord::new_text("Drag rich 中文", "ole-fixture");
    html.kind = ClipKind::HtmlClipItem;
    html.html_data = Some("<b>Drag rich 中文</b>".into());
    html.html_source_url = Some("https://example.invalid/fixture".into());
    let file_path = paths.data_root.join("Drag 文件.txt");
    std::fs::write(&file_path, "Synthetic drag file.\n")?;
    let mut file = ClipRecord::new_text("", "ole-fixture");
    file.kind = ClipKind::FileClipItem;
    file.file_paths = vec![file_path.display().to_string()];
    let image_path = paths.image_library.join("drag-image.png");
    image::RgbaImage::from_pixel(16, 12, image::Rgba([20, 90, 160, 255])).save(&image_path)?;
    let mut image = ClipRecord::new_text("", "ole-fixture");
    image.kind = ClipKind::ImageClipItem;
    image.image_path = Some(image_path.display().to_string());
    let items = [text, html, file, image];
    let mut manifest = Vec::new();
    for (index, item) in items.iter().enumerate() {
        // Fixed titles identify a visible row independently of its sorting order.
        let mut item = item.clone();
        item.title = Some(format!("OLE fixture {index}"));
        store.insert_clip(&item, true)?;
        manifest.push(serde_json::json!({"id":item.id, "kind":item.kind, "title":item.title,
            "text":item.body_text, "files":item.file_paths, "image":item.image_path}));
    }
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(())
}

//! Native LAN draft preparation. Call only from a user-initiated background task.
//! No history insert, clipboard write, listener or background-image storage.
use super::*;
use std::{
    io::{Cursor, Write},
    sync::OnceLock,
};

type DraftFiles = HashMap<PathBuf, tempfile::TempPath>;
fn owned_files() -> &'static Mutex<DraftFiles> {
    static FILES: OnceLock<Mutex<DraftFiles>> = OnceLock::new();
    FILES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn prepare_items(
    items: Vec<ClipRecord>,
    paths: &AppPaths,
    metadata: impl Fn(&Path) -> std::io::Result<std::fs::Metadata>,
) -> Result<(String, Vec<String>), String> {
    let mut text_parts = Vec::new();
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        crate::clipboard_payload::require_output_projection(&item)?;
        match item.kind {
            ClipKind::FileClipItem => {
                let mut folders = Vec::new();
                for path in &item.file_paths {
                    let path = PathBuf::from(path);
                    let Ok(info) = metadata(&path) else {
                        continue;
                    };
                    if info.is_file() {
                        let value = path.to_string_lossy().into_owned();
                        if seen.insert(value.to_ascii_lowercase()) {
                            files.push(value);
                        }
                    } else if info.is_dir() {
                        folders.push(path.to_string_lossy().into_owned());
                    }
                }
                if !folders.is_empty() {
                    text_parts.push(format!("[文件夹未自动发送] {}", folders.join(", ")));
                }
            }
            ClipKind::ImageClipItem => {
                if let Some(path) = item.image_path.as_deref().filter(|path| {
                    paths.is_allowed_image_content_file(Path::new(path))
                        && metadata(Path::new(path)).is_ok_and(|info| info.is_file())
                }) {
                    if seen.insert(path.to_ascii_lowercase()) {
                        files.push(path.into());
                    }
                }
            }
            _ => {
                let content = item
                    .body_text
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| item.csharp_summary());
                if !content.trim().is_empty() {
                    text_parts.push(content);
                }
            }
        }
    }
    let text = text_parts.join("\n\n");
    validate_payload(&text, &files)?;
    Ok((text, files))
}

fn validate_payload(text: &str, files: &[String]) -> Result<(), String> {
    if text.len() > crate::lan::MAX_TEXT_LENGTH {
        return Err("消息正文过长。".into());
    }
    if files.len() > crate::lan::MAX_ATTACHMENTS {
        return Err("附件数量过多。".into());
    }
    Ok(())
}

fn validate_png(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > 128 * 1024 * 1024 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("剪贴板图片无效或过大。".into());
    }
    let (width, height) =
        image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png)
            .into_dimensions()
            .map_err(|error| format!("无法读取剪贴板图片：{error}"))?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 64_000_000 {
        return Err("剪贴板图片尺寸过大。".into());
    }
    Ok(())
}

impl HostState {
    /// A snapshot of the most recently copied history item, independent of pins.
    pub fn lan_compose_recent(&self) -> Result<Value, String> {
        let (store, paths) = {
            let inner = self.inner.lock();
            (inner.store.clone(), inner.paths.clone())
        };
        let item = {
            let db = store.lock();
            let id = db
                .list_clip_keys()
                .map_err(|error| error.to_string())?
                .into_iter()
                .max_by_key(|key| key.create_ms)
                .ok_or("还没有最近复制的条目。")?
                .id;
            let item=db.get_clip(&id).map_err(|error|error.to_string())?;
            db.core_io_clip(&item).map_err(|error|error.to_string())?
        };
        let kind = match item.kind {
            ClipKind::ImageClipItem => "image",
            ClipKind::FileClipItem => "file",
            _ => "text",
        };
        let id = item.id.clone();
        let label: String = item.csharp_summary().chars().take(120).collect();
        let (text, files) = prepare_items(vec![item], &paths, |path| std::fs::metadata(path))?;
        if text.trim().is_empty() && files.is_empty() {
            return Err("最近条目没有可发送的文字或文件。".into());
        }
        Ok(
            json!({"id":id,"text":text,"filePaths":files,"ownedFilePaths":[],"label":label,"kind":kind}),
        )
    }

    /// Targeted clipboard formats only. Foreign rendering and file I/O are not UI work.
    pub fn lan_compose_clipboard(&self) -> Result<Value, String> {
        self.lan_compose_snapshot(crate::clipboard::read_lan_paste()?)
    }

    fn lan_compose_snapshot(&self, snapshot: ClipboardSnapshot) -> Result<Value, String> {
        let text = snapshot.text.unwrap_or_default();
        let mut files = snapshot.files;
        validate_payload(&text, &files)?;
        let mut owned = Vec::new();
        let mut kind = if files.is_empty() { "text" } else { "file" };
        if files.is_empty() {
            if let Some(png) = snapshot.png {
                validate_png(&png)?;
                let root = self.lan.lock().lan_root.join("Drafts");
                std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
                let mut image = tempfile::Builder::new()
                    .prefix("paste-")
                    .suffix(".png")
                    .tempfile_in(&root)
                    .map_err(|error| error.to_string())?;
                image.write_all(&png).map_err(|error| error.to_string())?;
                image.flush().map_err(|error| error.to_string())?;
                let path = image.into_temp_path();
                let filename = path.to_string_lossy().into_owned();
                owned_files().lock().insert(path.to_path_buf(), path);
                files.push(filename.clone());
                owned.push(filename);
                kind = "image";
            } else if snapshot.has_image_format {
                return Err("暂时无法读取剪贴板图片，请重新复制后粘贴。".into());
            }
        }
        let label = match kind {
            "image" => "剪贴板图片".into(),
            "file" if files.len() == 1 => Path::new(&files[0])
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            "file" => format!("{} 个文件", files.len()),
            _ => text.chars().take(120).collect::<String>(),
        };
        Ok(
            json!({"id":Uuid::new_v4().simple().to_string(),"text":text,"filePaths":files,"ownedFilePaths":owned,"label":label,"kind":kind}),
        )
    }

    /// Only exact temporary files created here can be removed. Caller paths never
    /// authorize arbitrary deletion; duplicate or unknown cleanup is a no-op.
    pub fn lan_compose_discard(&self, paths: &[String]) -> Result<(), String> {
        let root = self.lan.lock().lan_root.join("Drafts");
        let mut retired = Vec::new();
        {
            let mut owned = owned_files().lock();
            for path in paths {
                let path = PathBuf::from(path);
                if path.parent() == Some(root.as_path()) {
                    if let Some(file) = owned.remove(&path) {
                        retired.push(file);
                    }
                }
            }
        }
        drop(retired);
        Ok(())
    }

    pub fn lan_compose_discard_all(&self) -> Result<(), String> {
        let root = self.lan.lock().lan_root.join("Drafts");
        let paths: Vec<String> = owned_files()
            .lock()
            .keys()
            .filter(|path| path.parent() == Some(root.as_path()))
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        self.lan_compose_discard(&paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_core_lan_payload_rejects_before_metadata(){
        let mut item=ClipRecord::new_text("","fixture");item.kind=ClipKind::FileClipItem;
        item.storage_revision=Some("core2:synthetic".into());item.file_paths=vec![r"Z:\offline-never-probe\file.txt".into()];
        let paths=AppPaths::from_data_root(PathBuf::from(r"Z:\offline-never-probe"));
        assert!(prepare_items(vec![item],&paths,|_|panic!("canonical payload reached metadata")).is_err());
    }

    #[test]
    #[cfg(windows)]
    fn core_recent_lan_files_use_candidate_and_refuse_unmapped_external(){
        crate::clipboard::force_headless(true);
        let parent=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");std::fs::create_dir_all(&parent).unwrap();
        let parent=parent.canonicalize().unwrap();
        let directory=tempfile::Builder::new().prefix("lan-compose-core-").tempdir_in(parent).unwrap();
        let source=directory.path().join("source");std::fs::create_dir_all(source.join("Documents")).unwrap();
        let source_file=source.join("Documents/file.txt");std::fs::write(&source_file,b"candidate attachment").unwrap();
        let native=AppStore::open(&source.join("userdata.db")).unwrap();
        let mut item=ClipRecord::new_text("","fixture");item.kind=ClipKind::FileClipItem;
        item.file_paths=vec![source_file.to_string_lossy().into_owned()];item.content_hash=hash_files(&item.file_paths);
        native.insert_clip(&item,true).unwrap();drop(native);
        let candidate=directory.path().join("candidate");
        crate::storage_core::migrate::build_candidate(&crate::storage_core::migrate::MigrationOptions{source_root:source.clone(),candidate_root:candidate.clone()},&mut |_|Ok(())).unwrap();
        let host=HostState::open_isolated_core(AppPaths::from_data_root(candidate.clone()),29065).unwrap();
        let count=host.store().lock().counts().unwrap();let draft=host.lan_compose_recent().unwrap();
        assert_eq!(draft["id"],item.id);assert_eq!(draft["filePaths"].as_array().unwrap().len(),1);
        let projected=draft["filePaths"][0].as_str().unwrap();
        assert_eq!(std::fs::canonicalize(projected).unwrap(),std::fs::canonicalize(candidate.join("Documents/file.txt")).unwrap());
        assert_eq!(host.store().lock().counts().unwrap(),count);
        let outside=directory.path().join("unmapped.txt");std::fs::write(&outside,b"external synthetic unchanged").unwrap();
        let mut blocked=item.clone();blocked.id="unmapped-latest".into();blocked.file_paths=vec![outside.to_string_lossy().into_owned()];blocked.create_time+=chrono::Duration::hours(1);blocked.content_hash=hash_files(&blocked.file_paths);
        host.store().lock().insert_clip(&blocked,true).unwrap();
        assert!(host.lan_compose_recent().is_err());
        assert_eq!(std::fs::read(&outside).unwrap(),b"external synthetic unchanged");assert_eq!(std::fs::read(&source_file).unwrap(),b"candidate attachment");
    }
    fn fixture() -> (tempfile::TempDir, HostState) {
        crate::clipboard::force_headless(true);
        let root = tempfile::tempdir().unwrap();
        let roaming = root.path().join("profile/Roaming");
        let local = root.path().join("profile/Local");
        std::fs::create_dir_all(&roaming).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        std::env::set_var("APPDATA", roaming);
        std::env::set_var("LOCALAPPDATA", local);
        let host = HostState::open(
            AppPaths::from_data_root(root.path().join("BetterClipTauri")),
            29065,
        )
        .unwrap();
        (root, host)
    }
    #[test]
    fn recent_is_copy_order_not_pin_order_and_does_not_modify_history() {
        let (_root, host) = fixture();
        let mut old = ClipRecord::new_text("older pinned", "fixture");
        old.is_top = true;
        old.create_time -= chrono::Duration::hours(1);
        let recent = ClipRecord::new_text("new copied text", "fixture");
        host.store().lock().insert_clip(&old, true).unwrap();
        host.store().lock().insert_clip(&recent, true).unwrap();
        let prepared = host.lan_compose_recent().unwrap();
        assert_eq!(prepared["id"], recent.id);
        assert_eq!(prepared["text"], "new copied text");
        assert_eq!(host.store().lock().list_clips().unwrap().len(), 2);
    }
    #[test]
    fn mixed_paste_keeps_unicode_and_real_file_paths_without_parsing_text() {
        let (_root, host) = fixture();
        let prepared = host
            .lan_compose_snapshot(ClipboardSnapshot {
                text: Some("说明😀\nC:\\not-a-file".into()),
                files: vec!["D:\\actual.txt".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(prepared["filePaths"], json!(["D:\\actual.txt"]));
        assert_eq!(prepared["text"], "说明😀\nC:\\not-a-file");
        assert_eq!(prepared["ownedFilePaths"], json!([]));
    }
    #[test]
    fn screenshot_lives_in_lan_drafts_and_cleanup_cannot_delete_foreign_files() {
        let (root, host) = fixture();
        let prepared = host
            .lan_compose_snapshot(ClipboardSnapshot {
                png: Some(
                    include_bytes!("../../../../ui-design/assets/clip-main-thumbnail.png").to_vec(),
                ),
                ..Default::default()
            })
            .unwrap();
        let owned = prepared["ownedFilePaths"][0].as_str().unwrap().to_string();
        assert!(Path::new(&owned).is_file());
        assert!(owned.contains("LanShare"));
        assert!(owned.contains("Drafts"));
        let foreign = root.path().join("keep.png");
        std::fs::write(&foreign, b"keep").unwrap();
        host.lan_compose_discard(&[foreign.to_string_lossy().into_owned(), owned.clone()])
            .unwrap();
        assert!(!Path::new(&owned).exists());
        assert_eq!(std::fs::read(foreign).unwrap(), b"keep");
        host.lan_compose_discard(&[owned]).unwrap();
    }
    #[test]
    fn malformed_image_and_excessive_payload_do_not_create_draft_files() {
        let (_root, host) = fixture();
        assert!(host
            .lan_compose_snapshot(ClipboardSnapshot {
                png: Some(vec![0; 10]),
                ..Default::default()
            })
            .is_err());
        assert!(host
            .lan_compose_snapshot(ClipboardSnapshot {
                text: Some("x".repeat(crate::lan::MAX_TEXT_LENGTH + 1)),
                ..Default::default()
            })
            .is_err());
        assert!(host
            .lan_compose_snapshot(ClipboardSnapshot {
                has_image_format: true,
                ..Default::default()
            })
            .is_err());
        assert!(!host.lan.lock().lan_root.join("Drafts").exists());
    }
}

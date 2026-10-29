//! Import a committed C# wrapper snapshot into a new, isolated Rust data root.
//! The destination is published only after records and referenced images succeed.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;

use crate::csharp_import_document::{parse_clip, parse_datetime, parse_favor};
use crate::model::{ClipKind, ClipRecord, FavorRecord};
use crate::paths::AppPaths;
use crate::store::{clip_usage_key, AppStore, UsageInfo};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub schema_version: u32,
    pub clips: usize,
    pub favorites: usize,
    pub images: usize,
    pub usage_records: usize,
    pub configs: usize,
    pub history_limit: i32,
    pub dry_run: bool,
}

struct Document {
    table: &'static str,
    id: String,
    data: Value,
    metadata: Value,
}
struct Image {
    source: PathBuf,
    relative: PathBuf,
    bytes: Vec<u8>,
}
struct Snapshot {
    documents: Vec<Document>,
    clips: Vec<ClipRecord>,
    favorites: Vec<FavorRecord>,
    usage: Vec<(String, UsageInfo)>,
}

/// No merge or replacement: import only into an absent or empty data folder.
pub fn import_csharp_data(
    source_root: &Path,
    destination_root: &Path,
    dry_run: bool,
) -> Result<ImportReport, String> {
    let source = fs::canonicalize(source_root).map_err(|e| format!("无法读取 C# 数据目录：{e}"))?;
    if !source.is_dir() {
        return Err("C# 来源必须是包含 userdata.db 的目录。".into());
    }
    let destination =
        crate::data_location::DataLocation::from_environment().normalize_root(destination_root)?;
    let destination = future_path(&destination)?;
    if under(&destination, &source) || under(&source, &destination) {
        return Err("来源和目标数据目录不能相同或互相包含。".into());
    }
    require_empty_destination(&destination)?;
    let database = fs::canonicalize(source.join("userdata.db"))
        .map_err(|e| format!("无法读取 C# 数据库：{e}"))?;
    if !under(&database, &source) || !database.is_file() {
        return Err("C# 数据库解析后不在来源目录中。".into());
    }
    let mut snapshot = read_snapshot(&database)?;
    link_favorite_tree(&mut snapshot.favorites)?;
    let configs = crate::csharp_import_configs::prepare_configs(
        &source,
        snapshot.clips.iter().filter(|item| !item.is_top).count(),
    )?;
    let mut images = Vec::new();
    let mut image_paths = HashMap::new();
    let usage_by_id: HashMap<_, _> = snapshot
        .usage
        .iter()
        .map(|(key, info)| (key.as_str(), info))
        .collect();
    for item in &mut snapshot.clips {
        let image_index = migrate_image_path(
            &source,
            &destination,
            &mut item.image_path,
            &mut images,
            &mut image_paths,
        )?;
        if item.kind == ClipKind::ImageClipItem {
            if let Some(index) = image_index {
                item.content_hash = crate::model::sha256_hex(&images[index].bytes);
            }
        }
        rewrite_rich_images(item, &source, &destination, &mut images, &mut image_paths)
            .map_err(|error| format!("历史条目 {}：{error}", item.id))?;
        if let Some(usage) = usage_by_id.get(clip_usage_key(&item.id).as_str()) {
            item.last_access = usage.last_use_time;
            item.usage_count = usage.usage_count;
        }
    }
    for item in &mut snapshot.favorites {
        migrate_image_path(
            &source,
            &destination,
            &mut item.image_path,
            &mut images,
            &mut image_paths,
        )?;
        migrate_owned_icon(
            &source,
            &destination,
            &mut item.icon,
            &mut images,
            &mut image_paths,
        )?;
        let mut rich = ClipRecord::new_text("", &item.source_process_name);
        rich.kind = item.kind;
        rich.html_data = item.html_data.clone();
        rich.html_source_url = item.html_source_url.clone();
        rich.qq_rich_text = item.qq_rich_text.clone();
        rewrite_rich_images(
            &mut rich,
            &source,
            &destination,
            &mut images,
            &mut image_paths,
        )
        .map_err(|error| format!("收藏条目 {}：{error}", item.id))?;
        item.html_data = rich.html_data;
        item.qq_rich_text = rich.qq_rich_text;
    }
    let clip_config = configs
        .iter()
        .find(|(path, _)| path == Path::new("Configs/ClipConfig.json"))
        .ok_or("缺少准备好的剪贴板配置。")?;
    let history_limit = serde_json::from_slice::<Value>(&clip_config.1)
        .map_err(|e| e.to_string())?["maxItemSize"]
        .as_i64()
        .ok_or("历史上限配置无效。")? as i32;
    let report = ImportReport {
        schema_version: 1,
        clips: snapshot.clips.len(),
        favorites: snapshot.favorites.len(),
        images: images.len(),
        usage_records: snapshot.usage.len(),
        configs: configs
            .iter()
            .filter(|(path, _)| path.starts_with("Configs"))
            .count(),
        history_limit,
        dry_run,
    };
    if dry_run {
        return Ok(report);
    }

    let parent = destination.parent().ok_or("目标数据目录无父目录。")?;
    fs::create_dir_all(parent).map_err(|e| format!("无法创建目标父目录：{e}"))?;
    let stage = tempfile::Builder::new()
        .prefix(".cliphost-import-")
        .tempdir_in(parent)
        .map_err(|e| e.to_string())?;
    let paths = AppPaths::from_data_root(stage.path().to_path_buf());
    paths.ensure().map_err(|e| e.to_string())?;
    for (relative, bytes) in &configs {
        let path = paths.data_root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let mut file = fs::File::create(path).map_err(|e| e.to_string())?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
    }
    for image in &images {
        let path = paths.data_root.join(&image.relative);
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let mut file = fs::File::create(&path).map_err(|e| e.to_string())?;
        file.write_all(&image.bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        // Referenced files are outside SQLite's snapshot. Refuse to publish if
        // one changed during preparation; never retain a pointer to C# storage.
        if crate::model::sha256_hex(
            &fs::read(&image.source).map_err(|e| format!("来源图片已变化或不可读：{e}"))?,
        ) != crate::model::sha256_hex(&image.bytes)
        {
            return Err("准备导入期间来源图片已变化，请稍后重试。".into());
        }
    }
    let store = AppStore::open(&paths.db).map_err(|e| e.to_string())?;
    store
        .import_csharp_records(&snapshot.clips, &snapshot.favorites, &snapshot.usage)
        .map_err(|e| e.to_string())?;
    drop(store);
    let archive_dir = paths.data_root.join("migration");
    fs::create_dir_all(&archive_dir).map_err(|e| e.to_string())?;
    let mut archive =
        fs::File::create(archive_dir.join("source-documents.jsonl")).map_err(|e| e.to_string())?;
    for row in &snapshot.documents {
        serde_json::to_writer(&mut archive, &serde_json::json!({"table":row.table,"id":row.id,"data":row.data,"wrapperMetadata":row.metadata})).map_err(|e| e.to_string())?;
        archive.write_all(b"\n").map_err(|e| e.to_string())?;
    }
    archive.sync_all().map_err(|e| e.to_string())?;
    drop(archive);
    fs::write(
        archive_dir.join("report.json"),
        serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    require_empty_destination(&destination)?;
    let had_empty_destination = destination.exists();
    if had_empty_destination {
        fs::remove_dir(&destination).map_err(|e| e.to_string())?;
    }
    if let Err(error) = fs::rename(stage.path(), &destination) {
        if had_empty_destination {
            let _ = fs::create_dir(&destination);
        }
        return Err(format!("无法发布完整导入目录：{error}"));
    }
    Ok(report)
}

fn read_snapshot(db: &Path) -> Result<Snapshot, String> {
    let sidecar = |suffix: &str| {
        let mut path = db.as_os_str().to_os_string();
        path.push(suffix);
        PathBuf::from(path)
    };
    if sidecar("-wal").exists() && !sidecar("-shm").exists() {
        return Err("来源 WAL 缺少只读所需的共享文件，请先正常关闭 C# 程序后重试。".into());
    }
    let connection = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("无法只读打开 C# userdata.db：{e}"))?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    connection
        .execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED;")
        .map_err(|e| e.to_string())?;
    let mut snapshot = Snapshot {
        documents: Vec::new(),
        clips: Vec::new(),
        favorites: Vec::new(),
        usage: Vec::new(),
    };
    for table in ["clip_item", "favor_item", "UsageInfo"] {
        let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND lower(name)=lower(?1))", [table], |row| row.get(0)).map_err(|e| e.to_string())?;
        if !exists {
            if table == "UsageInfo" {
                continue;
            }
            return Err("来源不是当前 C#/WPF 数据格式，请先用 C# 版本升级旧数据。".into());
        }
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{table}\" ORDER BY Id"))
            .map_err(|_| "来源不是 C# 文档包装格式，不能直接导入 Rust 数据库。".to_string())?;
        let columns: Vec<_> = statement
            .column_names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        let id_index = columns
            .iter()
            .position(|name| name.eq_ignore_ascii_case("Id"))
            .ok_or("来源缺少 Id 列。")?;
        let data_index = columns
            .iter()
            .position(|name| name.eq_ignore_ascii_case("Data"))
            .ok_or("来源不是 C# 文档包装格式。")?;
        let mut rows = statement.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let id: String = row.get(id_index).map_err(|e| e.to_string())?;
            let data: String = row.get(data_index).map_err(|e| e.to_string())?;
            let mut metadata = serde_json::Map::new();
            for (index, name) in columns
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != data_index)
            {
                let value = match row.get_ref(index).map_err(|e| e.to_string())? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(value) => Value::from(value),
                    ValueRef::Real(value) => Value::Number(
                        serde_json::Number::from_f64(value).ok_or("包装元数据含无效浮点数。")?,
                    ),
                    ValueRef::Text(value) => Value::String(
                        std::str::from_utf8(value)
                            .map_err(|_| "包装元数据不是有效 UTF-8。")?
                            .to_string(),
                    ),
                    ValueRef::Blob(value) => {
                        serde_json::json!({"sqliteBlobBase64":base64::engine::general_purpose::STANDARD.encode(value)})
                    }
                };
                metadata.insert(name.clone(), value);
            }
            let data: Value = serde_json::from_str(&data)
                .map_err(|_| format!("{table} 条目 {id} 的 JSON 无效，已停止导入。"))?;
            match table {
                "clip_item" => snapshot
                    .clips
                    .push(parse_clip(&id, &data).map_err(|e| format!("历史条目 {id}：{e}"))?),
                "favor_item" => snapshot
                    .favorites
                    .push(parse_favor(&id, &data).map_err(|e| format!("收藏条目 {id}：{e}"))?),
                _ => snapshot.usage.push((id.clone(), parse_usage(&id, &data)?)),
            }
            snapshot.documents.push(Document {
                table,
                id,
                data,
                metadata: Value::Object(metadata),
            });
        }
    }
    connection
        .execute_batch("COMMIT")
        .map_err(|e| e.to_string())?;
    Ok(snapshot)
}

fn parse_usage(id: &str, data: &Value) -> Result<UsageInfo, String> {
    if data.get("Id").and_then(Value::as_str) != Some(id) {
        return Err(format!("使用统计 {id} 的文档 ID 不一致。"));
    }
    let count = data
        .get("UsageCount")
        .and_then(Value::as_i64)
        .filter(|value| (0..=i32::MAX as i64).contains(value))
        .ok_or_else(|| format!("使用统计 {id} 的次数无效。"))? as i32;
    let value = data
        .get("LastUseTime")
        .ok_or_else(|| format!("使用统计 {id} 缺少时间。"))?;
    let last_use_time = if value
        .as_str()
        .is_some_and(|value| value.starts_with("0001-01-01T00:00:00"))
    {
        None
    } else {
        Some(parse_datetime(value).map_err(|e| format!("使用统计 {id}：{e}"))?)
    };
    if last_use_time.is_none() && count != 0 {
        return Err(format!("使用统计 {id} 有次数但没有有效时间。"));
    }
    Ok(UsageInfo {
        last_use_time,
        usage_count: count,
    })
}

fn link_favorite_tree(items: &mut [FavorRecord]) -> Result<(), String> {
    let ids: HashMap<_, _> = items
        .iter()
        .enumerate()
        .map(|(index, item)| (item.id.clone(), index))
        .collect();
    if ids.len() != items.len() {
        return Err("收藏包含重复 ID。".into());
    }
    let mut parents = HashMap::new();
    for item in items.iter() {
        if !item.is_folder && !item.children.is_empty() {
            return Err(format!("收藏 {} 不是目录但含有子条目。", item.id));
        }
        for child in &item.children {
            if !ids.contains_key(child) {
                return Err(format!("收藏目录 {} 的子条目 {} 不存在。", item.id, child));
            }
            if parents.insert(child.clone(), item.id.clone()).is_some() {
                return Err(format!("收藏 {} 有重复父目录或重复链接。", child));
            }
        }
    }
    for item in items.iter_mut() {
        let parent = parents.get(&item.id).cloned();
        if item
            .parent_id
            .as_ref()
            .is_some_and(|declared| parent.as_ref() != Some(declared))
        {
            return Err(format!("收藏 {} 的父目录与子列表不一致。", item.id));
        }
        item.parent_id = parent;
        let mut seen = HashSet::new();
        let mut current = item.id.as_str();
        while let Some(parent) = parents.get(current) {
            if !seen.insert(current) {
                return Err("收藏目录存在循环，已停止导入。".into());
            }
            current = parent;
        }
    }
    Ok(())
}

fn rewrite_rich_images(
    item: &mut ClipRecord,
    source: &Path,
    destination: &Path,
    images: &mut Vec<Image>,
    known: &mut HashMap<PathBuf, (usize, String)>,
) -> Result<(), String> {
    crate::csharp_import_assets::rewrite_embedded_images(item, &mut |raw| {
        let mut value = Some(raw.to_string());
        migrate_image_path(source, destination, &mut value, images, known)?;
        value.ok_or_else(|| "图片迁移未生成目标路径。".to_string())
    })
}

fn migrate_owned_icon(
    source: &Path,
    destination: &Path,
    icon: &mut String,
    images: &mut Vec<Image>,
    known: &mut HashMap<PathBuf, (usize, String)>,
) -> Result<(), String> {
    let (prefix, raw) = icon
        .strip_prefix("icon:")
        .map(|raw| ("icon:", raw))
        .unwrap_or(("", icon.as_str()));
    let file_url = raw.to_ascii_lowercase().starts_with("file:");
    let path = if file_url {
        PathBuf::from(
            crate::explorer::file_url_to_local_path(raw).ok_or("收藏图标 file: URL 无效。")?,
        )
    } else {
        PathBuf::from(raw)
    };
    let relative_image = !path.is_absolute()
        && path.components().next().is_some_and(|part| {
            part.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case("Image")
        });
    if !path.is_absolute() && !relative_image {
        return Ok(());
    }
    let path = if relative_image {
        source.join(&path)
    } else {
        path
    };
    // User-selected files outside C# storage remain ordinary external refs.
    if !under(&future_path(&path)?, source) {
        return Ok(());
    }
    let mut mapped = Some(path.to_string_lossy().into_owned());
    migrate_image_path(source, destination, &mut mapped, images, known)?;
    let mapped = mapped.ok_or("收藏图标未生成目标路径。")?;
    *icon = format!("{prefix}{mapped}");
    Ok(())
}

fn migrate_image_path(
    source: &Path,
    destination: &Path,
    value: &mut Option<String>,
    images: &mut Vec<Image>,
    known: &mut HashMap<PathBuf, (usize, String)>,
) -> Result<Option<usize>, String> {
    let Some(raw) = value.as_ref() else {
        return Ok(None);
    };
    let library =
        fs::canonicalize(source.join("Image")).map_err(|e| format!("来源图片目录不可读：{e}"))?;
    if !under(&library, source) {
        return Err("来源 Image 库解析后不在来源目录中。".into());
    }
    let original = Path::new(raw);
    // Relative image records normally start at Image/, but embedded markup
    // can include that directory name explicitly.
    let mut components = original.components();
    let original = if !original.is_absolute()
        && components.next().is_some_and(|part| {
            part.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case("Image")
        }) {
        components.as_path()
    } else {
        original
    };
    let original = if original.is_absolute() {
        original.to_path_buf()
    } else {
        library.join(original)
    };
    let original =
        fs::canonicalize(original).map_err(|e| format!("引用图片不存在或不可读：{e}"))?;
    if !under(&original, &library) || !original.is_file() {
        return Err("来源图片不在来源 Image 库中，无法完整迁移外部图片引用。".into());
    }
    if let Some((index, mapped)) = known.get(&original) {
        *value = Some(mapped.clone());
        return Ok(Some(*index));
    }
    let bytes = fs::read(&original).map_err(|e| e.to_string())?;
    let extension = original
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 12
                && value.bytes().all(|value| value.is_ascii_alphanumeric())
        })
        .ok_or("来源图片没有可用的文件扩展名。")?;
    let index = images.len();
    let relative = PathBuf::from("Image").join("Imported").join(format!(
        "{}-{index}.{}",
        crate::model::sha256_hex(&bytes),
        extension.to_ascii_lowercase()
    ));
    let mapped = destination.join(&relative).to_string_lossy().into_owned();
    #[cfg(windows)]
    let mapped = if let Some(rest) = mapped.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        mapped.strip_prefix(r"\\?\").unwrap_or(&mapped).to_string()
    };
    known.insert(original.clone(), (index, mapped.clone()));
    images.push(Image {
        source: original,
        relative,
        bytes,
    });
    *value = Some(mapped);
    Ok(Some(index))
}

fn require_empty_destination(destination: &Path) -> Result<(), String> {
    if destination.exists()
        && (!destination.is_dir()
            || fs::read_dir(destination)
                .map_err(|e| e.to_string())?
                .next()
                .is_some())
    {
        return Err("导入目标必须是不存在或空的数据目录；不会合并或覆盖现有 Rust 数据。".into());
    }
    Ok(())
}

fn future_path(path: &Path) -> Result<PathBuf, String> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    while !ancestor.exists() {
        missing.push(ancestor.file_name().ok_or("目标路径无效。")?.to_os_string());
        ancestor = ancestor.parent().ok_or("目标路径无父目录。")?;
    }
    let mut resolved = fs::canonicalize(ancestor).map_err(|e| e.to_string())?;
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn under(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let path = path.to_string_lossy().to_ascii_lowercase();
        let root = root
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase();
        path == root || path.starts_with(&(root + "\\"))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}

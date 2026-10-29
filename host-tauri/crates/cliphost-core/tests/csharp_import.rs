//! Synthetic C# persistence fixtures; never reads installed user data or the clipboard.
use std::{collections::BTreeMap, fs, path::{Path, PathBuf}};

use chrono::{DateTime, Utc};
use cliphost_core::{csharp_import::import_csharp_data, AppStore, ClipKind};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const CREATED: &str = "2026-10-01T08:12:34.567+08:00";
const UPDATED: &str = "2026-10-02T10:23:45.678+08:00";
const USED: &str = "2026-10-03T12:34:56.789+08:00";

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    destination: PathBuf,
    image: PathBuf,
    file: PathBuf,
    directory: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("csharp-source");
        let destination = temp.path().join("rust-import");
        let image = source.join("Image/Temp/shared.png");
        let file = source.join("documents/文档.txt");
        let directory = source.join("documents");
        fs::create_dir_all(image.parent().unwrap()).unwrap();
        fs::create_dir_all(&directory).unwrap();
        fs::write(&file, "ordinary external file reference").unwrap();
        image::RgbaImage::from_pixel(2, 2, image::Rgba([31, 63, 127, 255]))
            .save(&image).unwrap();

        let connection = Connection::open(source.join("userdata.db")).unwrap();
        // Actual C# wrappers use Id/Data. UsageInfoAtomicStore.cs also reads Id/Data;
        // LastUseTime and UsageCount belong to the serialized payload, not columns.
        connection.execute_batch(
            "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL,
                ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
             CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL,
                ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
             CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY, Data TEXT NOT NULL,
                ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);"
        ).unwrap();

        // Numeric IDs are the persisted ClipItemTypeIds.cs contract, including
        // Directory=30 (a FileClipItem with the directory discriminator).
        let clips = [
            ("clip-text", json!({"ObjType":1,"Text":"第一行\r\n中文🦀"})),
            ("clip-file", json!({"ObjType":2,"FileList":[file.to_string_lossy()]})),
            ("clip-image", json!({"ObjType":3,"ImagePath":"Temp/shared.png",
                "ImageHash":cliphost_core::model::sha256_hex(&fs::read(&image).unwrap()),
                "PerceptualHash":"0101010101010101"})),
            ("clip-html", json!({"ObjType":10,"Text":"富文本","HtmlData":"<b>富文本</b>",
                "HtmlSourceUrl":"https://example.test/page"})),
            ("clip-qq", json!({"ObjType":11,"Text":"QQ文本","HtmlData":"<i>QQ文本</i>",
                "QQRichText":"<QQRichEditFormat><EditElement type=\"0\" text=\"QQ文本\" /></QQRichEditFormat>"})),
            ("clip-link", json!({"ObjType":20,"Title":"网页","Url":"https://example.test/link",
                "Icon":"fa:link"})),
            ("clip-command", json!({"ObjType":21,"Command":"explorer.exe"})),
            ("clip-directory", json!({"ObjType":30,"FileList":[directory.to_string_lossy()]})),
            ("clip-steps", json!({"ObjType":64,"StepData":"{\"steps\":[]}","StepSummary":"步骤"})),
            ("clip-action", json!({"ObjType":65,"ActionItem":{"Id":"00000000-0000-0000-0000-000000000123",
                "Title":"fixture action","Description":"fixture description","Icon":"fa:bolt"},
                "ExecuteParam":"fixture parameter"})),
        ];
        for (id, item) in clips {
            insert(&connection, "clip_item", id, record(id, item));
        }
        let mut folder = record("favorite-root", json!({"ObjType":100}));
        folder["Title"] = json!("我的收藏");
        folder["Children"] = json!(["favorite-text", "favorite-image"]);
        insert(&connection, "favor_item", "favorite-root", folder);
        let mut favorite = record("favorite-text", json!({"ObjType":1,"Text":"收藏正文"}));
        favorite["Title"] = json!("收藏标题");
        favorite["Icon"] = json!("fa:star");
        favorite["Children"] = json!([]);
        insert(&connection, "favor_item", "favorite-text", favorite);
        let mut favorite = record("favorite-image", json!({"ObjType":3,"ImagePath":"Temp/shared.png"}));
        favorite["Children"] = json!([]);
        insert(&connection, "favor_item", "favorite-image", favorite);
        for (id, count) in [("clip_item_clip-text", 7), ("favor_item_favorite-text", 5)] {
            insert(&connection, "UsageInfo", id,
                json!({"Id":id,"LastUseTime":USED,"UsageCount":count}));
        }
        drop(connection);
        Self { _temp:temp, source, destination, image, file, directory }
    }

    fn modify(&self, sql: &str) {
        Connection::open(self.source.join("userdata.db")).unwrap()
            .execute_batch(sql).unwrap();
    }
}

fn record(id: &str, item: Value) -> Value {
    json!({"Id":id,"Item":item,"Title":"保存标题","Description":"保存备注",
        "CreateTime":CREATED,"UpdateTime":UPDATED,"IsTop":true,"SubIndex":3,
        "SourceProcessName":"fixture.exe"})
}

fn insert(connection: &Connection, table: &str, id: &str, data: Value) {
    connection.execute(&format!("INSERT INTO {table}(Id,Data,CreatedTime,UpdateTime) VALUES(?1,?2,?3,?4)"),
        params![id, data.to_string(), CREATED, UPDATED]).unwrap();
}

fn timestamp(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value).unwrap().with_timezone(&Utc)
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() { visit(root, &path, result); }
            else { result.insert(path.strip_prefix(root).unwrap().to_owned(), fs::read(path).unwrap()); }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

fn report_json<T: serde::Serialize>(report: &T) -> Value {
    serde_json::to_value(report).unwrap()
}

#[test]
fn import_preserves_csharp_payloads_tree_metadata_usage_and_copies_shared_image_once() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.source);
    let report = import_csharp_data(&fixture.source, &fixture.destination, false).unwrap();
    let report = report_json(&report);
    assert_eq!(report["clips"], 10);
    assert_eq!(report["favorites"], 3);
    assert_eq!(report["images"], 1);
    let store = AppStore::open(&fixture.destination.join("userdata.db")).unwrap();
    assert_eq!(store.list_clips().unwrap().len(), 10);
    assert_eq!(store.list_favors().unwrap().len(), 3);

    let text = store.get_clip("clip-text").unwrap();
    assert_eq!(text.body_text.as_deref(), Some("第一行\r\n中文🦀"));
    assert_eq!(text.title.as_deref(), Some("保存标题"));
    assert_eq!(text.description, "保存备注");
    assert_eq!(text.create_time, timestamp(CREATED));
    assert_eq!(text.update_time, timestamp(UPDATED));
    assert!(text.is_top);
    assert_eq!(text.sub_index, 3);
    assert_eq!(text.source_process_name, "fixture.exe");
    assert_eq!(text.usage_count, 7);
    assert_eq!(text.last_access, Some(timestamp(USED)));
    assert_eq!(store.get_usage("favor_item_favorite-text").unwrap().usage_count, 5);
    assert_eq!(store.get_usage("favor_item_favorite-text").unwrap().last_use_time, Some(timestamp(USED)));

    let html = store.get_clip("clip-html").unwrap();
    assert_eq!(html.kind, ClipKind::HtmlClipItem);
    assert_eq!(html.html_data.as_deref(), Some("<b>富文本</b>"));
    assert_eq!(html.html_source_url.as_deref(), Some("https://example.test/page"));
    let qq = store.get_clip("clip-qq").unwrap();
    assert_eq!(qq.kind, ClipKind::QQRichTextItem);
    assert!(qq.qq_rich_text.as_deref().unwrap().contains("QQ文本"));
    assert_eq!(store.get_clip("clip-file").unwrap().file_paths, vec![fixture.file.to_string_lossy().into_owned()]);
    assert_eq!(store.get_clip("clip-directory").unwrap().file_paths, vec![fixture.directory.to_string_lossy().into_owned()]);
    assert_eq!(store.get_clip("clip-directory").unwrap().kind, ClipKind::FileClipItem);
    assert_eq!(store.get_clip("clip-link").unwrap().body_text.as_deref(), Some("https://example.test/link"));
    assert_eq!(store.get_clip("clip-command").unwrap().body_text.as_deref(), Some("explorer.exe"));
    assert_eq!(store.get_clip("clip-steps").unwrap().body_text.as_deref(), Some("{\"steps\":[]}"));
    let action = store.get_clip("clip-action").unwrap();
    assert_eq!(action.kind, ClipKind::ActionClipItem);
    assert_eq!(action.execute_param, "fixture parameter");
    let action_json: Value = serde_json::from_str(action.body_text.as_deref().unwrap()).unwrap();
    let action_json = action_json.get("ActionItem").unwrap_or(&action_json);
    assert_eq!(action_json["Id"], "00000000-0000-0000-0000-000000000123");
    assert_eq!(action_json["Title"], "fixture action");

    let folder = store.get_favor("favorite-root").unwrap();
    assert!(folder.is_folder);
    assert_eq!(folder.kind, ClipKind::FavoriteClipItem);
    assert_eq!(folder.name, "我的收藏");
    assert_eq!(folder.children, vec!["favorite-text", "favorite-image"]);
    let favorite = store.get_favor("favorite-text").unwrap();
    assert_eq!(favorite.parent_id.as_deref(), Some("favorite-root"));
    assert_eq!(favorite.name, "收藏标题");
    assert_eq!(favorite.body_text.as_deref(), Some("收藏正文"));
    assert_eq!(favorite.icon, "fa:star");
    assert_eq!(favorite.create_time, timestamp(CREATED));
    assert_eq!(favorite.update_time, timestamp(UPDATED));
    assert_eq!(favorite.sub_index, 3);

    let image = store.get_clip("clip-image").unwrap();
    let copied_path = Path::new(image.image_path.as_deref().unwrap());
    assert!(fs::canonicalize(copied_path).unwrap().starts_with(fs::canonicalize(&fixture.destination).unwrap()));
    assert_ne!(copied_path, fixture.image);
    assert_eq!(fs::read(copied_path).unwrap(), fs::read(&fixture.image).unwrap());
    assert_eq!(image.perceptual_hash, "0101010101010101");
    assert_eq!(store.get_favor("favorite-image").unwrap().image_path, image.image_path);
    drop(store);
    assert_eq!(snapshot(&fixture.source), before, "source data must remain byte-for-byte unchanged");
}

#[test]
fn dry_run_validates_and_reports_without_creating_destination_or_changing_source() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.source);
    let destination = fixture.destination.join("not-created/nested");
    let report = import_csharp_data(&fixture.source, &destination, true).unwrap();
    let report = report_json(&report);
    assert_eq!(report["clips"], 10);
    assert_eq!(report["favorites"], 3);
    assert_eq!(report["images"], 1);
    assert!(!fixture.destination.exists());
    assert_eq!(snapshot(&fixture.source), before);
}

#[test]
fn modern_omitted_defaults_and_unused_dotnet_minimum_usage_time_are_preserved() {
    let fixture = Fixture::new();
    let connection = Connection::open(fixture.source.join("userdata.db")).unwrap();
    let raw: String = connection.query_row("SELECT Data FROM clip_item WHERE Id='clip-text'", [], |r| r.get(0)).unwrap();
    let mut payload: Value = serde_json::from_str(&raw).unwrap();
    let object = payload.as_object_mut().unwrap();
    object.remove("IsTop");
    object.remove("SubIndex");
    connection.execute("UPDATE clip_item SET Data=?1 WHERE Id='clip-text'", [payload.to_string()]).unwrap();
    // A modern empty favorite folder may omit the default Children collection.
    // It remains a folder; omission must not turn it into a text favorite.
    insert(&connection, "favor_item", "empty-favorite", record("empty-favorite", json!({"ObjType":100})));
    let unused = json!({"Id":"clip_item_clip-text","LastUseTime":"0001-01-01T00:00:00","UsageCount":0});
    connection.execute("UPDATE UsageInfo SET Data=?1 WHERE Id='clip_item_clip-text'", [unused.to_string()]).unwrap();
    drop(connection);
    let before = snapshot(&fixture.source);

    let report = import_csharp_data(&fixture.source, &fixture.destination, false).unwrap();
    assert_eq!(report_json(&report)["favorites"], 4);
    let store = AppStore::open(&fixture.destination.join("userdata.db")).unwrap();
    let clip = store.get_clip("clip-text").unwrap();
    assert!(!clip.is_top);
    assert_eq!(clip.sub_index, 0);
    assert_eq!(clip.usage_count, 0);
    assert_eq!(clip.last_access, None);
    let usage = store.get_usage("clip_item_clip-text").unwrap();
    assert_eq!(usage.usage_count, 0);
    assert_eq!(usage.last_use_time, None, "DateTime.MinValue represents no previous use");
    let folder = store.get_favor("empty-favorite").unwrap();
    assert!(folder.is_folder);
    assert_eq!(folder.kind, ClipKind::FavoriteClipItem);
    assert!(folder.children.is_empty());
    drop(store);
    assert_eq!(snapshot(&fixture.source), before);
}

#[test]
fn an_existing_empty_destination_is_allowed_but_existing_data_is_never_overwritten() {
    let fixture = Fixture::new();
    fs::create_dir(&fixture.destination).unwrap();
    import_csharp_data(&fixture.source, &fixture.destination, false).unwrap();
    let before = snapshot(&fixture.destination);
    assert!(import_csharp_data(&fixture.source, &fixture.destination, false).is_err());
    assert_eq!(snapshot(&fixture.destination), before);

    let other = fixture._temp.path().join("nonempty");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("sentinel.txt"), b"keep existing data").unwrap();
    let before = snapshot(&other);
    assert!(import_csharp_data(&fixture.source, &other, true).is_err());
    assert!(import_csharp_data(&fixture.source, &other, false).is_err());
    assert_eq!(snapshot(&other), before);
}

#[test]
fn source_and_destination_overlap_is_rejected_in_both_directions() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.source);
    let nested = fixture.source.join("nested-import");
    let destinations: [&Path; 3] = [fixture.source.as_path(), nested.as_path(), fixture.source.parent().unwrap()];
    for destination in destinations {
        for dry_run in [true, false] {
            assert!(import_csharp_data(&fixture.source, destination, dry_run).is_err());
        }
    }
    assert_eq!(snapshot(&fixture.source), before);
    assert!(!fixture.source.join("nested-import").exists());
}

#[test]
fn invalid_payloads_or_missing_images_fail_without_a_partial_destination() {
    for failure in ["bad-json", "unknown-type", "missing-image", "mismatched-id"] {
        let fixture = Fixture::new();
        match failure {
            "bad-json" => fixture.modify("UPDATE clip_item SET Data='not JSON' WHERE Id='clip-text'"),
            "unknown-type" => fixture.modify("UPDATE clip_item SET Data=json_set(Data,'$.Item.ObjType',999) WHERE Id='clip-text'"),
            "missing-image" => fs::remove_file(&fixture.image).unwrap(),
            "mismatched-id" => fixture.modify("UPDATE clip_item SET Data=json_set(Data,'$.Id','other-id') WHERE Id='clip-text'"),
            _ => unreachable!(),
        }
        let before = snapshot(&fixture.source);
        for dry_run in [true, false] {
            let result = import_csharp_data(&fixture.source, &fixture.destination, dry_run);
            assert!(result.is_err(), "{failure}, dry_run={dry_run} must fail");
            assert!(!fixture.destination.exists(), "{failure} left a visible partial import");
            assert_eq!(snapshot(&fixture.source), before, "{failure} changed source files");
            let siblings: Vec<_> = fs::read_dir(fixture._temp.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
            assert_eq!(siblings, vec![std::ffi::OsString::from("csharp-source")], "{failure} leaked a staging directory");
        }
    }
}

#[test]
fn invalid_favorite_relationships_fail_without_dropping_or_reparenting_items() {
    for children in [json!(["favorite-text", "missing-child"]), json!(["favorite-root"])] {
        let fixture = Fixture::new();
        let connection = Connection::open(fixture.source.join("userdata.db")).unwrap();
        let raw: String = connection.query_row("SELECT Data FROM favor_item WHERE Id='favorite-root'", [], |r| r.get(0)).unwrap();
        let mut payload: Value = serde_json::from_str(&raw).unwrap();
        payload["Children"] = children;
        connection.execute("UPDATE favor_item SET Data=?1 WHERE Id='favorite-root'", [payload.to_string()]).unwrap();
        drop(connection);
        let before = snapshot(&fixture.source);
        assert!(import_csharp_data(&fixture.source, &fixture.destination, false).is_err());
        assert!(!fixture.destination.exists());
        assert_eq!(snapshot(&fixture.source), before);
    }
}

#[test]
fn read_only_snapshot_includes_committed_rows_in_the_source_wal() {
    let fixture = Fixture::new();
    let connection = Connection::open(fixture.source.join("userdata.db")).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").unwrap();
    insert(&connection, "clip_item", "wal-text", record("wal-text", json!({"ObjType":1,"Text":"committed in WAL"})));
    let database_before = fs::read(fixture.source.join("userdata.db")).unwrap();
    let wal_before = fs::read(fixture.source.join("userdata.db-wal")).unwrap();
    let report = import_csharp_data(&fixture.source, &fixture.destination, false).unwrap();
    assert_eq!(report_json(&report)["clips"], 11);
    let store = AppStore::open(&fixture.destination.join("userdata.db")).unwrap();
    assert_eq!(store.get_clip("wal-text").unwrap().body_text.as_deref(), Some("committed in WAL"));
    assert_eq!(fs::read(fixture.source.join("userdata.db")).unwrap(), database_before);
    assert_eq!(fs::read(fixture.source.join("userdata.db-wal")).unwrap(), wal_before);
    drop(store);
    drop(connection);
}

#[test]
fn embedded_html_and_qq_images_are_shared_and_readable_after_isolated_import() {
    use cliphost_core::im_rich::{parse_segments, ImRichOrigin, ImRichSegmentKind};

    let mut fixture = Fixture::new();
    fixture.destination = fixture._temp.path().join("Rust 导入 & images");
    fixture.modify("DELETE FROM clip_item; DELETE FROM favor_item; DELETE FROM UsageInfo;");
    let image_bytes = fs::read(&fixture.image).unwrap();
    let source_uri = url::Url::from_file_path(&fixture.image).unwrap().to_string();
    let html = format!(r#"<p>嵌入图片<img src="{source_uri}"></p>"#);
    let escaped_path = fixture.image.to_string_lossy().replace('&', "&amp;").replace('"', "&quot;");
    let qq = format!(r#"<QQRichEditFormat><EditElement type="0" text="QQ图片"/><EditElement type="3" filepath="{escaped_path}"/></QQRichEditFormat>"#);
    let connection = Connection::open(fixture.source.join("userdata.db")).unwrap();
    for (table, prefix) in [("clip_item", "clip"), ("favor_item", "favor")] {
        for (suffix, item) in [
            ("html", json!({"ObjType":10,"Text":"嵌入图片","HtmlData":html,"HtmlSourceUrl":"https://origin.test/page"})),
            ("qq", json!({"ObjType":11,"Text":"QQ图片","HtmlData":html,"QQRichText":qq})),
            ("image", json!({"ObjType":3,"ImagePath":"Temp/shared.png"})),
        ] {
            let id = format!("{prefix}-{suffix}");
            insert(&connection, table, &id, record(&id, item));
        }
    }
    let mut folder = record("favor-root", json!({"ObjType":100}));
    folder["Children"] = json!(["favor-html", "favor-qq", "favor-image"]);
    insert(&connection, "favor_item", "favor-root", folder);
    drop(connection);
    let before = snapshot(&fixture.source);

    let report = import_csharp_data(&fixture.source, &fixture.destination, false).unwrap();
    let report = report_json(&report);
    assert_eq!(report["clips"], 3);
    assert_eq!(report["favorites"], 4);
    assert_eq!(report["images"], 1, "all embedded and ordinary references share one copied image");
    let store = AppStore::open(&fixture.destination.join("userdata.db")).unwrap();
    let copied = PathBuf::from(store.get_clip("clip-image").unwrap().image_path.unwrap());
    let copied_canonical = fs::canonicalize(&copied).unwrap();
    assert!(copied_canonical.starts_with(fs::canonicalize(&fixture.destination).unwrap()));
    assert_eq!(fs::read(&copied).unwrap(), image_bytes);

    let assert_html = |html: &str| {
        let reference = html.split("src=\"").nth(1).unwrap().split('"').next().unwrap()
            .replace("&amp;", "&");
        let uri = url::Url::parse(&reference).unwrap();
        assert_eq!(uri.scheme(), "file");
        let path = uri.to_file_path().unwrap();
        assert_eq!(fs::canonicalize(&path).unwrap(), copied_canonical);
        assert_eq!(fs::read(&path).unwrap(), image_bytes);
    };
    let assert_qq = |qq: &str| {
        assert!(qq.contains("&amp;"), "the destination ampersand must be XML escaped");
        let images: Vec<_> = parse_segments(qq, ImRichOrigin::Qq).into_iter()
            .filter(|segment| segment.kind == ImRichSegmentKind::Image).collect();
        assert_eq!(images.len(), 1);
        assert_eq!(fs::canonicalize(&images[0].path).unwrap(), copied_canonical);
        assert_eq!(fs::read(&images[0].path).unwrap(), image_bytes);
    };
    for id in ["clip-html", "clip-qq"] {
        let clip = store.get_clip(id).unwrap();
        assert_html(clip.html_data.as_deref().unwrap());
        if let Some(qq) = clip.qq_rich_text.as_deref() { assert_qq(qq); }
    }
    for id in ["favor-html", "favor-qq"] {
        let favor = store.get_favor(id).unwrap();
        assert_html(favor.html_data.as_deref().unwrap());
        if let Some(qq) = favor.qq_rich_text.as_deref() { assert_qq(qq); }
    }
    let source_path = fixture.source.to_string_lossy();
    let source_url = url::Url::from_directory_path(&fixture.source).unwrap().to_string();
    fn check_strings(value: &Value, source_path: &str, source_url: &str) {
        match value {
            Value::String(text) => {
                assert!(!text.contains(source_path), "active record retains source path: {text}");
                assert!(!text.contains(source_url), "active record retains source URL: {text}");
            }
            Value::Array(values) => for value in values { check_strings(value, source_path, source_url); },
            Value::Object(values) => for value in values.values() { check_strings(value, source_path, source_url); },
            _ => {}
        }
    }
    for clip in store.list_clips().unwrap() {
        check_strings(&serde_json::to_value(clip).unwrap(), &source_path, &source_url);
    }
    for favor in store.list_favors().unwrap() {
        check_strings(&serde_json::to_value(favor).unwrap(), &source_path, &source_url);
    }
    assert_eq!(snapshot(&fixture.source), before);
}

#[cfg(windows)]
#[test]
fn image_library_junction_outside_source_is_rejected_without_changing_external_files() {
    use std::os::windows::process::CommandExt;

    struct RemoveJunction(PathBuf);
    impl Drop for RemoveJunction {
        fn drop(&mut self) {
            // RemoveDirectoryW unlinks this directory junction itself, not its
            // target. Do so before TempDir's general tree cleanup on failures.
            let _ = fs::remove_dir(&self.0);
        }
    }
    let fixture = Fixture::new();
    let junction = fixture.source.join("Image");
    let external = fixture._temp.path().join("external-image-library");
    fs::rename(&junction, &external).unwrap();
    let output = std::process::Command::new("cmd.exe")
        .args(["/C", "mklink", "/J"]).arg(&junction).arg(&external)
        .creation_flags(0x08000000).output().unwrap(); // CREATE_NO_WINDOW
    let unlink = RemoveJunction(junction.clone());
    assert!(output.status.success(), "cannot create local temp junction: {}",
        String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::canonicalize(&junction).unwrap(), fs::canonicalize(&external).unwrap());
    let database_before = fs::read(fixture.source.join("userdata.db")).unwrap();
    let external_before = snapshot(&external);
    for dry_run in [true, false] {
        let error = import_csharp_data(&fixture.source, &fixture.destination, dry_run).unwrap_err();
        assert!(error.contains("来源 Image 库解析后不在来源目录中"), "{error}");
        assert!(!fixture.destination.exists());
        assert_eq!(fs::read(fixture.source.join("userdata.db")).unwrap(), database_before);
        assert_eq!(snapshot(&external), external_before);
    }
    drop(unlink);
    assert!(!junction.exists());
    assert_eq!(snapshot(&external), external_before);
}

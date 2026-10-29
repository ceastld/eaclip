//! Synthetic in-place compatibility tests. They only create temporary userdata
//! fixtures, never resolve installed paths or read the desktop clipboard.
use std::{collections::BTreeMap, fs, path::PathBuf};

use cliphost_core::store::clip_usage_key;
use cliphost_core::{
    csharp_import::import_csharp_data, AppStore, ClipKind, ClipRecord, FavorRecord, HostState,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

const CREATED: &str = "2026-01-02T11:04:05.1234567+08:00";
const UPDATED: &str = "2026-01-03T12:04:05+08:00";
const WRAPPER_CREATED: &str = "2026-01-02 11:04:05.9876543";
const WRAPPER_UPDATED: &str = "2026-01-03 12:04:05.9876543";

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    image: PathBuf,
}

impl Fixture {
    fn new(guarded: bool) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("original-csharp-root");
        let db = root.join("userdata.db");
        let image = root.join("Image/Temp/relative.png");
        fs::create_dir_all(image.parent().unwrap()).unwrap();
        image::RgbaImage::from_pixel(2, 2, image::Rgba([20, 40, 60, 255]))
            .save(&image)
            .unwrap();
        let connection = Connection::open(&db).unwrap();
        for table in ["clip_item", "favor_item"] {
            connection
                .execute_batch(&format!(
                    "CREATE TABLE {table}(Id NVARCHAR(255) NOT NULL PRIMARY KEY,
                 UpdateTime DATETIME NOT NULL,Data TEXT,ObjectType NVARCHAR(100),
                 CreatedTime DATETIME NOT NULL,RowToken TEXT,
                 UnknownNullableMetadata TEXT);
                 CREATE UNIQUE INDEX idx_{table}_row_token ON {table}(RowToken);"
                ))
                .unwrap();
        }
        connection
            .execute_batch(
                "CREATE TABLE UsageInfo(Id TEXT NOT NULL PRIMARY KEY,Data TEXT,ObjectType TEXT,
             CreatedTime DATETIME NOT NULL,UpdateTime DATETIME NOT NULL);",
            )
            .unwrap();
        insert(
            &connection,
            "clip_item",
            "old-text",
            document(
                "old-text",
                json!({"ObjType":1,"Text":"旧历史中文🦀","UnknownItem":17}),
            ),
            true,
        );
        insert(
            &connection,
            "clip_item",
            "old-image",
            document(
                "old-image",
                json!({"ObjType":3,"ImagePath":"Temp/relative.png","ImageHash":"fixture-hash","PerceptualHash":"1010"}),
            ),
            true,
        );
        let mut folder = document("folder", json!({"ObjType":100,"UnknownFolder":true}));
        folder["Title"] = json!("旧收藏夹");
        folder["Children"] = json!(["old-favorite"]);
        insert(&connection, "favor_item", "folder", folder, true);
        let mut favorite = document(
            "old-favorite",
            json!({"ObjType":1,"Text":"旧收藏正文","UnknownItem":"favorite"}),
        );
        favorite["Title"] = json!("旧收藏");
        favorite["ParentId"] = json!("folder");
        favorite["Children"] = json!([]);
        favorite["SourceClipId"] = json!("old-text");
        insert(&connection, "favor_item", "old-favorite", favorite, true);
        insert(
            &connection,
            "UsageInfo",
            "clip_item_old-text",
            json!({"Id":"clip_item_old-text","LastUseTime":UPDATED,"UsageCount":2,"UnknownUsage":{"retain":true}}),
            false,
        );
        insert(
            &connection,
            "UsageInfo",
            "favor_item_old-favorite",
            json!({"Id":"favor_item_old-favorite","LastUseTime":"0001-01-01T00:00:00","UsageCount":0}),
            false,
        );
        if guarded {
            install_guard(&connection);
        }
        drop(connection);
        Self {
            _temporary: temporary,
            root,
            db,
            image,
        }
    }

    fn connection(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }

    fn document(&self, table: &str, id: &str) -> Option<Value> {
        self.connection()
            .query_row(
                &format!("SELECT Data FROM {table} WHERE Id=?1"),
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .unwrap()
            .map(|text| serde_json::from_str(&text).unwrap())
    }

    fn row(&self, table: &str, id: &str) -> Vec<Option<String>> {
        let connection = self.connection();
        let columns = if table == "UsageInfo" {
            "Id,Data,ObjectType,CreatedTime,UpdateTime"
        } else {
            "Id,Data,ObjectType,CreatedTime,UpdateTime,RowToken,UnknownNullableMetadata"
        };
        connection
            .query_row(
                &format!("SELECT {columns} FROM {table} WHERE Id=?1"),
                [id],
                |row| {
                    (0..if table == "UsageInfo" { 5 } else { 7 })
                        .map(|column| row.get(column))
                        .collect()
                },
            )
            .unwrap()
    }

    fn rows(&self) -> BTreeMap<String, Vec<Option<String>>> {
        let mut result = BTreeMap::new();
        for table in ["clip_item", "favor_item", "UsageInfo"] {
            let connection = self.connection();
            let mut statement = connection
                .prepare(&format!("SELECT Id FROM {table} ORDER BY Id"))
                .unwrap();
            for id in statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
            {
                let id = id.unwrap();
                result.insert(format!("{table}/{id}"), self.row(table, &id));
            }
        }
        result
    }

    fn original_schema(&self) -> BTreeMap<String, String> {
        let connection = self.connection();
        let mut statement = connection.prepare(
            "SELECT name,sql FROM sqlite_master WHERE tbl_name IN ('clip_item','favor_item','UsageInfo','__clip_versioned_write_gate') AND sql IS NOT NULL ORDER BY name"
        ).unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<BTreeMap<_, _>, _>>()
            .unwrap()
    }

    fn assert_guard_closed(&self) {
        let open: i32 = self
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM __clip_versioned_write_gate WHERE Permit<>0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(open, 0);
    }

    fn assert_csharp_payloads_readable(&self) {
        let report = import_csharp_data(
            &self.root,
            &self.root.parent().unwrap().join("unused-dry-run-target"),
            true,
        )
        .unwrap();
        assert!(report.dry_run);
        assert!(!self
            .root
            .parent()
            .unwrap()
            .join("unused-dry-run-target")
            .exists());
    }
}

fn document(id: &str, item: Value) -> Value {
    json!({"Id":id,"Item":item,"CreateTime":CREATED,"UpdateTime":UPDATED,
        "Title":"原标题","Description":"原备注","Icon":"custom:record",
        "IsTop":false,"SubIndex":3,"SourceProcessName":"fixture.exe",
        "UnknownRoot":{"retain":[1,2,3]}})
}

fn insert(connection: &Connection, table: &str, id: &str, data: Value, token: bool) {
    if token {
        connection.execute(&format!(
            "INSERT INTO {table}(Id,Data,ObjectType,CreatedTime,UpdateTime,RowToken,UnknownNullableMetadata) VALUES(?1,?2,'original:type',?3,?4,?5,'retain-column')"),
            params![id,data.to_string(),WRAPPER_CREATED,WRAPPER_UPDATED,format!("token-{id}")]).unwrap();
    } else {
        connection.execute(&format!(
            "INSERT INTO {table}(Id,Data,ObjectType,CreatedTime,UpdateTime) VALUES(?1,?2,NULL,?3,?4)"),
            params![id,data.to_string(),WRAPPER_CREATED,WRAPPER_UPDATED]).unwrap();
    }
}

fn install_guard(connection: &Connection) {
    connection.execute_batch(
        "CREATE TABLE __clip_versioned_write_gate(TableName TEXT PRIMARY KEY NOT NULL, Permit INTEGER NOT NULL DEFAULT 0 CHECK(Permit IN(0,1)));
         INSERT INTO __clip_versioned_write_gate VALUES('clip_item',0),('favor_item',0);"
    ).unwrap();
    for table in ["clip_item", "favor_item"] {
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            connection.execute_batch(&format!(
                "CREATE TRIGGER __guard_{table}_{operation} BEFORE {operation} ON {table}
                 WHEN COALESCE((SELECT Permit FROM __clip_versioned_write_gate WHERE TableName='{table}'),0)<>1
                 BEGIN SELECT RAISE(ABORT,'versioned item writer required'); END;"
            )).unwrap();
        }
    }
}

#[test]
fn opening_in_place_reads_history_favorites_relative_images_without_rewriting_original_rows() {
    let fixture = Fixture::new(true);
    let rows = fixture.rows();
    let schema = fixture.original_schema();
    let image_bytes = fs::read(&fixture.image).unwrap();
    let store = AppStore::open(&fixture.db).unwrap();
    let text = store.get_clip("old-text").unwrap();
    assert_eq!(text.body_text.as_deref(), Some("旧历史中文🦀"));
    assert_eq!(text.usage_count, 2);
    assert!(store
        .get_usage("favor_item_old-favorite")
        .unwrap()
        .last_use_time
        .is_none());
    let image_record = store.get_clip("old-image").unwrap();
    assert_eq!(
        std::path::Path::new(image_record.image_path.as_deref().unwrap()),
        fixture.image
    );
    assert_eq!(
        store.get_favor("folder").unwrap().children,
        ["old-favorite"]
    );
    assert_eq!(
        store
            .get_favor("old-favorite")
            .unwrap()
            .parent_id
            .as_deref(),
        Some("folder")
    );
    assert_eq!(fixture.rows(), rows);
    assert_eq!(fixture.original_schema(), schema);
    assert_eq!(fs::read(&fixture.image).unwrap(), image_bytes);
    fixture.assert_guard_closed();
    drop(store);
    assert_eq!(fixture.rows(), rows);
}

#[test]
fn history_only_document_store_repairs_unused_favorites_and_preserves_original_rows_and_images() {
    let fixture = Fixture::new(false);
    fixture.connection().execute_batch("DROP TABLE favor_item;").unwrap();
    let text = fixture.row("clip_item", "old-text");
    let image = fixture.row("clip_item", "old-image");
    let bytes = fs::read(&fixture.image).unwrap();
    let schema = fixture.original_schema();
    let store = AppStore::open(&fixture.db).unwrap();
    assert!(store.is_document_store());
    assert_eq!(store.get_clip("old-text").unwrap().body_text.as_deref(), Some("旧历史中文🦀"));
    let favorite = FavorRecord::from_clip(&ClipRecord::new_text("首次收藏", "fixture.exe"), None);
    store.insert_favor_checked(&favorite).unwrap();
    drop(store);
    let reopened = AppStore::open(&fixture.db).unwrap();
    assert_eq!(reopened.get_favor(&favorite.id).unwrap().body_text.as_deref(), Some("首次收藏"));
    assert_eq!(fixture.row("clip_item", "old-text"), text);
    assert_eq!(fixture.row("clip_item", "old-image"), image);
    assert_eq!(fs::read(&fixture.image).unwrap(), bytes);
    for (name, sql) in schema { assert_eq!(fixture.original_schema().get(&name), Some(&sql)); }
    assert_eq!(fixture.document("favor_item", &favorite.id).unwrap()["Item"]["Text"], "首次收藏");
    fixture.assert_csharp_payloads_readable();
}

#[test]
fn favorites_only_document_store_repairs_unused_history_without_changing_favorite_tree() {
    let fixture = Fixture::new(false);
    fixture.connection().execute_batch("DROP TABLE clip_item;").unwrap();
    let folder = fixture.row("favor_item", "folder");
    let favorite = fixture.row("favor_item", "old-favorite");
    let schema = fixture.original_schema();
    let store = AppStore::open(&fixture.db).unwrap();
    assert!(store.is_document_store());
    assert_eq!(store.counts().unwrap(), (0, 0));
    assert_eq!(store.get_favor("folder").unwrap().children, ["old-favorite"]);
    let clip = ClipRecord::new_text("首次历史", "fixture.exe");
    store.insert_clip(&clip, true).unwrap();
    drop(store);
    let reopened = AppStore::open(&fixture.db).unwrap();
    assert_eq!(reopened.get_clip(&clip.id).unwrap().body_text.as_deref(), Some("首次历史"));
    assert_eq!(fixture.row("favor_item", "folder"), folder);
    assert_eq!(fixture.row("favor_item", "old-favorite"), favorite);
    for (name, sql) in schema { assert_eq!(fixture.original_schema().get(&name), Some(&sql)); }
    assert_eq!(fixture.document("clip_item", &clip.id).unwrap()["Item"]["Text"], "首次历史");
    fixture.assert_csharp_payloads_readable();
}

#[test]
fn missing_document_tables_can_be_completed_when_corresponding_old_tables_are_empty() {
    for (missing, old) in [("clip_item", "cLiPbOaRdItEm"), ("favor_item", "fAvOrItEm")] {
        let fixture = Fixture::new(false);
        fixture.connection().execute_batch(&format!(
            "DROP TABLE {missing}; CREATE TABLE {old}(Id INTEGER PRIMARY KEY, Item TEXT);"
        )).unwrap();
        let store = AppStore::open(&fixture.db).unwrap();
        assert!(store.is_document_store());
        let old_rows: i64 = fixture.connection().query_row(&format!("SELECT count(*) FROM {old}"), [], |row| row.get(0)).unwrap();
        assert_eq!(old_rows, 0);
        let columns: Vec<String> = fixture.connection().prepare(&format!("SELECT name FROM pragma_table_info('{missing}')"))
            .unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert!(columns.iter().any(|name| name == "Data"));
        assert!(!columns.iter().any(|name| name == "kind"));
    }
}

#[test]
fn unconverted_old_rows_prevent_empty_replacement_for_single_table_and_old_only_databases() {
    for (missing, old) in [("clip_item", "ClipboardItem"), ("favor_item", "FavorItem")] {
        for both_missing in [false, true] {
            let fixture = Fixture::new(false);
            let database = fixture.connection();
            if both_missing { database.execute_batch("DROP TABLE clip_item; DROP TABLE favor_item;").unwrap(); }
            else { database.execute_batch(&format!("DROP TABLE {missing};")).unwrap(); }
            database.execute_batch(&format!(
                "CREATE TABLE {old}(Id INTEGER PRIMARY KEY, Item TEXT);
                 INSERT INTO {old} VALUES(731, 'private old clipboard payload');"
            )).unwrap();
            drop(database);
            let original = fs::read(&fixture.db).unwrap();
            let error = match AppStore::open(&fixture.db) { Ok(_) => panic!("Must not hide unconverted old rows"), Err(error) => error.to_string() };
            assert!(error.contains(old) && error.contains(missing));
            assert!(!error.contains("731") && !error.contains("private old clipboard payload"));
            assert_eq!(fs::read(&fixture.db).unwrap(), original);
        }
    }
}

#[test]
fn completed_document_and_native_stores_can_keep_obsolete_old_tables_without_reimporting() {
    let fixture = Fixture::new(false);
    let rows = fixture.rows();
    fixture.connection().execute_batch(
        "CREATE TABLE ClipboardItem(Id INTEGER PRIMARY KEY, Item TEXT); INSERT INTO ClipboardItem VALUES(1,'old');
         CREATE TABLE FavorItem(Id INTEGER PRIMARY KEY, Item TEXT); INSERT INTO FavorItem VALUES(2,'old');"
    ).unwrap();
    let store = AppStore::open(&fixture.db).unwrap();
    assert_eq!(store.counts().unwrap(), (2, 0));
    assert_eq!(fixture.rows(), rows);
    drop(store);
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("native.db");
    drop(AppStore::open(&db).unwrap());
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TABLE ClipboardItem(Id INTEGER PRIMARY KEY, Item TEXT); INSERT INTO ClipboardItem VALUES(1,'old');
         CREATE TABLE FavorItem(Id INTEGER PRIMARY KEY, Item TEXT); INSERT INTO FavorItem VALUES(2,'old');"
    ).unwrap();
    let store = AppStore::open(&db).unwrap();
    assert!(!store.is_document_store());
    assert_eq!(store.counts().unwrap(), (0, 0));
}

#[test]
fn unsupported_mixed_or_required_metadata_schema_is_rejected_before_missing_table_repair() {
    for schema in [
        "CREATE TABLE clip_item(Id TEXT PRIMARY KEY, Data TEXT); CREATE TABLE favor_item(id TEXT PRIMARY KEY, kind TEXT);",
        "CREATE TABLE favor_item(Id TEXT PRIMARY KEY, Data TEXT); CREATE TABLE clip_item(id TEXT PRIMARY KEY, kind TEXT);",
        "CREATE TABLE clip_item(Id TEXT PRIMARY KEY, Data TEXT, kind TEXT);",
        "CREATE TABLE clip_item(Id TEXT PRIMARY KEY, Data TEXT, RequiredExtension TEXT NOT NULL);",
        "CREATE TABLE clip_item(Id TEXT PRIMARY KEY, Data TEXT); CREATE TABLE UsageInfo(Id TEXT, LegacyPayload TEXT);",
        "CREATE VIEW clip_item AS SELECT 'id' AS Id, '{}' AS Data;",
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let db = temporary.path().join("userdata.db");
        Connection::open(&db).unwrap().execute_batch(schema).unwrap();
        let original = fs::read(&db).unwrap();
        assert!(AppStore::open(&db).is_err(), "{schema}");
        assert_eq!(fs::read(&db).unwrap(), original, "{schema}");
    }
}

#[test]
fn document_tables_without_an_unconditional_single_id_key_are_rejected_without_changes() {
    for table in ["clip_item", "favor_item", "UsageInfo"] {
        for declaration in [
            "CREATE TABLE {table}(Id TEXT, Data TEXT);",
            "CREATE TABLE {table}(Id TEXT, Data TEXT, PRIMARY KEY(Id,Data));",
            "CREATE TABLE {table}(Id TEXT, Data TEXT, UNIQUE(Id,Data));",
            "CREATE TABLE {table}(Id TEXT, Data TEXT); CREATE UNIQUE INDEX fixture_partial_id ON {table}(Id) WHERE Id IS NOT NULL;",
            "CREATE TABLE {table}(Id TEXT, Data TEXT); CREATE UNIQUE INDEX fixture_expression_id ON {table}(lower(Id));",
        ] {
            // Item-table cases leave the other table absent, so a rejection
            // must precede repair. UsageInfo is checked with both items present.
            let temporary = tempfile::tempdir().unwrap();
            let db = temporary.path().join("userdata.db");
            let connection = Connection::open(&db).unwrap();
            if table == "UsageInfo" {
                connection.execute_batch(
                    "CREATE TABLE clip_item(Id TEXT PRIMARY KEY, Data TEXT);
                     CREATE TABLE favor_item(Id TEXT PRIMARY KEY, Data TEXT);"
                ).unwrap();
            }
            connection.execute_batch(&declaration.replace("{table}", table)).unwrap();
            drop(connection);
            let original = fs::read(&db).unwrap();
            let error = match AppStore::open(&db) { Ok(_) => panic!("Invalid {table} key must be rejected"), Err(error) => error.to_string() };
            assert!(error.contains(table) && error.contains("Id 缺少单列主键或非部分唯一索引"));
            assert_eq!(fs::read(&db).unwrap(), original, "{table}: {declaration}");
        }
    }
}

#[test]
fn document_tables_with_a_separate_single_id_unique_index_remain_readable_and_writable() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("userdata.db");
    Connection::open(&db).unwrap().execute_batch(
        "CREATE TABLE clip_item(Id TEXT, Data TEXT); CREATE UNIQUE INDEX fixture_clip_id ON clip_item(Id);
         CREATE TABLE favor_item(Id TEXT, Data TEXT); CREATE UNIQUE INDEX fixture_favor_id ON favor_item(Id);
         CREATE TABLE UsageInfo(Id TEXT, Data TEXT); CREATE UNIQUE INDEX fixture_usage_id ON UsageInfo(Id);"
    ).unwrap();
    let store = AppStore::open(&db).unwrap();
    let clip = ClipRecord::new_text("unique index history", "fixture.exe");
    store.insert_clip(&clip, true).unwrap();
    store.update_usage(&clip_usage_key(&clip.id)).unwrap();
    let favorite = FavorRecord::from_clip(&clip, None);
    store.insert_favor_checked(&favorite).unwrap();
    drop(store);
    let reopened = AppStore::open(&db).unwrap();
    assert_eq!(reopened.get_clip(&clip.id).unwrap().body_text, clip.body_text);
    assert_eq!(reopened.get_clip(&clip.id).unwrap().usage_count, 1);
    assert_eq!(reopened.get_favor(&favorite.id).unwrap().body_text, favorite.body_text);
}

#[test]
fn in_place_insert_edit_delete_and_usage_survive_reopen_in_csharp_document_format() {
    let fixture = Fixture::new(false);
    let schema = fixture.original_schema();
    let old_row = fixture.row("clip_item", "old-text");
    let store = AppStore::open(&fixture.db).unwrap();
    let mut text = store.get_clip("old-text").unwrap();
    text.body_text = Some("新正文 ; clip_item ' usage_info \" 中文🦀".into());
    text.title = Some("新标题".into());
    text.is_top = true;
    store.save_clip(&text).unwrap();
    let written = fixture.document("clip_item", "old-text").unwrap();
    assert_eq!(written["Item"]["Text"], text.body_text.clone().unwrap());
    assert_eq!(written["UnknownRoot"], json!({"retain":[1,2,3]}));
    assert_eq!(written["Item"]["UnknownItem"], 17);
    assert_eq!(written["Icon"], "custom:record");
    assert_eq!(written["CreateTime"], CREATED);
    let updated_row = fixture.row("clip_item", "old-text");
    assert_eq!(updated_row[2], old_row[2]);
    assert_eq!(updated_row[3], old_row[3]);
    assert_eq!(updated_row[6], old_row[6]);
    assert_ne!(updated_row[5], old_row[5]);
    let mut image = store.get_clip("old-image").unwrap();
    image.is_top = true;
    store.save_clip(&image).unwrap();
    assert_eq!(
        fixture.document("clip_item", "old-image").unwrap()["Item"]["ImagePath"],
        "Temp/relative.png"
    );
    let added = ClipRecord::new_text("新增正文", "new.exe");
    store.insert_clip(&added, true).unwrap();
    let mut favorite = store.get_favor("old-favorite").unwrap();
    favorite.name = "新收藏名".into();
    favorite.body_text = Some("新收藏正文".into());
    store.save_favor(&favorite).unwrap();
    let mut new_favorite = FavorRecord::from_clip(&added, Some("folder".into()));
    new_favorite.name = "新增收藏".into();
    store.insert_favor_checked(&new_favorite).unwrap();
    assert_eq!(
        store.get_favor("folder").unwrap().children,
        ["old-favorite", new_favorite.id.as_str()]
    );
    let usage = store.update_usage(&clip_usage_key("old-text")).unwrap();
    assert_eq!(usage.usage_count, 3);
    assert_eq!(
        fixture.document("UsageInfo", "clip_item_old-text").unwrap()["UnknownUsage"],
        json!({"retain":true})
    );
    store.delete_favor("old-favorite").unwrap();
    assert_eq!(
        store.get_favor("folder").unwrap().children,
        [new_favorite.id.as_str()]
    );
    store.delete_clip(&added.id).unwrap();
    assert!(fixture.document("clip_item", &added.id).is_none());
    assert!(fixture.document("favor_item", "old-favorite").is_none());
    drop(store);
    let reopened = AppStore::open(&fixture.db).unwrap();
    assert_eq!(
        reopened.get_clip("old-text").unwrap().body_text,
        text.body_text
    );
    assert_eq!(reopened.get_clip("old-text").unwrap().usage_count, 3);
    assert_eq!(
        reopened.get_favor(&new_favorite.id).unwrap().name,
        "新增收藏"
    );
    assert!(reopened.get_clip(&added.id).is_err());
    assert_eq!(fixture.original_schema(), schema);
    fixture.assert_csharp_payloads_readable();
}

#[test]
fn guarded_writes_change_tokens_and_failed_batches_roll_back_main_cache_and_gate() {
    let fixture = Fixture::new(true);
    let store = AppStore::open(&fixture.db).unwrap();
    let old_token = fixture.row("clip_item", "old-text")[5].clone();
    let mut old = store.get_clip("old-text").unwrap();
    old.title = Some("合法版本写".into());
    store.save_clip(&old).unwrap();
    assert_ne!(fixture.row("clip_item", "old-text")[5], old_token);
    fixture.assert_guard_closed();
    assert!(fixture
        .connection()
        .execute("UPDATE clip_item SET Data=Data WHERE Id='old-text'", [])
        .is_err());
    let rows = fixture.rows();
    let first = ClipRecord::new_text("必须回滚", "fixture.exe");
    let mut invalid = ClipRecord::new_text("", "fixture.exe");
    invalid.kind = ClipKind::ImageClipItem;
    invalid.image_path = None;
    assert!(store.insert_clips(&[first.clone(), invalid]).is_err());
    assert_eq!(fixture.rows(), rows);
    assert!(store.get_clip(&first.id).is_err());
    fixture.assert_guard_closed();
    drop(store);
    let reopened = AppStore::open(&fixture.db).unwrap();
    assert!(reopened.get_clip(&first.id).is_err());
    assert_eq!(reopened.get_clip("old-text").unwrap().title, old.title);
}

#[test]
fn external_commits_refresh_reads_and_stale_objects_remain_rejected_after_other_reads() {
    let fixture = Fixture::new(false);
    let store = AppStore::open(&fixture.db).unwrap();
    let mut stale = store.get_clip("old-text").unwrap();
    stale.title = Some("不能覆盖外部更新".into());
    let mut external = fixture.document("clip_item", "old-text").unwrap();
    external["Item"]["Text"] = json!("其他进程提交的新正文");
    external["UpdateTime"] = json!("2026-01-04T12:04:05Z");
    fixture.connection().execute(
        "UPDATE clip_item SET Data=?1,RowToken='external-new-token',UpdateTime='2026-01-04 12:04:05' WHERE Id='old-text'",
        [external.to_string()]
    ).unwrap();
    assert!(store.save_clip(&stale).is_err());
    assert_eq!(fixture.document("clip_item", "old-text").unwrap(), external);
    let refreshed = store.get_clip("old-text").unwrap();
    assert_eq!(refreshed.body_text.as_deref(), Some("其他进程提交的新正文"));
    // A fresh read elsewhere must not authorize an old editor's stale object.
    assert!(store.save_clip(&stale).is_err());
    assert_eq!(fixture.document("clip_item", "old-text").unwrap(), external);
    let mut current = refreshed;
    current.title = Some("在最新正文上编辑标题".into());
    store.save_clip(&current).unwrap();
    assert_eq!(
        fixture.document("clip_item", "old-text").unwrap()["Item"]["Text"],
        "其他进程提交的新正文"
    );
    assert_eq!(
        fixture.document("clip_item", "old-text").unwrap()["Title"],
        "在最新正文上编辑标题"
    );
}

#[test]
fn failing_favorite_parent_update_leaves_neither_child_nor_gate_permission() {
    let fixture = Fixture::new(true);
    let store = AppStore::open(&fixture.db).unwrap();
    fixture
        .connection()
        .execute_batch(
            "CREATE TRIGGER reject_fixture_parent BEFORE UPDATE ON favor_item WHEN OLD.Id='folder'
         BEGIN SELECT RAISE(ABORT,'fixture parent save failure'); END;",
        )
        .unwrap();
    // Reload after the external DDL, before starting the checked batch.
    let folder = store.get_favor("folder").unwrap();
    let new_clip = ClipRecord::new_text("新收藏", "fixture.exe");
    let child = FavorRecord::from_clip(&new_clip, Some(folder.id));
    let rows = fixture.rows();
    assert!(store.insert_favor_checked(&child).is_err());
    assert_eq!(fixture.rows(), rows);
    assert!(fixture.document("favor_item", &child.id).is_none());
    assert!(store.get_favor(&child.id).is_err());
    assert_eq!(
        store.get_favor("folder").unwrap().children,
        ["old-favorite"]
    );
    fixture.assert_guard_closed();
}

#[test]
fn native_rust_database_keeps_its_existing_relational_format_and_data() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("native-rust/userdata.db");
    let store = AppStore::open(&db).unwrap();
    let mut record = ClipRecord::new_text("Rust 数据仍可读写", "fixture.exe");
    store.insert_clip(&record, true).unwrap();
    record.title = Some("原生格式".into());
    store.save_clip(&record).unwrap();
    store.update_usage(&clip_usage_key(&record.id)).unwrap();
    drop(store);
    let reopened = AppStore::open(&db).unwrap();
    assert_eq!(reopened.get_clip(&record.id).unwrap().title, record.title);
    assert_eq!(reopened.get_clip(&record.id).unwrap().usage_count, 1);
    let connection = Connection::open(&db).unwrap();
    let native: i32 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('clip_item') WHERE name='kind'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(native, 1);
    let legacy: i32 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('clip_item') WHERE name='Data'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(legacy, 0);
}

#[test]
fn actual_headless_host_opens_legacy_root_without_pruning_history_at_the_default_limit() {
    cliphost_core::clipboard::force_headless(true);
    let fixture = Fixture::new(false);
    let connection = fixture.connection();
    for index in 0..120 {
        let id = format!("existing-{index:03}");
        insert(
            &connection,
            "clip_item",
            &id,
            document(
                &id,
                json!({"ObjType":1,"Text":format!("保留旧历史 {index}")}),
            ),
            true,
        );
    }
    drop(connection);
    fs::create_dir_all(fixture.root.join("Configs")).unwrap();
    fs::write(
        fixture.root.join("Configs/ClipConfig.json"),
        b"\xef\xbb\xbf{\"MaxItemSize\":100,\"AllowDuplicateCopy\":true,\"FutureLegacyPreference\":{\"keep\":7}}",
    )
    .unwrap();
    let rows = fixture.rows();
    let schema = fixture.original_schema();
    let bytes = fs::read(&fixture.image).unwrap();
    let host = HostState::open(
        cliphost_core::paths::AppPaths::from_data_root(fixture.root.clone()),
        28965,
    )
    .unwrap();
    assert_eq!(host.data_paths()["storageFormat"], "csharp-documents");
    let config = host.config_get();
    assert_eq!(config["nonPinnedItemCount"], 122);
    assert!(config["maxItemSize"].as_i64().unwrap() >= 122);
    assert_eq!(
        host.detail(&json!({"id":"old-text"})).unwrap()["bodyText"],
        "旧历史中文🦀"
    );
    assert_eq!(
        host.favor_get(&json!({"id":"old-favorite"})).unwrap()["bodyText"],
        "旧收藏正文"
    );
    assert_eq!(
        host.favor_get(&json!({"id":"folder"})).unwrap()["children"],
        json!(["old-favorite"])
    );
    assert_eq!(host.image_bytes("old-image").unwrap().0, bytes);
    host.flush_for_shutdown();
    drop(host);
    let reopened = AppStore::open(&fixture.db).unwrap();
    assert_eq!(reopened.counts().unwrap(), (122, 0));
    assert_eq!(
        reopened.get_favor("folder").unwrap().children,
        ["old-favorite"]
    );
    assert_eq!(fixture.rows(), rows);
    assert_eq!(fixture.original_schema(), schema);
    assert_eq!(fs::read(&fixture.image).unwrap(), bytes);
}

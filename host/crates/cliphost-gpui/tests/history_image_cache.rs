//! Synthetic offline documents/native/CORE integration. No product window, listener, clipboard
//! write, capture, hooks or production root lookup. Run with --test-threads=1.
#![cfg(all(windows, feature = "test-support"))]

use std::{collections::BTreeMap, fs, path::{Path, PathBuf}};

use cliphost_core::{
    dto::{clip_to_dto, HighlightHitRangeDto}, model::{sha256_hex, FavorRecord},
    storage_core::migrate::{build_candidate, MigrationOptions},
    AppPaths, AppStore, ClipKind, ClipRecord, HostState,
};
use cliphost_gpui::{
    editor::{build_patch, DraftSnapshot, EditorTarget},
    history_image_resources::{load_primary_image, load_primary_image_resources, primary_image_projection, probe_source, PRIMARY_IMAGE_RESOURCE},
    hover::{row_hover_content, HoverThumbnailTarget, ImageSource, RowHoverInput, RowHoverSource},
};
use image::AnimationDecoder;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::json;

// The authored encoder is shared with the optional native fixture generator;
// its main is never invoked by this integration test.
#[allow(dead_code)]
#[path = "../examples/history_image_cache_fixture.rs"]
mod authored;

const PNG_ID: &str = "owned-cache-png";
const GIF_ID: &str = "owned-cache-gif";
const WEBP_ID: &str = "owned-cache-webp";
const STATIC_WEBP_ID: &str = "owned-cache-static-webp";
const FAVORITE_ID: &str = "owned-cache-favorite";

struct Fixture {
    // Declared after Host so the database is closed before TempDir cleanup.
    host: HostState,
    _directory: tempfile::TempDir,
    source: PathBuf,
    runtime: PathBuf,
    originals: BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    // A new independent connection per assertion observes the committed
    // candidate transaction. Test reads never borrow HostState internals.
    fn clip(&self, id: &str) -> ClipRecord {
        AppStore::open_isolated_core(&self.runtime.join("userdata.db"))
            .unwrap().get_clip(id).unwrap()
    }

    fn favorite(&self, id: &str) -> FavorRecord {
        AppStore::open_isolated_core(&self.runtime.join("userdata.db"))
            .unwrap().get_favor(id).unwrap()
    }
}

fn hashes(root: &Path) -> BTreeMap<PathBuf, String> {
    fn walk(base: &Path, at: &Path, out: &mut BTreeMap<PathBuf, String>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                walk(base, &entry.path(), out);
            } else {
                out.insert(entry.path().strip_prefix(base).unwrap().to_path_buf(), sha256_hex(&fs::read(entry.path()).unwrap()));
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}

// Only the GPUI-owned derived cache may be added during display loading. All
// original image files and other pre-existing resources remain byte-identical.
fn original_image_hashes(root: &Path) -> BTreeMap<PathBuf, String> {
    hashes(root).into_iter()
        .filter(|(path, _)| !path.starts_with("Thumbnails/history-v1"))
        .collect()
}

fn thumbnail_cache_root(fixture: &Fixture) -> PathBuf {
    fixture.runtime.join("Image/Thumbnails/history-v1")
}

fn fixture() -> Fixture {
    cliphost_core::clipboard::force_headless(true);
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap();
    let parent = repository.join(".tmp/history-image-cache-tests");
    fs::create_dir_all(&parent).unwrap();
    let directory = tempfile::tempdir_in(parent).unwrap();
    let source = directory.path().join("source");
    let runtime = directory.path().join("candidate");
    fs::create_dir_all(source.join("Image/Temp")).unwrap();
    fs::create_dir_all(source.join("Configs")).unwrap();
    fs::write(source.join("Configs/ClipConfig.json"), br#"{"MaxItemSize":10000}"#).unwrap();
    fs::write(source.join("Configs/FavorConfig.json"), serde_json::to_vec(&json!({"RootChildren":[FAVORITE_ID]})).unwrap()).unwrap();
    let originals = BTreeMap::from([
        (PNG_ID.to_string(), authored::png(3840, 2160, 31).unwrap()),
        (GIF_ID.to_string(), authored::gif(320, 180, 4).unwrap()),
        (WEBP_ID.to_string(), authored::animated_webp(320, 180).unwrap()),
        (STATIC_WEBP_ID.to_string(), authored::static_webp(3840, 2160).unwrap()),
    ]);
    {
        let store = AppStore::open(&source.join("userdata.db")).unwrap();
        for (id, extension) in [(PNG_ID, "png"), (GIF_ID, "gif"), (WEBP_ID, "webp"), (STATIC_WEBP_ID, "webp")] {
            let bytes = &originals[id];
            let path = source.join(format!("Image/Temp/{id}.{extension}"));
            fs::write(&path, bytes).unwrap();
            let mut record = ClipRecord::new_text("", "owned-history-image-cache-fixture");
            record.id = id.into();
            record.kind = ClipKind::ImageClipItem;
            record.body_text = None;
            record.title = Some(format!("original {id}"));
            record.image_path = Some(path.to_string_lossy().into_owned());
            record.content_hash = sha256_hex(bytes);
            store.insert_clip(&record, true).unwrap();
            if id == PNG_ID {
                let mut favorite = FavorRecord::from_clip(&record, None);
                favorite.id = FAVORITE_ID.into();
                store.insert_favor(&favorite).unwrap();
            }
        }
    }
    let before = hashes(&source);
    let report = build_candidate(&MigrationOptions { source_root: source.clone(), candidate_root: runtime.clone() }, &mut |_| Ok(())).unwrap();
    assert_eq!(report.history_count, 4);
    assert_eq!(report.favorite_count, 1);
    assert_eq!(report.source_format, "native");
    assert_eq!(before, hashes(&source), "migration must not change the authored source");
    let host = HostState::open_isolated_core(AppPaths::from_data_root(runtime.clone()), 29065).unwrap();
    Fixture { host, _directory: directory, source, runtime, originals }
}

fn assert_original(fixture: &Fixture, id: &str) {
    let (bytes, _) = fixture.host.image_bytes(id).unwrap();
    assert_eq!(bytes, fixture.originals[id], "history API must serve original bytes");
    let path = fixture.host.hover_image_file(id, false).unwrap();
    let actual = path.canonicalize().unwrap_or_else(|error| panic!("hover original path cannot canonicalize: path={path:?}, root={:?}, error={error}", fixture.runtime));
    let root = fixture.runtime.canonicalize().unwrap_or_else(|error| panic!("candidate root cannot canonicalize: path={path:?}, root={:?}, error={error}", fixture.runtime));
    assert!(actual.starts_with(&root), "hover original escaped selected candidate: path={path:?}, root={:?}, canonical_path={actual:?}, canonical_root={root:?}", fixture.runtime);
    let source_root = fixture.source.canonicalize().unwrap();
    assert!(!actual.starts_with(&source_root), "hover original resolved to source rather than selected candidate: path={path:?}, canonical_path={actual:?}, canonical_source={source_root:?}");
    assert_eq!(fs::read(path).unwrap(), fixture.originals[id], "hover must resolve the original file");
    let hover = row_hover_content(RowHoverSource::History, RowHoverInput { id, kind: "ImageClipItem", title: "", summary: "", display_text: "", file_paths: &[], im_rich_segments: &[] });
    assert_eq!(hover.thumbnail(HoverThumbnailTarget::ItemImage).unwrap().source, ImageSource::History(id.into()));
}

#[test]
fn native_v1_store_reader_and_display_projection_keep_original_record_and_file() {
    let fixture = fixture();
    // Ordinary legacy Host startup has unrelated external services. This test
    // intentionally verifies the actual native-v1 AppStore reader contract,
    // not a claim that a product Legacy Host or its OS clipboard was started.
    let original_files = hashes(&fixture.source.join("Image"));
    let store = AppStore::open(&fixture.source.join("userdata.db")).unwrap();
    let record = store.get_clip(PNG_ID).unwrap();
    let record_before = serde_json::to_value(&record).unwrap();
    let original_path = PathBuf::from(record.image_path.as_ref().unwrap());
    assert!(original_path.is_absolute() && original_path.starts_with(&fixture.source));
    let original_bytes = fs::read(&original_path).unwrap();
    assert_eq!(record.content_hash, sha256_hex(&original_bytes));
    let dto = clip_to_dto(&record, None);
    let dto_before = serde_json::to_value(&dto).unwrap();
    let version = probe_source(&original_path).unwrap().source_version;
    assert!(primary_image_projection(&dto, &version).is_some());
    let resources = cliphost_gpui::history_image_resources::load_path_resources(&original_path, &version).unwrap();
    let display = resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap();
    let decoded = image::load_from_memory(display.image.bytes()).unwrap();
    assert!(decoded.width() <= 1024 && decoded.height() <= 640);
    assert_ne!(display.image.bytes(), original_bytes.as_slice());
    assert_eq!(serde_json::to_value(&dto).unwrap(), dto_before);
    assert_eq!(serde_json::to_value(store.get_clip(PNG_ID).unwrap()).unwrap(), record_before);
    assert_eq!(fs::read(&original_path).unwrap(), original_bytes);
    assert_eq!(hashes(&fixture.source.join("Image")), original_files);
}

#[test]
fn csharp_documents_image_and_favorite_readers_keep_original_documents_and_bytes() {
    const HISTORY: &str = "owned-documents-history-image";
    const FAVORITE: &str = "owned-documents-favorite-image";
    const STAMP: &str = "2026-10-18T03:12:34.123456789Z";
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap();
    let parent = repository.join(".tmp/history-image-cache-tests");
    fs::create_dir_all(&parent).unwrap();
    let directory = tempfile::tempdir_in(parent).unwrap();
    let root = directory.path();
    fs::create_dir_all(root.join("Image/Temp")).unwrap();
    let image_path = root.join("Image/Temp/documents-large.png");
    let original = authored::png(3840, 2160, 153).unwrap();
    fs::write(&image_path, &original).unwrap();
    let original_sha = sha256_hex(&original);
    let database = root.join("userdata.db");
    let document = |id: &str, favorite: bool| {
        serde_json::to_string(&json!({
            "Id":id,"Title":if favorite {"收藏原图 中文 🙂"}else{"历史原图 中文 🙂"},
            "Icon":null,"Description":"authored documents image cache fixture",
            "SourceProcessName":"owned-documents-fixture.exe","CreateTime":STAMP,"UpdateTime":STAMP,
            "SubIndex":0,"IsTop":false,"ParentId":null,"Children":[],
            "SourceClipId":if favorite {Some(HISTORY)}else{None},
            "Item":{"ObjType":3,"RSource":{"IsInImageDir":true,"RPath":"Temp/documents-large.png"},
                "ImageHash":original_sha,"ImageMD5":"owned-original-md5","PerceptualHash":"010101",
                "FuturePayload":{"keep":"原始未知字段","exactNumber":"RAW_BIG_NUMBER","lexical":"RAW_LEXICAL_NUMBER"}},
            "FutureDocument":{"unchanged":true}
        })).unwrap().replace("\"RAW_BIG_NUMBER\"","1234567890123456789012345678901234567890")
            .replace("\"RAW_LEXICAL_NUMBER\"","1.23400e+30")
    };
    let history_document = document(HISTORY, false);
    let favorite_document = document(FAVORITE, true);
    {
        let sql = Connection::open(&database).unwrap();
        // Same accepted C# JsonItemWrapper columns as store_connection's
        // csharp_fixture and storage_core_host_tests::seed_documents.
        sql.execute_batch("CREATE TABLE clip_item (Id TEXT PRIMARY KEY,Data TEXT NOT NULL,ObjectType TEXT,CreatedTime TEXT,UpdateTime TEXT,RowToken TEXT);
            CREATE TABLE favor_item (Id TEXT PRIMARY KEY,Data TEXT NOT NULL,ObjectType TEXT,CreatedTime TEXT,UpdateTime TEXT,RowToken TEXT);
            CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY,Data TEXT NOT NULL,ObjectType TEXT,CreatedTime TEXT,UpdateTime TEXT,RowToken TEXT);").unwrap();
        for (table,id,data,token) in [("clip_item",HISTORY,&history_document,"owned-history-token"),("favor_item",FAVORITE,&favorite_document,"owned-favorite-token")] {
            sql.execute(&format!("INSERT INTO {table}(Id,Data,ObjectType,CreatedTime,UpdateTime,RowToken) VALUES(?1,?2,'authored-image-document',?3,?3,?4)"),params![id,data,STAMP,token]).unwrap();
        }
    }
    let raw_rows = || {
        let sql = Connection::open_with_flags(&database,OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).unwrap();
        [("clip_item",HISTORY),("favor_item",FAVORITE)].into_iter().map(|(table,id)| {
            sql.query_row(&format!("SELECT Id,Data,ObjectType,CreatedTime,UpdateTime,RowToken FROM {table} WHERE Id=?1"),[id],|row| {
                Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,String>(5)?))
            }).unwrap()
        }).collect::<Vec<_>>()
    };
    let rows_before = raw_rows();
    let image_tree = hashes(&root.join("Image"));
    let store = AppStore::open(&database).unwrap();
    assert!(store.is_document_store(), "fixture must exercise the real C# documents adapter");
    assert!(!store.is_core_store());
    assert_eq!(raw_rows(), rows_before, "AppStore initialization must preserve original document cells");
    // Initial auxiliary schema setup is allowed above. Every following
    // projection/read/load is required to leave the entire database unchanged.
    let database_before = fs::read(&database).unwrap();
    let history = store.get_clip(HISTORY).unwrap();
    let favorite = store.get_favor(FAVORITE).unwrap();
    assert_eq!(history.kind, ClipKind::ImageClipItem);
    assert_eq!(favorite.kind, ClipKind::ImageClipItem);
    assert_eq!(favorite.source_clip_id.as_deref(), Some(HISTORY));
    assert_eq!(history.content_hash, original_sha);
    let history_record = serde_json::to_value(&history).unwrap();
    let favorite_record = serde_json::to_value(&favorite).unwrap();
    let dto = clip_to_dto(&history, None);
    let favorite_dto = cliphost_core::dto::favor_to_dto(&favorite);
    let dto_before = serde_json::to_value(&dto).unwrap();
    let favorite_dto_before = serde_json::to_value(&favorite_dto).unwrap();
    let expected_path = image_path.canonicalize().unwrap();
    for path in [history.image_path.as_ref().unwrap(),favorite.image_path.as_ref().unwrap()] {
        let path = Path::new(path);
        assert!(path.is_absolute());
        assert_eq!(path.canonicalize().unwrap(), expected_path, "documents RSource must resolve to the original Image/Temp file");
        let version = probe_source(path).unwrap();
        assert_eq!((version.width,version.height),(3840,2160));
        assert!(primary_image_projection(&dto,&version.source_version).is_some());
        let resources = cliphost_gpui::history_image_resources::load_path_resources(path,&version.source_version).unwrap();
        let display = resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap();
        let raster = image::load_from_memory(display.image.bytes()).unwrap();
        assert!(raster.width() <= 1024 && raster.height() <= 640);
        assert_ne!(display.image.bytes(), original.as_slice());
        assert_eq!(fs::read(path).unwrap(), original);
    }
    assert_eq!(serde_json::to_value(&dto).unwrap(), dto_before);
    assert_eq!(serde_json::to_value(&favorite_dto).unwrap(), favorite_dto_before);
    assert_eq!(serde_json::to_value(store.get_clip(HISTORY).unwrap()).unwrap(), history_record);
    assert_eq!(serde_json::to_value(store.get_favor(FAVORITE).unwrap()).unwrap(), favorite_record);
    assert_eq!(raw_rows(), rows_before, "original Data including unknown fields/number spelling and every wrapper column must remain byte-identical");
    assert_eq!(fs::read(&database).unwrap(), database_before, "display preparation must not write the documents database");
    assert_eq!(sha256_hex(&fs::read(&image_path).unwrap()), original_sha);
    assert_eq!(hashes(&root.join("Image")), image_tree);
}

#[test]
fn thumbnail_display_does_not_replace_original_edit_or_favorite_payload() {
    let fixture = fixture();
    let source_before = hashes(&fixture.source);
    let images_before = original_image_hashes(&fixture.runtime.join("Image"));
    let original_record = fixture.clip(PNG_ID);
    let path = fixture.host.hover_image_file(PNG_ID, false).unwrap();
    let source = probe_source(&path).unwrap();
    assert_eq!((source.width, source.height), (3840, 2160));
    let dto = clip_to_dto(&original_record, None);
    let dto_before = serde_json::to_value(&dto).unwrap();
    let projection = primary_image_projection(&dto, &source.source_version).unwrap();
    let resources = load_primary_image_resources(&fixture.host, PNG_ID, &source.source_version);
    let image = resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap();
    assert_eq!((image.width, image.height), (3840.0, 2160.0), "natural dimensions remain separate from decoded display size");
    let thumbnail = image::load_from_memory(image.image.bytes()).unwrap();
    assert!(thumbnail.width() <= 1024 && thumbnail.height() <= 640);
    assert_ne!(image.image.bytes(), fixture.originals[PNG_ID]);
    assert!(resources.retained_bytes() < 8 * 1024 * 1024);
    assert_eq!(serde_json::to_value(&dto).unwrap(), dto_before, "projection/loading cannot mutate the action DTO");
    assert!(format!("{projection:?}").contains("@history-primary:"));
    assert_original(&fixture, PNG_ID);
    let static_path = fixture.host.hover_image_file(STATIC_WEBP_ID, false).unwrap();
    let static_version = probe_source(&static_path).unwrap().source_version;
    let static_resources = load_primary_image_resources(&fixture.host, STATIC_WEBP_ID, &static_version);
    let static_image = static_resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap();
    assert_eq!(static_image.image.format(), gpui::ImageFormat::Png);
    let static_thumbnail = image::load_from_memory(static_image.image.bytes()).unwrap();
    assert!(static_thumbnail.width() <= 1024 && static_thumbnail.height() <= 640);
    assert_original(&fixture, STATIC_WEBP_ID);

    let detail = fixture.host.detail(&json!({"id":PNG_ID})).unwrap();
    let mut draft = DraftSnapshot::from_detail(&detail, false);
    draft.title = "edited title 中文 🙂".into();
    let patch = build_patch(&EditorTarget::History(PNG_ID.into()), &detail, &draft).unwrap();
    assert!(patch.get("imagePath").is_none() && patch.get("bodyText").is_none());
    assert_eq!(patch["expectedRevision"], detail["revision"]);
    fixture.host.patch_item(&patch).unwrap();
    assert!(fixture.host.patch_item(&patch).is_err(), "stale editor revision must be rejected");
    let after = fixture.clip(PNG_ID);
    assert_eq!(after.image_path, original_record.image_path);
    assert_eq!(after.content_hash, original_record.content_hash);
    assert_eq!(after.kind, original_record.kind);
    assert_original(&fixture, PNG_ID);

    let favor_before = fixture.favorite(FAVORITE_ID);
    let favorite_detail = fixture.host.favor_get(&json!({"id":FAVORITE_ID})).unwrap();
    let mut draft = DraftSnapshot::from_detail(&favorite_detail, true);
    draft.title = "favorite edited title".into();
    let patch = build_patch(&EditorTarget::Favorite(FAVORITE_ID.into()), &favorite_detail, &draft).unwrap();
    assert!(patch.get("imagePath").is_none());
    fixture.host.favor_update(&patch).unwrap();
    let favorite = fixture.favorite(FAVORITE_ID);
    assert_eq!(favorite.image_path, favor_before.image_path);
    assert_eq!(favorite.source_clip_id, favor_before.source_clip_id);
    assert_eq!(fixture.host.favor_image_bytes(FAVORITE_ID).unwrap().0, fixture.originals[PNG_ID]);
    assert_eq!(fs::read(fixture.host.hover_image_file(FAVORITE_ID, true).unwrap()).unwrap(), fixture.originals[PNG_ID]);
    let hover = row_hover_content(RowHoverSource::Favorite, RowHoverInput { id:FAVORITE_ID, kind:"ImageClipItem", title:"", summary:"", display_text:"", file_paths:&[], im_rich_segments:&[] });
    assert_eq!(hover.thumbnail(HoverThumbnailTarget::ItemImage).unwrap().source, ImageSource::Favorite(FAVORITE_ID.into()));
    assert_eq!(original_image_hashes(&fixture.runtime.join("Image")), images_before, "thumbnail display and title edits cannot rewrite original resources");
    assert_eq!(hashes(&fixture.source), source_before, "all changes stay in the isolated candidate");
}

#[test]
fn animation_bytes_survive_display_loading_and_repeat_page_visits() {
    let fixture = fixture();
    let before = hashes(&fixture.runtime.join("Image"));
    for id in [GIF_ID, WEBP_ID] {
        let path = fixture.host.hover_image_file(id, false).unwrap();
        let version = probe_source(&path).unwrap().source_version;
        // Repeated worker loads are separate from App/GPU retirement, which the
        // native paint example verifies after actual successful paint.
        for _ in 0..8 {
            let resources = load_primary_image_resources(&fixture.host, id, &version);
            let image = resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap();
            assert_eq!(image.image.bytes(), fixture.originals[id], "safe animations must retain every original encoded frame");
            assert_original(&fixture, id);
        }
        let frames = if id == GIF_ID {
            image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&fixture.originals[id])).unwrap().into_frames().collect_frames().unwrap()
        } else {
            image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&fixture.originals[id])).unwrap().into_frames().collect_frames().unwrap()
        };
        assert_eq!(frames.len(), if id == GIF_ID { 4 } else { 3 });
        assert_ne!(frames[0].buffer(), frames[1].buffer());
        assert!(frames.iter().all(|frame| frame.delay() == image::Delay::from_numer_denom_ms(90, 1)));
    }
    assert_eq!(hashes(&fixture.runtime.join("Image")), before);
}

#[test]
fn same_path_replacement_rejects_stale_source_and_projection_identity() {
    let fixture = fixture();
    let record = fixture.clip(PNG_ID);
    let dto = clip_to_dto(&record, None);
    let path = fixture.host.hover_image_file(PNG_ID, false).unwrap();
    let old = probe_source(&path).unwrap();
    let old_projection = primary_image_projection(&dto, &old.source_version).unwrap();
    let old_resources = load_primary_image_resources(&fixture.host, PNG_ID, &old.source_version);
    let old_display = old_resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap().image.bytes().to_vec();
    let old_cache = hashes(&thumbnail_cache_root(&fixture));
    assert!(!old_cache.is_empty(), "the initial source should have a persistent thumbnail");
    // Only this authored test candidate is changed; no production copy exists.
    let replacement = authored::png(1920, 1080, 199).unwrap();
    fs::write(&path, &replacement).unwrap();
    let current = probe_source(&path).unwrap();
    assert_ne!(old.source_version, current.source_version);
    assert_ne!(old_projection, primary_image_projection(&dto, &current.source_version).unwrap());
    let stale = load_primary_image_resources(&fixture.host, PNG_ID, &old.source_version);
    assert!(stale.images[PRIMARY_IMAGE_RESOURCE].is_none());
    let fresh = load_primary_image_resources(&fixture.host, PNG_ID, &current.source_version);
    assert_eq!(fresh.images[PRIMARY_IMAGE_RESOURCE].as_ref().map(|image|(image.width,image.height)), Some((1920.0,1080.0)));
    assert_ne!(fresh.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap().image.bytes(), old_display);
    assert_ne!(hashes(&thumbnail_cache_root(&fixture)), old_cache, "replacement source must generate a distinct persistent version");
    assert_eq!(fixture.host.image_bytes(PNG_ID).unwrap().0, replacement, "source admission must never substitute a display thumbnail");
    assert_eq!(fixture.clip(PNG_ID).image_path, record.image_path);
}

#[test]
fn primary_projection_survives_title_pin_display_and_search_changes() {
    let fixture = fixture();
    let path = fixture.host.hover_image_file(PNG_ID, false).unwrap();
    let version = probe_source(&path).unwrap().source_version;
    let dto = clip_to_dto(&fixture.clip(PNG_ID), None);
    let projection = primary_image_projection(&dto, &version).unwrap();
    let mut changed = dto.clone();
    changed.title = "renamed image 中文".into();
    changed.summary = "updated summary".into();
    changed.is_top = !dto.is_top;
    changed.display_text = "new row display".into();
    changed.highlighted_text = "new search hit".into();
    changed.highlight_hit_ranges = vec![HighlightHitRangeDto { start: 4, end: 10 }];
    assert_ne!(serde_json::to_value(&changed).unwrap(), serde_json::to_value(&dto).unwrap());
    assert_eq!(primary_image_projection(&changed, &version).unwrap(), projection,
        "row presentation changes cannot trigger decoding the same image again");
    changed.id = "different-image-entry".into();
    assert_ne!(primary_image_projection(&changed, &version).unwrap(), projection);
    assert_original(&fixture, PNG_ID);
}

#[test]
fn disk_thumbnail_is_reused_by_repeated_loads_and_an_independent_host() {
    let fixture = fixture();
    let source_before = hashes(&fixture.source);
    let images_before = original_image_hashes(&fixture.runtime.join("Image"));
    let path = fixture.host.hover_image_file(PNG_ID, false).unwrap();
    let version = probe_source(&path).unwrap().source_version;
    let first = load_primary_image_resources(&fixture.host, PNG_ID, &version);
    let display_bytes = first.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap().image.bytes().to_vec();
    let cache_root = thumbnail_cache_root(&fixture);
    let cache_before = hashes(&cache_root);
    assert_eq!(cache_before.len(), 1, "one static source produces one thumbnail envelope");
    let (relative_path, _) = cache_before.first_key_value().unwrap();
    assert_eq!(relative_path.extension().and_then(|extension| extension.to_str()), Some("thumb"));
    let cache_path = cache_root.join(relative_path);
    // A deliberate old timestamp makes regeneration observable without sleeps
    // or depending on the filesystem's clock resolution between worker loads.
    let cache_file = fs::OpenOptions::new().write(true).open(&cache_path).unwrap();
    cache_file.set_times(fs::FileTimes::new().set_modified(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
    )).unwrap();
    drop(cache_file);
    let cached_modified = fs::metadata(&cache_path).unwrap().modified().unwrap();
    for _ in 0..3 {
        let resources = load_primary_image_resources(&fixture.host, PNG_ID, &version);
        assert_eq!(resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap().image.bytes(), display_bytes);
        assert_eq!(fs::metadata(&cache_path).unwrap().modified().unwrap(), cached_modified,
            "a disk cache hit must not regenerate the PNG");
    }
    let independent_host = HostState::open_isolated_core(
        AppPaths::from_data_root(fixture.runtime.clone()), 29065,
    ).unwrap();
    let database_before = fs::read(fixture.runtime.join("userdata.db")).unwrap();
    let resumed = load_primary_image(&independent_host, PNG_ID);
    assert!(resumed.failure.is_none());
    assert!(resumed.thumbnail.is_some());
    assert_eq!(resumed.resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap().image.bytes(), display_bytes);
    assert_eq!(fs::metadata(&cache_path).unwrap().modified().unwrap(), cached_modified,
        "a fresh Host must reuse the thumbnail left by its predecessor");
    assert_eq!(hashes(&cache_root), cache_before);
    assert_eq!(fs::read(fixture.runtime.join("userdata.db")).unwrap(), database_before,
        "display caching cannot rewrite the business database");
    assert_eq!(original_image_hashes(&fixture.runtime.join("Image")), images_before);
    assert_eq!(hashes(&fixture.source), source_before);
    assert_original(&fixture, PNG_ID);
}

#[test]
fn visible_load_recovers_when_an_unavailable_source_returns_after_failed_probe() {
    let fixture = fixture();
    let source_before = hashes(&fixture.source);
    let images_before = original_image_hashes(&fixture.runtime.join("Image"));
    let record_before = serde_json::to_value(fixture.clip(PNG_ID)).unwrap();
    let path = fixture.host.hover_image_file(PNG_ID, false).unwrap();
    let absent_path = path.with_extension("temporarily-unavailable");
    fs::rename(&path, &absent_path).unwrap();
    assert!(probe_source(&path).is_err(), "query probe must observe the temporary miss");
    let missing = load_primary_image(&fixture.host, PNG_ID);
    assert!(missing.resources.images[PRIMARY_IMAGE_RESOURCE].is_none());
    assert!(missing.thumbnail.is_none());
    assert!(missing.failure.as_ref().is_some_and(|failure| failure.is_retryable()),
        "a missing source may return and must not become a permanent negative result");
    fs::rename(&absent_path, &path).unwrap();
    let recovered = load_primary_image(&fixture.host, PNG_ID);
    assert!(recovered.failure.is_none());
    assert!(recovered.thumbnail.is_some());
    assert!(recovered.resources.images[PRIMARY_IMAGE_RESOURCE].is_some(),
        "visible loading must re-probe even when the earlier query supplied no version");
    assert!(!hashes(&thumbnail_cache_root(&fixture)).is_empty());
    assert_eq!(serde_json::to_value(fixture.clip(PNG_ID)).unwrap(), record_before);
    assert_eq!(original_image_hashes(&fixture.runtime.join("Image")), images_before);
    assert_eq!(hashes(&fixture.source), source_before);
    assert_original(&fixture, PNG_ID);
}

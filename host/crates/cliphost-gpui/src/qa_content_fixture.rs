//! Debug-only, offline content fixtures. The CLI owns test-instance/headless/
//! independent-port gates; this helper independently confines every file to one
//! source-workspace `.tmp` run before creating its fresh SQLite database.

use std::{
    cell::RefCell,
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

use cliphost_core::{
    AppStore, ClipKind, ClipRecord,
    im_rich::document::{ImRichDocument, RichNodeKind, RichOrigin},
    model::{hash_files, hash_text, sha256_hex},
};
use cliphost_gpui::native_content::icons::{StepIconSpec, parse_step_icon};
use html5ever::{
    tendril::StrTendril,
    tokenizer::{BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer},
};
use serde::Deserialize;
use serde_json::Value;

const MAX_FIXTURE_BYTES: u64 = 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Fixture {
    schema_version: u32,
    items: Vec<FixtureItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FixtureItem {
    id: String,
    kind: String,
    title: Option<String>,
    body_text: Option<String>,
    html_data: Option<String>,
    html_source_url: Option<String>,
    qq_rich_text: Option<String>,
    #[serde(default)]
    file_paths: Vec<String>,
    image_path: Option<String>,
    source_process_name: Option<String>,
}

struct Location {
    run: PathBuf,
    data: PathBuf,
    fixture: PathBuf,
}

fn source_root() -> Result<PathBuf, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .ok_or("Could not resolve source workspace")?
        .canonicalize()
        .map_err(|error| error.to_string())
}

fn regular_file(path: &Path) -> Result<PathBuf, String> {
    let path = path.canonicalize().map_err(|error| error.to_string())?;
    if !path.is_file() {
        return Err("Fixture resource must be an existing regular file".into());
    }
    Ok(path)
}

fn locations(data: &Path, fixture: &Path, report: &Path) -> Result<Location, String> {
    if !data.is_absolute() || !fixture.is_absolute() || !report.is_absolute() {
        return Err("QA fixture paths must be absolute".into());
    }
    let root = source_root()?;
    let scratch = root
        .join(".tmp")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if scratch.parent() != Some(root.as_path()) || !scratch.is_dir() {
        return Err("Workspace .tmp must not escape the source repository".into());
    }
    if data.file_name() != Some(std::ffi::OsStr::new("data")) {
        return Err("QA data root must be run/data".into());
    }
    let requested_run = data.parent().ok_or("QA data root has no run parent")?;
    let run = requested_run
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let data = data.canonicalize().map_err(|error| error.to_string())?;
    if run == scratch
        || !run.starts_with(&scratch)
        || !run.is_dir()
        || !data.is_dir()
        || data.parent() != Some(run.as_path())
        || data.file_name() != Some(std::ffi::OsStr::new("data"))
    {
        return Err("QA data root must be an existing run/data inside source .tmp".into());
    }
    let fixture = regular_file(fixture)?;
    if !fixture.starts_with(&run) {
        return Err("QA fixture must belong to the same run as its data root".into());
    }
    let report_parent = report
        .parent()
        .ok_or("QA report has no parent")?
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !report_parent.is_dir() || !report_parent.starts_with(&run) {
        return Err("QA report parent must exist inside the same run".into());
    }
    if report.file_name().is_none() {
        return Err("QA report must name a file".into());
    }
    let requested_report = report_parent.join(report.file_name().unwrap());
    if requested_report == fixture
        || [
            "userdata.db",
            "userdata.db-wal",
            "userdata.db-shm",
            "userdata.db-journal",
        ]
        .iter()
        .any(|name| requested_report == data.join(name))
    {
        return Err("QA report must not overwrite its input or database".into());
    }
    match fs::symlink_metadata(report) {
        Ok(_) => {
            let target = regular_file(report)?;
            if !target.starts_with(&run) || target == fixture {
                return Err("QA report must not escape its run".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    ensure_fresh(&data)?;
    Ok(Location { run, data, fixture })
}

fn ensure_fresh(data: &Path) -> Result<(), String> {
    for name in [
        "userdata.db",
        "userdata.db-wal",
        "userdata.db-shm",
        "userdata.db-journal",
    ] {
        match fs::symlink_metadata(data.join(name)) {
            Ok(_) => return Err("QA fixture refuses an existing database or SQLite sidecar".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path).map_err(|error| error.to_string())?;
    if file.metadata().map_err(|error| error.to_string())?.len() > limit {
        return Err("QA fixture/resource exceeds its input byte limit".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("QA fixture/resource grew past its input byte limit".into());
    }
    Ok(bytes)
}

fn local_resource(run: &Path, raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    let path = if raw
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        let url = url::Url::parse(raw).map_err(|error| error.to_string())?;
        if url.host_str().is_some_and(|host| host != "localhost")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Only local file URLs are allowed in QA resources".into());
        }
        url.to_file_path().map_err(|_| "Invalid local file URL")?
    } else {
        PathBuf::from(raw)
    };
    // Relative references would be interpreted using the host's CWD later,
    // rather than this fixture run. Preserve strings, so reject that ambiguity.
    if raw.is_empty() || !path.is_absolute() {
        return Err("Local QA resources must be existing absolute paths".into());
    }
    let path = regular_file(&path)?;
    if !path.starts_with(run) {
        return Err("QA resource escaped its fixture run".into());
    }
    Ok(path)
}

fn image_reference(run: &Path, raw: &str) -> Result<(), String> {
    let text = raw.trim();
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("data:image/") {
        let (header, data) = text.split_once(',').ok_or("Incomplete QA data image")?;
        let mime = header[5..]
            .split(';')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(
            mime.as_str(),
            "image/png" | "image/gif" | "image/jpeg" | "image/webp" | "image/bmp"
        ) || data.is_empty()
        {
            return Err("QA embedded image must use a supported raster MIME type".into());
        }
        return Ok(());
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let url = url::Url::parse(text).map_err(|error| error.to_string())?;
        let host = url.host_str().unwrap_or_default();
        let port = url
            .port()
            .ok_or("QA loopback image needs an explicit port")?;
        if !matches!(host, "127.0.0.1" | "[::1]" | "::1")
            || port < 1024
            || [28865, 28965, 18765, 18776, 28766].contains(&port)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "QA image URL must use controlled numeric loopback and an independent port".into(),
            );
        }
        return Ok(());
    }
    local_resource(run, text).map(|_| ())
}

#[derive(Default)]
struct MarkupScan {
    references: Vec<String>,
    styles: Vec<String>,
    style_depth: usize,
    error: Option<String>,
    xml: bool,
}

#[derive(Default)]
struct MarkupSink(RefCell<MarkupScan>);

impl TokenSink for MarkupSink {
    type Handle = ();

    fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<()> {
        let mut scan = self.0.borrow_mut();
        match token {
            Token::TagToken(tag) => {
                let name = tag.name.as_ref();
                if tag.kind == TagKind::StartTag {
                    if matches!(
                        name,
                        "script" | "iframe" | "object" | "embed" | "link" | "base"
                    ) {
                        scan.error =
                            Some("Active/external markup is outside QA fixture scope".into());
                    }
                    if name == "style" {
                        scan.style_depth += 1;
                    }
                    for attr in tag.attrs {
                        match attr.name.local.as_ref() {
                            "src" | "background" | "poster" | "filepath" => {
                                scan.references.push(attr.value.to_string())
                            }
                            "style" => scan.styles.push(attr.value.to_string()),
                            "srcset" => {
                                scan.error =
                                    Some("QA fixture srcset resources are unsupported".into())
                            }
                            _ => {}
                        }
                    }
                } else if name == "style" {
                    scan.style_depth = scan.style_depth.saturating_sub(1);
                }
            }
            Token::CharacterTokens(text) if scan.style_depth > 0 => {
                scan.styles.push(text.to_string())
            }
            Token::ParseError(error) if !scan.xml => {
                scan.error = Some(format!("Unverifiable QA markup: {error}"))
            }
            Token::NullCharacterToken => {
                scan.error = Some("Null markup cannot be verified as a QA fixture".into())
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

fn validate_css(run: &Path, css: &str) -> Result<(), String> {
    let lower = css.to_ascii_lowercase();
    if css.contains('\\') || css.contains("/*") || lower.contains("@import") {
        return Err("Escaped/commented/imported CSS resources are outside QA fixture scope".into());
    }
    let mut rest = css;
    while let Some(at) = rest.to_ascii_lowercase().find("url(") {
        rest = &rest[at + 4..];
        let end = rest.find(')').ok_or("Incomplete CSS URL in QA fixture")?;
        let target = rest[..end].trim().trim_matches(['\'', '"']);
        image_reference(run, target)?;
        rest = &rest[end + 1..];
    }
    Ok(())
}

fn validate_markup(run: &Path, raw: &str, xml: bool) -> Result<(), String> {
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(raw));
    // Core's lossless XML parser validates QQ syntax. HTML tokenization merely
    // collects extra attributes there; XML declarations/CDATA are not HTML errors.
    let tokenizer = Tokenizer::new(
        MarkupSink(RefCell::new(MarkupScan {
            xml,
            ..Default::default()
        })),
        Default::default(),
    );
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let scan = tokenizer.sink.0.into_inner();
    if let Some(error) = scan.error {
        return Err(error);
    }
    for reference in scan.references {
        image_reference(run, &reference)?;
    }
    for css in scan.styles {
        validate_css(run, &css)?;
    }
    Ok(())
}

fn validate_icon(run: &Path, raw: &str) -> Result<(), String> {
    // Reuse the actual renderer's dispatch instead of accepting arbitrary
    // strings just because they begin with an apparent icon prefix.
    match parse_step_icon(raw) {
        StepIconSpec::FontAwesome(_) => Ok(()),
        StepIconSpec::Remote(uri) => image_reference(run, &uri),
        StepIconSpec::Shell(value) if value.starts_with('#') => Ok(()),
        StepIconSpec::Shell(value) if value.contains(['/', '\\', ':', '.']) => {
            image_reference(run, &value)
        }
        StepIconSpec::Shell(_) => Ok(()), // Existing inert text/emoji fallback.
    }
}

fn validate_step_resources(run: &Path, value: &Value) -> Result<(), String> {
    match value {
        Value::Array(values) => {
            for value in values {
                validate_step_resources(run, value)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if let Some(text) = value.as_str() {
                    match key.to_ascii_lowercase().as_str() {
                        "icon" | "primaryicon" => validate_icon(run, text)?,
                        "imagepath" | "imageurl" | "imagesource" | "filepath" => {
                            image_reference(run, text)?;
                        }
                        _ => {}
                    }
                }
                validate_step_resources(run, value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn record(run: &Path, item: FixtureItem, index: usize) -> Result<ClipRecord, String> {
    let kind = match item.kind.as_str() {
        "TextClipItem" => ClipKind::TextClipItem,
        "HtmlClipItem" => ClipKind::HtmlClipItem,
        "QQRichTextItem" => ClipKind::QQRichTextItem,
        "StepClipItem" => ClipKind::StepClipItem,
        "FileClipItem" => ClipKind::FileClipItem,
        "ImageClipItem" => ClipKind::ImageClipItem,
        _ => return Err("Only passive supported content kinds are allowed in QA fixtures".into()),
    };
    for path in &item.file_paths {
        if !Path::new(path).is_absolute() {
            return Err("filePaths requires native absolute filesystem paths".into());
        }
        local_resource(run, path)?;
    }
    if item
        .image_path
        .as_ref()
        .is_some_and(|path| !Path::new(path).is_absolute())
    {
        return Err("imagePath requires a native absolute filesystem path".into());
    }
    let image = item
        .image_path
        .as_ref()
        .map(|path| local_resource(run, path))
        .transpose()?;
    if let Some(html) = &item.html_data {
        validate_markup(run, html, false)?;
    }
    if let Some(xml) = &item.qq_rich_text {
        let origin = cliphost_core::im_rich::guess_origin_from_xml(xml);
        let document = ImRichDocument::parse(xml, RichOrigin::from_token(origin.as_api_token()))
            .map_err(|error| error.to_string())?;
        for node in document.nodes() {
            if node.path().is_empty() {
                continue;
            }
            match node.kind() {
                RichNodeKind::Image => image_reference(run, node.path())?,
                RichNodeKind::File => {
                    local_resource(run, node.path())?;
                }
                _ => {}
            }
        }
        validate_markup(run, xml, true)?;
    }
    if kind == ClipKind::StepClipItem {
        let text = item
            .body_text
            .as_deref()
            .ok_or("Step QA fixture needs bodyText JSON")?;
        let step: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
        validate_step_resources(run, &step)?;
    }
    if kind == ClipKind::HtmlClipItem && item.html_data.is_none()
        || kind == ClipKind::QQRichTextItem && item.qq_rich_text.is_none()
        || kind == ClipKind::FileClipItem && item.file_paths.is_empty()
        || kind == ClipKind::ImageClipItem && image.is_none()
    {
        return Err("QA fixture is missing the required payload for its kind".into());
    }
    let mut row = ClipRecord::new_text(
        item.body_text.clone().unwrap_or_default(),
        item.source_process_name
            .unwrap_or_else(|| "qa-content-fixture".into()),
    );
    row.id = item.id;
    row.kind = kind;
    row.title = item.title;
    row.body_text = item.body_text;
    row.html_data = item.html_data;
    row.html_source_url = item.html_source_url;
    row.qq_rich_text = item.qq_rich_text;
    row.file_paths = item.file_paths;
    row.image_path = item.image_path;
    row.sub_index = index as i32;
    row.content_hash = match kind {
        ClipKind::QQRichTextItem => hash_text(&format!(
            "qq:{}",
            row.qq_rich_text.as_deref().unwrap_or_default()
        )),
        ClipKind::FileClipItem => hash_files(&row.file_paths),
        ClipKind::ImageClipItem => {
            sha256_hex(&bounded_read(image.as_deref().unwrap(), MAX_IMAGE_BYTES)?)
        }
        _ => hash_text(row.body_text.as_deref().unwrap_or_default()),
    };
    Ok(row)
}

pub(crate) fn seed(
    data_root: &Path,
    fixture_path: &Path,
    report_path: &Path,
) -> Result<Vec<String>, String> {
    let location = locations(data_root, fixture_path, report_path)?;
    let fixture: Fixture =
        serde_json::from_slice(&bounded_read(&location.fixture, MAX_FIXTURE_BYTES)?)
            .map_err(|error| error.to_string())?;
    if fixture.schema_version != 1 || fixture.items.is_empty() || fixture.items.len() > 32 {
        return Err("QA fixture needs schemaVersion 1 and 1..32 items".into());
    }
    let mut ids = HashSet::new();
    let mut rows = Vec::with_capacity(fixture.items.len());
    for (index, item) in fixture.items.into_iter().enumerate() {
        if item.id.trim().is_empty() || !ids.insert(item.id.clone()) {
            return Err("QA fixture IDs must be nonempty and unique".into());
        }
        rows.push(record(&location.run, item, index)?);
    }
    // One synthetic capture burst gives the existing SubIndex tie-breaker a
    // stable fixture order, without adding timestamp fields to the input schema.
    let timestamp = rows[0].create_time;
    for row in &mut rows {
        row.create_time = timestamp;
        row.update_time = timestamp;
    }
    // Validation and file hashing precede all database creation. Recheck the
    // canonical run and sidecars, then reserve this one fresh file exclusively.
    let current = locations(data_root, fixture_path, report_path)?;
    if current.run != location.run
        || current.data != location.data
        || current.fixture != location.fixture
    {
        return Err("QA fixture paths changed during validation".into());
    }
    let database = current.data.join("userdata.db");
    drop(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&database)
            .map_err(|error| error.to_string())?,
    );
    let store = AppStore::open(&database).map_err(|error| error.to_string())?;
    store
        .insert_clips(&rows)
        .map_err(|error| error.to_string())?;
    drop(store);
    Ok(rows.into_iter().map(|row| row.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run() -> tempfile::TempDir {
        let scratch = source_root().unwrap().join(".tmp");
        fs::create_dir_all(&scratch).unwrap();
        let run = tempfile::Builder::new()
            .prefix("qa-content-fixture-")
            .tempdir_in(scratch)
            .unwrap();
        fs::create_dir(run.path().join("data")).unwrap();
        run
    }

    fn write_fixture(run: &Path, value: &Value) -> PathBuf {
        let path = run.join("fixture.json");
        fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path
    }

    fn seed_value(run: &Path, value: &Value) -> Result<Vec<String>, String> {
        seed(
            &run.join("data"),
            &write_fixture(run, value),
            &run.join("report.json"),
        )
    }

    #[test]
    fn valid_all_content_kinds_roundtrip_original_payloads_and_hashes() {
        let run = run();
        let images = run.path().join("data/Image/Temp");
        fs::create_dir_all(&images).unwrap();
        let image = images.join("picture.png");
        let file = run.path().join("notes.txt");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([17, 34, 51, 255]))
            .save(&image)
            .unwrap();
        fs::write(&file, b"fixture file").unwrap();
        let image_text = image.to_string_lossy().into_owned();
        let file_text = file.to_string_lossy().into_owned();
        let html = "<table><tr><td style='color:#123456;font-size:14px'>保留😀</td></tr></table>";
        let qq = format!(
            "<?xml version='1.0'?>{}",
            cliphost_core::im_rich::qq_image_envelope(&image_text)
        );
        let step =
            "{\"Steps\":[{\"Title\":\"原样\",\"Icon\":\"fa:Light_File\",\"Private\":\"keep\"}]}";
        let ids = seed_value(run.path(), &json!({"schemaVersion":1,"items":[
            {"id":"text","kind":"TextClipItem","bodyText":" 文😀\r\n字 ","title":"标题"},
            {"id":"html","kind":"HtmlClipItem","bodyText":"保留😀","htmlData":html,"htmlSourceUrl":"https://example.test/source"},
            {"id":"qq","kind":"QQRichTextItem","bodyText":"[图片]","qqRichText":qq},
            {"id":"step","kind":"StepClipItem","bodyText":step},
            {"id":"file","kind":"FileClipItem","filePaths":[file_text]},
            {"id":"image","kind":"ImageClipItem","imagePath":image_text}
        ]})).unwrap();
        assert_eq!(ids, ["text", "html", "qq", "step", "file", "image"]);
        let store = AppStore::open(&run.path().join("data/userdata.db")).unwrap();
        assert_eq!(store.list_clips().unwrap().len(), 6);
        assert_eq!(
            store.get_clip("text").unwrap().body_text.as_deref(),
            Some(" 文😀\r\n字 ")
        );
        assert_eq!(
            store.get_clip("html").unwrap().html_data.as_deref(),
            Some(html)
        );
        assert_eq!(
            store.get_clip("html").unwrap().html_source_url.as_deref(),
            Some("https://example.test/source")
        );
        assert_eq!(
            store.get_clip("qq").unwrap().qq_rich_text.as_deref(),
            Some(qq.as_str())
        );
        assert_eq!(
            store.get_clip("qq").unwrap().content_hash,
            hash_text(&format!("qq:{qq}"))
        );
        assert_eq!(
            store.get_clip("step").unwrap().body_text.as_deref(),
            Some(step)
        );
        assert_eq!(
            store.get_clip("file").unwrap().content_hash,
            hash_files(&[file_text])
        );
        assert_eq!(
            store.get_clip("image").unwrap().content_hash,
            sha256_hex(&fs::read(&image).unwrap())
        );
        assert!(!run.path().join("report.json").exists());
    }

    #[test]
    fn invalid_schema_unknown_fields_kinds_and_duplicate_ids_never_create_database() {
        for fixture in [
            json!({"schemaVersion":2,"items":[{"id":"a","kind":"TextClipItem"}]}),
            json!({"schemaVersion":1,"items":[]}),
            json!({"schemaVersion":1,"items":[{"id":"a","kind":"CmdClipItem"}]}),
            json!({"schemaVersion":1,"items":[{"id":"a","kind":"ActionClipItem"}]}),
            json!({"schemaVersion":1,"items":[{"id":"a","kind":"unknown"}]}),
            json!({"schemaVersion":1,"items":[{"id":"valid-first","kind":"TextClipItem","bodyText":"keep"},{"id":"invalid-last","kind":"CmdClipItem"}]}),
            json!({"schemaVersion":1,"items":[{"id":"a","kind":"TextClipItem","executeParam":"unsafe"}]}),
            json!({"schemaVersion":1,"unknown":true,"items":[{"id":"a","kind":"TextClipItem"}]}),
            json!({"schemaVersion":1,"items":[{"id":" ","kind":"TextClipItem"}]}),
            json!({"schemaVersion":1,"items":[{"id":"a","kind":"TextClipItem"},{"id":"a","kind":"TextClipItem"}]}),
        ] {
            let run = run();
            assert!(seed_value(run.path(), &fixture).is_err());
            assert!(!run.path().join("data/userdata.db").exists());
        }
    }

    #[test]
    fn existing_database_or_sidecar_and_shared_run_mismatch_are_rejected() {
        for name in [
            "userdata.db",
            "userdata.db-wal",
            "userdata.db-shm",
            "userdata.db-journal",
        ] {
            let run = run();
            let sentinel = run.path().join("data").join(name);
            fs::write(&sentinel, b"preserve").unwrap();
            assert!(
                seed_value(
                    run.path(),
                    &json!({"schemaVersion":1,"items":[{"id":"a","kind":"TextClipItem"}]})
                )
                .is_err()
            );
            assert_eq!(fs::read(sentinel).unwrap(), b"preserve");
        }
        let first = run();
        let second = run();
        let fixture = write_fixture(
            first.path(),
            &json!({"schemaVersion":1,"items":[{"id":"a","kind":"TextClipItem"}]}),
        );
        assert!(
            seed(
                &second.path().join("data"),
                &fixture,
                &second.path().join("report.json")
            )
            .is_err()
        );
        assert!(
            seed(
                &first.path().join("data"),
                &fixture,
                &second.path().join("report.json")
            )
            .is_err()
        );
        assert!(seed(&first.path().join("data"), &fixture, &fixture).is_err());
        assert!(
            seed(
                &first.path().join("data"),
                &fixture,
                &first.path().join("data/userdata.db")
            )
            .is_err()
        );
        assert!(!first.path().join("data/userdata.db").exists());
        assert!(!second.path().join("data/userdata.db").exists());
    }

    #[test]
    fn declared_local_html_qq_step_resources_cannot_escape_run() {
        let outside = run();
        let outside_image = outside.path().join("private.png");
        fs::write(&outside_image, b"not a fixture for the tested run").unwrap();
        let path = outside_image.to_string_lossy().into_owned();
        for item in [
            json!({"id":"a","kind":"ImageClipItem","imagePath":path}),
            json!({"id":"a","kind":"FileClipItem","filePaths":[path]}),
            json!({"id":"a","kind":"HtmlClipItem","htmlData":format!("<img src='{}'>", url::Url::from_file_path(&outside_image).unwrap())}),
            json!({"id":"a","kind":"QQRichTextItem","qqRichText":cliphost_core::im_rich::qq_image_envelope(&path)}),
            json!({"id":"a","kind":"StepClipItem","bodyText":json!({"Steps":[{"Icon":path}]}).to_string()}),
        ] {
            let run = run();
            assert!(seed_value(run.path(), &json!({"schemaVersion":1,"items":[item]})).is_err());
            assert!(!run.path().join("data/userdata.db").exists());
        }
        assert_eq!(
            fs::read(outside_image).unwrap(),
            b"not a fixture for the tested run"
        );
    }

    #[test]
    fn fixture_size_item_limit_and_unverifiable_resources_are_rejected_before_database() {
        let run = run();
        let fixture = run.path().join("fixture.json");
        fs::write(&fixture, vec![b' '; MAX_FIXTURE_BYTES as usize + 1]).unwrap();
        assert!(
            seed(
                &run.path().join("data"),
                &fixture,
                &run.path().join("report.json")
            )
            .is_err()
        );
        let items: Vec<_> = (0..33)
            .map(|i| json!({"id":i.to_string(),"kind":"TextClipItem"}))
            .collect();
        assert!(seed_value(run.path(), &json!({"schemaVersion":1,"items":items})).is_err());
        for html in [
            "<img src='https://example.test/private.png'>",
            "<img src='relative.png'>",
            "<img srcset='https://example.test/a.png 1x'>",
            "<div style='background:url(file:///C:/outside.png)'>x</div>",
            "<style>@import 'https://example.test/style.css';</style>",
        ] {
            assert!(seed_value(run.path(), &json!({"schemaVersion":1,"items":[{"id":"a","kind":"HtmlClipItem","htmlData":html}]})).is_err());
        }
        assert!(!run.path().join("data/userdata.db").exists());
    }

    #[test]
    fn data_images_and_independent_numeric_loopback_are_allowed_without_fetching() {
        let run = run();
        assert!(image_reference(run.path(), "data:image/png;base64,aGVsbG8=").is_ok());
        assert!(image_reference(run.path(), "http://127.0.0.1:39075/image.png").is_ok());
        assert!(image_reference(run.path(), "http://[::1]:39075/image.gif").is_ok());
        for uri in [
            "http://127.0.0.1/image.png",
            "http://localhost:39075/image.png",
            "http://127.0.0.1:28965/image.png",
            "http://user@127.0.0.1:39075/image.png",
            "data:image/svg+xml,<svg/>",
        ] {
            assert!(image_reference(run.path(), uri).is_err());
        }
        // Plain path-looking text is display content and never a declared file.
        let text = r"C:\\not-a-fixture\\ordinary-display.txt";
        assert!(
            seed_value(
                run.path(),
                &json!({"schemaVersion":1,"items":[{
                    "id":"path-text","kind":"TextClipItem","bodyText":text
                }]})
            )
            .is_ok()
        );
        let store = AppStore::open(&run.path().join("data/userdata.db")).unwrap();
        assert_eq!(
            store.get_clip("path-text").unwrap().body_text.as_deref(),
            Some(text)
        );
    }
}

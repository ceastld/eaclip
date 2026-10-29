//! Prepare clipboard bytes and native memory before dispatching the short write
//! to the clipboard owner thread. Preparation never opens or empties the clipboard.

use crate::clipboard::{self, ImagePasteTarget};
use crate::model::{ClipKind, ClipRecord};
use std::path::Path;

#[cfg(all(test, windows))]
#[path = "clipboard_payload_native_fixture.rs"]
mod native_fixture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Standard(u32),
    Registered(&'static str),
}

#[derive(Default)]
struct Payload(Vec<(Format, Vec<u8>)>);

impl Payload {
    fn unicode(&mut self, text: &str) {
        self.0.push((Format::Standard(13), utf16(text)));
    }

    fn utf8(&mut self, name: &'static str, text: &str) {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        self.0.push((Format::Registered(name), bytes));
    }

    fn files(&mut self, paths: &[String]) {
        // DROPFILES is 20 bytes on both Windows ABIs: DWORD offset, POINT,
        // BOOL non-client, BOOL wide; followed by double-NUL UTF-16 paths.
        let mut bytes = vec![0; 20];
        bytes[..4].copy_from_slice(&20u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
        for path in paths {
            bytes.extend(utf16(path));
        }
        bytes.extend([0, 0]);
        self.0.push((Format::Standard(15), bytes));
    }

    fn image(&mut self, path: &str, target: ImagePasteTarget) {
        match target {
            ImagePasteTarget::FileDropOnly => self.files(&[path.to_string()]),
            ImagePasteTarget::QqImage => {
                self.utf8(
                    "QQ_Unicode_RichEdit_Format",
                    &crate::im_rich::qq_image_envelope(path),
                );
            }
            ImagePasteTarget::BitmapAndFile => {
                // Keep the existing file-drop fallback if decoding fails. Read
                // once so PNG and DIB represent the same file snapshot.
                if let Ok(bytes) = std::fs::read(path) {
                    if let Ok(dib) = clipboard::encode_dib_from_bytes(&bytes) {
                        self.0.push((Format::Standard(8), dib));
                    }
                    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                        self.0.push((Format::Registered("PNG"), bytes));
                    }
                }
                self.files(&[path.to_string()]);
            }
        }
    }

    fn prepare(self) -> Result<PreparedClipboard, String> {
        if self.0.is_empty() {
            return Err("No clipboard data to copy.".into());
        }
        #[cfg(windows)]
        {
            use windows::core::HSTRING;
            use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
            let mut formats = Vec::with_capacity(self.0.len());
            for (format, bytes) in self.0 {
                let id = match format {
                    Format::Standard(id) => id,
                    Format::Registered(name) => {
                        let id = unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) };
                        if id == 0 {
                            return Err(format!("RegisterClipboardFormatW failed for {name}"));
                        }
                        id
                    }
                };
                formats.push((id, ClipboardMemory::allocate(&bytes)?));
            }
            Ok(PreparedClipboard { formats })
        }
        #[cfg(not(windows))]
        Err("clipboard write is Windows-only".into())
    }
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Owns already encoded, allocated formats; Send, but deliberately not Clone.
/// Dropping a cancelled operation frees all memory not handed to Windows.
#[derive(Default)]
pub(crate) struct PreparedClipboard {
    #[cfg(windows)]
    formats: Vec<(u32, ClipboardMemory)>,
}

impl PreparedClipboard {
    pub(crate) fn write(self) -> Result<(), String> {
        self.write_if_current(None)
    }

    pub(crate) fn write_if_current(self, expected_sequence: Option<u32>) -> Result<(), String> {
        self.write_if_current_with_guard(expected_sequence, || Ok(()))
    }

    pub(crate) fn write_if_current_with_guard(
        self,
        expected_sequence: Option<u32>,
        verify: impl Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        verify()?;
        #[cfg(all(feature = "native-acceptance", not(test)))]
        crate::native_acceptance::verify_clipboard_write()?;
        if clipboard::is_headless() {
            return crate::selection_capture::verify_unchanged(expected_sequence);
        }
        #[cfg(windows)]
        {
            use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard};
            if self.formats.is_empty() {
                return Err("No clipboard data to copy.".into());
            }
            #[cfg(test)]
            let formats = native_fixture::exclude_fixture_capture(self.formats)?;
            #[cfg(not(test))]
            let formats = self.formats;
            #[cfg(all(feature = "native-acceptance", not(test)))]
            let formats = exclude_capture(formats, crate::native_acceptance::clipboard_nonce()?)?;
            open_for_write()?;
            struct Close;
            impl Drop for Close {
                fn drop(&mut self) {
                    unsafe {
                        let _ = CloseClipboard();
                    }
                }
            }
            let _close = Close;
            // Hold the native clipboard lock before validating, so a newer copy
            // cannot slip between the check and EmptyClipboard.
            verify()?;
            crate::selection_capture::verify_unchanged(expected_sequence)?;
            unsafe {
                EmptyClipboard().map_err(|e| e.to_string())?;
            }
            publish_formats(formats, |format, memory| memory.publish(format))
        }
        #[cfg(not(windows))]
        Err("clipboard write is Windows-only".into())
    }
}

#[cfg(all(windows, any(test, feature = "native-acceptance")))]
fn exclude_capture(
    formats: Vec<(u32, ClipboardMemory)>,
    nonce: &str,
) -> Result<Vec<(u32, ClipboardMemory)>, String> {
    use windows::{core::HSTRING, Win32::System::DataExchange::RegisterClipboardFormatW};
    let mut marked = Vec::new();
    for (name, bytes) in [
        ("ClipHost.NativeFixture", [nonce.as_bytes(), &[0]].concat()),
        (
            "ExcludeClipboardContentFromMonitorProcessing",
            1u32.to_le_bytes().to_vec(),
        ),
        ("CanIncludeInClipboardHistory", 0u32.to_le_bytes().to_vec()),
        ("CanUploadToCloudClipboard", 0u32.to_le_bytes().to_vec()),
    ] {
        let id = unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) };
        if id == 0 {
            return Err(format!("Cannot register native fixture format {name}"));
        }
        marked.push((id, ClipboardMemory::allocate(&bytes)?));
    }
    marked.extend(formats);
    Ok(marked)
}

#[cfg(any(windows, test))]
fn publish_formats<T>(
    formats: Vec<(u32, T)>,
    mut publish: impl FnMut(u32, T) -> Result<(), String>,
) -> Result<(), String> {
    if formats.is_empty() {
        return Err("No clipboard data to copy.".into());
    }
    for (format, memory) in formats {
        // A partial payload is not ready to paste. Propagate the first failure
        // even if another format was already transferred to Windows.
        publish(format, memory)
            .map_err(|error| format!("Clipboard format {format} failed: {error}"))?;
    }
    Ok(())
}

/// Keep a message-only owner on the writing thread. OpenClipboard(NULL) is valid
/// for reads, but EmptyClipboard then removes ownership required for publication.
/// https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-openclipboard
#[cfg(windows)]
fn open_for_write() -> Result<(), String> {
    use windows::core::w;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    };
    struct Owner(HWND);
    impl Drop for Owner {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }
    thread_local! {
        static OWNER: std::cell::RefCell<Option<Owner>> = const { std::cell::RefCell::new(None) };
    }
    OWNER.with(|slot| {
        let mut owner = slot.borrow_mut();
        if owner.is_none() {
            let window = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    w!("ClipHost Clipboard Owner"),
                    WINDOW_STYLE::default(),
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    None,
                    None,
                    None,
                )
            }
            .map_err(|error| format!("Cannot create clipboard owner: {error}"))?;
            *owner = Some(Owner(window));
        }
        clipboard::open_clipboard_with_owner(owner.as_ref().unwrap().0)
    })
}

#[cfg(windows)]
struct ClipboardMemory(windows::Win32::Foundation::HGLOBAL);

// SAFETY: this owns an unlocked GMEM_MOVEABLE allocation, not thread-affine
// window/COM state. No pointer or alias escapes. Publishing consumes ownership;
// before publication it can be moved to (and freed on) another process thread.
#[cfg(windows)]
unsafe impl Send for ClipboardMemory {}

#[cfg(windows)]
impl ClipboardMemory {
    fn allocate(bytes: &[u8]) -> Result<Self, String> {
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
        };
        unsafe {
            let memory = Self(GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| e.to_string())?);
            let data = GlobalLock(memory.0);
            if data.is_null() {
                return Err("GlobalLock failed".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast(), bytes.len());
            let _ = GlobalUnlock(memory.0);
            Ok(memory)
        }
    }

    fn publish(self, format: u32) -> Result<(), String> {
        use windows::Win32::{Foundation::HANDLE, System::DataExchange::SetClipboardData};
        unsafe {
            SetClipboardData(format, HANDLE(self.0 .0)).map_err(|e| e.to_string())?;
        }
        // SetClipboardData transfers ownership only on success.
        // https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-setclipboarddata
        std::mem::forget(self);
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ClipboardMemory {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::GlobalFree(self.0);
        }
    }
}

/// GIF copy carries the original file and the existing QQ/WeChat image format.
/// Do not include DIB/PNG: consumers might prefer those and flatten animation.
pub(crate) fn prepare_gif_file(path: &str) -> Result<PreparedClipboard, String> {
    if !Path::new(path).is_file() {
        return Err("GIF file not found.".into());
    }
    let mut payload = Payload::default();
    payload.files(&[path.to_string()]);
    payload.image(path, ImagePasteTarget::QqImage);
    payload.prepare()
}

pub(crate) fn prepare_text(text: &str) -> Result<PreparedClipboard, String> {
    if clipboard::is_headless() {
        return Ok(PreparedClipboard::default());
    }
    let mut payload = Payload::default();
    payload.unicode(text);
    payload.prepare()
}

pub(crate) fn prepare_sync_text(text: &str, origin: &str) -> Result<PreparedClipboard, String> {
    if clipboard::is_headless() {
        return Ok(PreparedClipboard::default());
    }
    let mut payload = Payload::default();
    payload.unicode(text);
    let origin=content_bound_origin(origin,&format!("text:{}",crate::model::sha256_hex(text.as_bytes())));
    payload.utf8(crate::clipboard_sync::ORIGIN_FORMAT, &origin);
    payload.prepare()
}

pub(crate) fn prepare_sync_image(bytes: &[u8], origin: &str) -> Result<PreparedClipboard, String> {
    if clipboard::is_headless() {
        return Ok(PreparedClipboard::default());
    }
    // No file-drop fallback: every published image has a valid bitmap format.
    let dib = clipboard::encode_dib_from_bytes(bytes)?;
    // JPEG is published as a bitmap. Bind the marker to the exact PNG form the
    // native reader will observe, rather than to the original encoded JPEG.
    let png=if bytes.starts_with(b"\x89PNG\r\n\x1a\n"){bytes.to_vec()}
        else {clipboard::dib_to_png(dib.clone()).ok_or("Cannot encode the synchronized clipboard bitmap.")?};
    let mut payload = Payload::default();
    payload.0.push((Format::Standard(8), dib));
    let origin=content_bound_origin(origin,&crate::device_sync::image_key(&png)?);
    payload.0.push((Format::Registered("PNG"),png));
    payload.utf8(crate::clipboard_sync::ORIGIN_FORMAT, &origin);
    payload.prepare()
}

pub(crate) fn sync_image_platform_key(bytes:&[u8])->Result<String,String>{
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n"){return crate::device_sync::image_key(bytes);}
    let dib=clipboard::encode_dib_from_bytes(bytes)?;
    let png=clipboard::dib_to_png(dib).ok_or("Cannot encode the synchronized clipboard bitmap.")?;
    crate::device_sync::image_key(&png)
}

fn content_bound_origin(origin:&str,key:&str)->String{
    let mut value=serde_json::from_str::<serde_json::Value>(origin).ok().filter(serde_json::Value::is_object)
        .unwrap_or_else(||serde_json::json!({}));
    value["deviceSyncVersion"]=serde_json::json!(1);value["contentKey"]=serde_json::json!(key);value.to_string()
}

pub(crate) fn prepare_text_and_files(
    text: &str,
    paths: &[String],
) -> Result<PreparedClipboard, String> {
    if clipboard::is_headless() {
        return Ok(PreparedClipboard::default());
    }
    text_and_files_payload(text, paths)?.prepare()
}

fn text_and_files_payload(text: &str, paths: &[String]) -> Result<Payload, String> {
    let mut payload = Payload::default();
    let existing: Vec<_> = paths
        .iter()
        .filter(|path| Path::new(path).exists())
        .cloned()
        .collect();
    if !text.is_empty() {
        payload.unicode(text);
    }
    if !existing.is_empty() {
        payload.files(&existing);
    }
    if payload.0.is_empty() {
        return Err("No clipboard data to copy.".into());
    }
    Ok(payload)
}

pub(crate) fn prepare_record(
    item: &ClipRecord,
    plain_only: bool,
    new_line: bool,
    standard: bool,
) -> Result<PreparedClipboard, String> {
    require_output_projection(item)?;
    if item.kind == ClipKind::ActionClipItem {
        crate::quicker_action::action_id_from_payload(item.body_text.as_deref().unwrap_or(""))?;
    }
    if clipboard::is_headless() {
        return Ok(PreparedClipboard::default());
    }
    let (process, _) = clipboard::foreground_target();
    prepare_record_for_process(item, plain_only, new_line, standard, &process)
}

pub(crate) fn prepare_record_for_process(
    item: &ClipRecord,
    plain_only: bool,
    new_line: bool,
    standard: bool,
    process: &str,
) -> Result<PreparedClipboard, String> {
    record_payload(item, plain_only, new_line, standard, process)?.prepare()
}

fn record_payload(
    item: &ClipRecord,
    plain_only: bool,
    new_line: bool,
    standard: bool,
    process: &str,
) -> Result<Payload, String> {
    require_output_projection(item)?;
    let existing = if item.kind == ClipKind::FileClipItem {
        let files = clipboard::file_clip_existing_paths(item);
        if files.is_empty() {
            return Err("No existing files to copy.".into());
        }
        files
    } else {
        Vec::new()
    };
    if item.kind == ClipKind::ImageClipItem
        && !item
            .image_path
            .as_deref()
            .is_some_and(|path| Path::new(path).is_file())
    {
        return Err("Image file not found.".into());
    }
    let mut payload = Payload::default();
    if let Some(text) = clipboard::set_to_data_object_unicode(item, new_line, standard) {
        payload.unicode(&text);
    }
    // The plain-text preference strips rich text, not non-text clipboard data.
    // Applying it to the whole payload makes images/files/steps impossible to
    // paste whenever PlainTextPasteByDefault is enabled.
    match item.kind {
        ClipKind::HtmlClipItem | ClipKind::QQRichTextItem if !plain_only => {
            if let Some(html) = item.html_data.as_deref().filter(|html| !html.is_empty()) {
                let html = if crate::cf_html::looks_like_complete_format(html) {
                    html.to_string()
                } else {
                    crate::cf_html::build_clipboard_html(html, item.html_source_url.as_deref())
                };
                payload.utf8("HTML Format", &html);
            }
            if item.kind == ClipKind::QQRichTextItem {
                if let Some(rich) = item.qq_rich_text.as_deref() {
                    let (name, origin) = if clipboard::is_wechat_family(process) {
                        (
                            "WeChat_RichEdit_Format",
                            crate::im_rich::ImRichOrigin::WeChat,
                        )
                    } else {
                        (
                            "QQ_Unicode_RichEdit_Format",
                            crate::im_rich::ImRichOrigin::Qq,
                        )
                    };
                    payload.utf8(
                        name,
                        &crate::im_rich::rewrite_for_record_origin(
                            rich,
                            &item.source_process_name,
                            origin,
                        ),
                    );
                }
            }
        }
        ClipKind::StepClipItem => {
            if let Some(text) = item.body_text.as_deref() {
                payload
                    .0
                    .push((Format::Registered("quicker-action-steps"), utf16(text)));
            }
        }
        ClipKind::FileClipItem if !clipboard::file_clip_item_is_single_image(item) => {
            payload.files(&existing)
        }
        ClipKind::FileClipItem | ClipKind::ImageClipItem => {
            let path = if item.kind == ClipKind::FileClipItem {
                &item.file_paths[0]
            } else {
                item.image_path.as_deref().unwrap()
            };
            let len = std::fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            payload.image(path, clipboard::classify_image_paste_target(process, len));
        }
        _ => {}
    }
    if payload.0.is_empty() {
        return Err("No clipboard data to copy.".into());
    }
    Ok(payload)
}

/// A core token identifies an authoritative persistence snapshot. Final output
/// must use the explicit disposable resource projection, even in headless tests.
pub(crate) fn require_output_projection(item:&ClipRecord)->Result<(),String>{
    if item.storage_revision.as_deref().is_some_and(|token|token.starts_with("core2:")){
        return Err("CORE_OUTPUT_REQUIRES_PROJECTION: canonical core records cannot perform file or clipboard I/O.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_core_output_is_refused_before_headless_or_path_metadata(){
        for kind in [ClipKind::TextClipItem,ClipKind::ImageClipItem,ClipKind::FileClipItem,ClipKind::QQRichTextItem]{
            let mut item=ClipRecord::new_text("synthetic","fixture");item.kind=kind;item.storage_revision=Some("core2:synthetic".into());
            item.image_path=Some(r"Z:\offline-root-must-not-be-probed\image.png".into());item.file_paths=vec![r"Z:\offline-root-must-not-be-probed\file".into()];
            assert!(prepare_record(&item,false,false,false).is_err());
            assert!(prepare_record_for_process(&item,false,false,false,"fixture.exe").is_err());
        }
    }

    fn isolated_payload_root() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("native-clipboard-payload-")
            .tempdir_in(root)
            .unwrap()
    }

    fn rich_wire_xml(
        root: &str,
        image_code: u8,
        file_code: u8,
        image: &Path,
        file: &Path,
    ) -> String {
        let attribute = |path: &Path| {
            path.display()
                .to_string()
                .replace('&', "&amp;")
                .replace('\'', "&apos;")
        };
        format!("prefix-原样<?vendor keep?>\r\n<{root} vendor='keep &amp; raw'><Info version='1001' private='keep'/><!-- <EditElement type='3' filepath='fake.png'/> --><EditElement type='0' style='keep'><![CDATA[text </{root}> <EditElement type='3'/>]]><?text keep?></EditElement><EditElement type = '&#{};' filepath='{}' vendor='image'><Blob keep='image-body'/></EditElement><EditElement type='{file_code}' filepath='{}' vendor='file'><![CDATA[file-body]]></EditElement><EditElement type='2' vendor='placeholder'/><EditElement type='6' fileid='face-9'/><EditElement type='99'><Vendor><![CDATA[opaque]]></Vendor></EditElement><Vendor><EditElement type='3' filepath='nested-opaque.png'/></Vendor></{root}>suffix-完整", image_code + 48, attribute(image), attribute(file))
    }

    fn rich_record(raw: &str, source: &str) -> ClipRecord {
        let mut item = ClipRecord::new_text("原始 Unicode 🦀\r\nbody", source);
        item.kind = ClipKind::QQRichTextItem;
        item.qq_rich_text = Some(raw.to_owned());
        item.html_data = Some(crate::cf_html::build_clipboard_html(
            "<b>原始 HTML 🦀</b>",
            Some("https://example.test/source"),
        ));
        item
    }

    fn published_rich<'a>(payload: &'a Payload, format: &'static str) -> &'a str {
        let raw = bytes(payload, Format::Registered(format));
        assert_eq!(raw.last(), Some(&0));
        std::str::from_utf8(&raw[..raw.len() - 1]).unwrap()
    }

    fn assert_record_original_formats(
        item: &ClipRecord,
        payload: &Payload,
        rich_format: &'static str,
        rich: &str,
    ) {
        assert_eq!(
            bytes(payload, Format::Standard(13)),
            utf16(item.body_text.as_deref().unwrap())
        );
        assert_eq!(
            published_rich(payload, "HTML Format"),
            item.html_data.as_deref().unwrap()
        );
        assert_eq!(published_rich(payload, rich_format), rich);
        assert_eq!(payload.0.len(), 3);
    }

    #[test]
    fn native_record_payload_same_family_preserves_every_rich_byte_and_original_formats() {
        let dir = isolated_payload_root();
        let image = dir.path().join("图片 & 原样.png");
        let file = dir.path().join("文件's.txt");
        let image_bytes = b"original referenced image bytes";
        std::fs::write(&image, image_bytes).unwrap();
        std::fs::write(&file, b"original referenced file bytes").unwrap();
        for (root, image_code, file_code, source, targets, format) in [
            (
                "QQRichEditFormat",
                3,
                4,
                "qq.exe",
                vec!["qq.exe", "tim.exe", "chrome.exe", ""],
                "QQ_Unicode_RichEdit_Format",
            ),
            (
                "WeChatRichEditFormat",
                1,
                3,
                "wechat.exe",
                vec!["wechat.exe", "weixin.exe", "wxwork.exe"],
                "WeChat_RichEdit_Format",
            ),
        ] {
            let raw = rich_wire_xml(root, image_code, file_code, &image, &file);
            let item = rich_record(&raw, source);
            for target in targets {
                let payload = record_payload(&item, false, false, false, target).unwrap();
                assert_record_original_formats(&item, &payload, format, &raw);
                assert_eq!(item.qq_rich_text.as_deref(), Some(raw.as_str()));
            }
        }
        assert_eq!(std::fs::read(&image).unwrap(), image_bytes);
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"original referenced file bytes"
        );
    }

    #[test]
    fn native_record_payload_cross_family_patches_only_known_codes_and_root() {
        let dir = isolated_payload_root();
        let image = dir.path().join("图片 & 原样.png");
        let file = dir.path().join("文件's.txt");
        std::fs::write(&image, b"owned original image").unwrap();
        std::fs::write(&file, b"owned original file").unwrap();
        for (
            root,
            target_root,
            image_code,
            target_image,
            file_code,
            target_file,
            source,
            target,
            format,
        ) in [
            (
                "QQRichEditFormat",
                "WeChatRichEditFormat",
                3,
                1,
                4,
                3,
                "tim.exe",
                "wechat.exe",
                "WeChat_RichEdit_Format",
            ),
            (
                "WeChatRichEditFormat",
                "QQRichEditFormat",
                1,
                3,
                3,
                4,
                "wxwork.exe",
                "qq.exe",
                "QQ_Unicode_RichEdit_Format",
            ),
        ] {
            let raw = rich_wire_xml(root, image_code, file_code, &image, &file);
            let file_attribute = file
                .display()
                .to_string()
                .replace('&', "&amp;")
                .replace('\'', "&apos;");
            let expected = raw
                .replacen(
                    &format!("<{root} vendor="),
                    &format!("<{target_root} vendor="),
                    1,
                )
                .replacen(
                    &format!("</{root}>suffix-完整"),
                    &format!("</{target_root}>suffix-完整"),
                    1,
                )
                .replacen(
                    &format!("type = '&#{};'", image_code + 48),
                    &format!("type = '{target_image}'"),
                    1,
                )
                .replacen(
                    &format!("type='{file_code}' filepath='{file_attribute}' vendor='file'"),
                    &format!("type='{target_file}' filepath='{file_attribute}' vendor='file'"),
                    1,
                );
            let item = rich_record(&raw, source);
            let payload = record_payload(&item, false, false, false, target).unwrap();
            assert_record_original_formats(&item, &payload, format, &expected);
            assert_eq!(item.qq_rich_text.as_deref(), Some(raw.as_str()));
        }
        assert_eq!(std::fs::read(&image).unwrap(), b"owned original image");
        assert_eq!(std::fs::read(&file).unwrap(), b"owned original file");
    }

    #[test]
    fn native_record_payload_keeps_recorded_source_hint_and_xml_fallback() {
        let raw = "prefix<QQRichEditFormat vendor='legacy'><Info version='1001'/><EditElement type='1' filepath='image.png'/><EditElement type='3' filepath='file.txt'/><Vendor keep='raw'/></QQRichEditFormat>suffix";
        let item = rich_record(raw, "wxwork.exe");
        let same = record_payload(&item, false, false, false, "wechat.exe").unwrap();
        assert_record_original_formats(&item, &same, "WeChat_RichEdit_Format", raw);
        let cross = record_payload(&item, false, false, false, "qq.exe").unwrap();
        assert_record_original_formats(
            &item,
            &cross,
            "QQ_Unicode_RichEdit_Format",
            &raw.replacen(
                "type='1' filepath='image.png'",
                "type='3' filepath='image.png'",
                1,
            )
            .replacen(
                "type='3' filepath='file.txt'",
                "type='4' filepath='file.txt'",
                1,
            ),
        );

        let fallback = "prefix<QQRichEditFormat vendor='legacy'><EditElement type='1' filepath='image.png'/></QQRichEditFormat>suffix";
        let item = rich_record(fallback, "unknown-source.exe");
        let same = record_payload(&item, false, false, false, "wechat.exe").unwrap();
        assert_record_original_formats(&item, &same, "WeChat_RichEdit_Format", fallback);
        let cross = record_payload(&item, false, false, false, "qq.exe").unwrap();
        assert_record_original_formats(
            &item,
            &cross,
            "QQ_Unicode_RichEdit_Format",
            &fallback.replacen("type='1'", "type='3'", 1),
        );

        let qq_image_one = "prefix<QQRichEditFormat><Info version='1001'/><EditElement type='1' filepath='image.png'/></QQRichEditFormat>suffix";
        let item = rich_record(qq_image_one, "unknown-source.exe");
        let same = record_payload(&item, false, false, false, "qq.exe").unwrap();
        assert_record_original_formats(&item, &same, "QQ_Unicode_RichEdit_Format", qq_image_one);
    }

    #[test]
    fn native_record_payload_retains_malformed_raw_and_plain_preference_without_mutation() {
        for raw in [
            "prefix<QQRichEditFormat><EditElement type='3' filepath='image.png'></QQRichEditFormat>suffix",
            "prefix<QQRichEditFormat><EditElement type='0'><![CDATA[unterminated</QQRichEditFormat>suffix",
            "complete raw without an envelope <Vendor/> suffix",
        ] {
            let item = rich_record(raw, "qq.exe");
            for (target, format) in [("qq.exe", "QQ_Unicode_RichEdit_Format"), ("wechat.exe", "WeChat_RichEdit_Format")] {
                let payload = record_payload(&item, false, false, false, target).unwrap();
                assert_record_original_formats(&item, &payload, format, raw);
                let plain = record_payload(&item, true, false, false, target).unwrap();
                assert_eq!(plain.0, vec![(Format::Standard(13), utf16(item.body_text.as_deref().unwrap()))]);
                assert_eq!(item.qq_rich_text.as_deref(), Some(raw));
            }
        }
    }

    #[test]
    fn publication_failure_is_not_hidden_by_another_successful_format() {
        let formats = [13, 0xc000, 15]; // Unicode text, registered HTML, file drop.
        for failed in [None, Some(13), Some(0xc000), Some(15)] {
            let mut attempts = Vec::new();
            let result =
                publish_formats(formats.map(|format| (format, ())).to_vec(), |format, ()| {
                    attempts.push(format);
                    if failed == Some(format) {
                        Err("fixture write rejected".into())
                    } else {
                        Ok(())
                    }
                });
            if let Some(format) = failed {
                let error =
                    result.expect_err("a partial clipboard must not be reported as ready to paste");
                assert!(error.contains("fixture write rejected"));
                assert!(
                    error.contains(&format.to_string()),
                    "failed format must be identified: {error}"
                );
                let index = formats.iter().position(|id| *id == format).unwrap();
                assert_eq!(
                    attempts,
                    formats[..=index],
                    "stop publishing after a failed format"
                );
            } else {
                result.unwrap();
                assert_eq!(attempts, formats);
            }
        }
    }

    #[test]
    fn partial_publication_stops_history_paste_and_allows_retry() {
        use serde_json::{json, Value};
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        };
        crate::clipboard::force_headless(true);
        let dir = isolated_payload_root();
        let host = crate::HostState::open(
            crate::AppPaths::from_data_root(dir.path().to_path_buf()),
            28965,
        )
        .unwrap();
        let id = host
            .add_text(&json!({"text": "publication retry"}))
            .unwrap()["createdIds"][0]
            .clone();
        let fail = Arc::new(AtomicBool::new(true));
        let reject = fail.clone();
        host.set_clipboard_ui_runner(move |_| {
            publish_formats(vec![(13, ()), (0xc000, ())], |format, ()| {
                if format == 0xc000 && reject.load(Ordering::SeqCst) {
                    Err("fixture HTML publication failed".into())
                } else {
                    Ok(())
                }
            })
        });
        let inputs = Arc::new(AtomicUsize::new(0));
        let seen = inputs.clone();
        host.set_input_runner(move |job| {
            seen.fetch_add(1, Ordering::SeqCst);
            job()
        });
        let usage = || {
            let detail = host.detail(&json!({"id": id})).unwrap();
            serde_json::from_str::<Value>(detail["rawJson"].as_str().unwrap()).unwrap()
                ["usageCount"]
                .clone()
        };
        assert!(host.paste_item(&json!({"id": id})).is_err());
        assert_eq!(
            inputs.load(Ordering::SeqCst),
            1,
            "only activation, no paste after partial publication"
        );
        assert_eq!(usage(), 0);
        fail.store(false, Ordering::SeqCst);
        inputs.store(0, Ordering::SeqCst);
        assert_eq!(host.paste_item(&json!({"id": id})).unwrap()["ok"], true);
        assert_eq!(
            inputs.load(Ordering::SeqCst),
            2,
            "retry activates and pastes once"
        );
        assert_eq!(usage(), 1);
    }

    fn bytes(payload: &Payload, format: Format) -> &[u8] {
        &payload
            .0
            .iter()
            .find(|entry| entry.0 == format)
            .expect("format missing")
            .1
    }

    fn image_record(path: &Path) -> ClipRecord {
        let mut item = ClipRecord::new_text("", "payload-test");
        item.kind = ClipKind::ImageClipItem;
        item.image_path = Some(path.display().to_string());
        item
    }

    #[test]
    fn quicker_json_copy_writes_only_the_action_id_and_rejects_invalid_records() {
        let mut item =
            ClipRecord::new_text(r#"{"Id":"clipboard-action","Title":"复制动作"}"#, "fixture");
        item.kind = ClipKind::ActionClipItem;
        for plain in [false, true] {
            let payload = record_payload(&item, plain, true, true, "").unwrap();
            assert_eq!(
                payload.0,
                vec![(Format::Standard(13), utf16("clipboard-action"))]
            );
        }
        crate::clipboard::force_headless(true);
        for invalid in [
            "",
            r#"{"Title":"missing ID"}"#,
            "{broken JSON",
            r#"{"Id":"bad\u0000id"}"#,
        ] {
            item.body_text = Some(invalid.into());
            assert!(record_payload(&item, false, false, false, "").is_err());
            assert!(prepare_record(&item, false, false, false).is_err());
        }
    }

    #[test]
    fn html_plain_text_and_steps_preserve_their_wire_encodings() {
        let text = "中文 🦀\r\nsecond line";
        let mut item = ClipRecord::new_text(text, "payload-test");
        item.kind = ClipKind::HtmlClipItem;
        item.html_data = Some("<b>中文 🦀</b>".into());
        let rich = record_payload(&item, false, false, false, "chrome.exe").unwrap();
        assert_eq!(bytes(&rich, Format::Standard(13)), utf16(text));
        let html = bytes(&rich, Format::Registered("HTML Format"));
        assert_eq!(html.last(), Some(&0));
        let html = std::str::from_utf8(&html[..html.len() - 1]).unwrap();
        assert!(crate::cf_html::looks_like_complete_format(html));
        assert!(html.contains("<b>中文 🦀</b>"));
        let plain = record_payload(&item, true, false, false, "chrome.exe").unwrap();
        assert_eq!(plain.0, vec![(Format::Standard(13), utf16(text))]);

        item.kind = ClipKind::StepClipItem;
        let steps = record_payload(&item, false, false, false, "").unwrap();
        assert_eq!(
            steps.0,
            vec![(Format::Registered("quicker-action-steps"), utf16(text))]
        );
        assert_eq!(
            record_payload(&item, true, false, false, "").unwrap().0,
            steps.0
        );
    }

    #[test]
    fn html_preservation_paste_keeps_complete_wps_bytes_without_rebuilding() {
        let raw = format!("\u{feff}{}", crate::cf_html::build_clipboard_html("<html xmlns:wps='urn:wps'><head><style>.xl1{color:red}</style></head><body><table class='xl1'><tr><td>中文😀</td></tr></table></body></html>", Some("https://source.test/")));
        let mut item = ClipRecord::new_text("中文😀", "wps.exe");
        item.kind = ClipKind::HtmlClipItem;
        item.html_data = Some(raw.clone());
        item.html_source_url = Some("https://chromium-metadata.test/".into());
        let payload = record_payload(&item, false, false, false, "wps.exe").unwrap();
        assert_eq!(bytes(&payload, Format::Registered("HTML Format")), [raw.as_bytes(), &[0]].concat());
        item.html_data = Some(String::new());
        let cleared = record_payload(&item, false, false, false, "wps.exe").unwrap();
        assert_eq!(cleared.0, vec![(Format::Standard(13), utf16("中文😀"))],
            "Explicit HTML clear still published an empty rich document ahead of valid plain text");
    }

    #[test]
    fn im_rich_text_uses_the_target_app_format() {
        let mut item = ClipRecord::new_text("rich text", "payload-test");
        item.kind = ClipKind::QQRichTextItem;
        item.qq_rich_text = Some("<QQRichEditFormat><Info version=\"1001\"/><EditElement type=\"0\"><![CDATA[中文]]></EditElement></QQRichEditFormat>".into());
        for (process, expected, absent) in [
            (
                "wechat.exe",
                "WeChat_RichEdit_Format",
                "QQ_Unicode_RichEdit_Format",
            ),
            (
                "qq.exe",
                "QQ_Unicode_RichEdit_Format",
                "WeChat_RichEdit_Format",
            ),
        ] {
            let payload = record_payload(&item, false, false, false, process).unwrap();
            assert!(
                std::str::from_utf8(bytes(&payload, Format::Registered(expected)))
                    .unwrap()
                    .contains("中文")
            );
            assert!(!payload
                .0
                .iter()
                .any(|entry| entry.0 == Format::Registered(absent)));
        }
    }

    #[test]
    fn file_payload_filters_missing_paths_and_serializes_wide_dropfiles() {
        let dir = isolated_payload_root();
        let path = dir.path().join("中文 🦀.txt");
        std::fs::write(&path, "fixture").unwrap();
        let missing = dir.path().join("missing.txt").display().to_string();
        let mut item = ClipRecord::new_text("", "payload-test");
        item.kind = ClipKind::FileClipItem;
        item.file_paths = vec![
            missing.clone(),
            dir.path().display().to_string(),
            path.display().to_string(),
        ];
        let payload = record_payload(&item, false, false, false, "").unwrap();
        let drop = bytes(&payload, Format::Standard(15));
        #[cfg(windows)]
        assert_eq!(
            std::mem::size_of::<windows::Win32::UI::Shell::DROPFILES>(),
            20
        );
        assert_eq!(&drop[..4], &20u32.to_le_bytes());
        assert!(drop[4..16].iter().all(|byte| *byte == 0));
        assert_eq!(&drop[16..20], &1u32.to_le_bytes());
        assert_eq!(
            &drop[20..],
            [utf16(&path.display().to_string()), vec![0, 0]].concat()
        );
        item.file_paths = vec![missing.clone()];
        assert!(record_payload(&item, false, false, false, "").is_err());
        assert!(text_and_files_payload("", &[missing.clone()]).is_err());
        let fallback = text_and_files_payload("keep text", &[missing]).unwrap();
        assert_eq!(fallback.0, vec![(Format::Standard(13), utf16("keep text"))]);
    }

    #[test]
    fn image_payload_keeps_a_consistent_png_and_bottom_up_dib_snapshot() {
        let dir = isolated_payload_root();
        let path = dir.path().join("image.png");
        let mut image = image::RgbaImage::new(1, 2);
        image.put_pixel(0, 0, image::Rgba([10, 20, 30, 255]));
        image.put_pixel(0, 1, image::Rgba([40, 50, 60, 128]));
        image.save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let payload =
            record_payload(&image_record(&path), false, false, false, "chrome.exe").unwrap();
        // Changing the file after preparation must not change either bitmap format.
        std::fs::write(&path, "replaced after preparation").unwrap();
        assert_eq!(bytes(&payload, Format::Registered("PNG")), original);
        let dib = bytes(&payload, Format::Standard(8));
        assert_eq!(&dib[..4], &40u32.to_le_bytes());
        assert_eq!(&dib[4..8], &1i32.to_le_bytes());
        assert_eq!(&dib[8..12], &2i32.to_le_bytes());
        assert_eq!(&dib[40..], &[60, 50, 40, 128, 30, 20, 10, 255]);
        assert_eq!(payload.0.len(), 3);
    }

    #[test]
    fn image_payload_preserves_app_routing_and_invalid_bitmap_file_fallback() {
        let dir = isolated_payload_root();
        let path = dir.path().join("image.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let mut item = image_record(&path);
        for process in ["explorer.exe", "winword.exe", "powerpnt.exe"] {
            let payload = record_payload(&item, false, false, false, process).unwrap();
            assert_eq!(payload.0.len(), 1);
            assert_eq!(payload.0[0].0, Format::Standard(15));
        }
        for process in [
            "qq.exe",
            "wechat.exe",
            "weixin.exe",
            "tim.exe",
            "wxwork.exe",
        ] {
            let payload = record_payload(&item, false, false, false, process).unwrap();
            assert_eq!(payload.0.len(), 1);
            assert_eq!(
                payload.0[0].0,
                Format::Registered("QQ_Unicode_RichEdit_Format")
            );
        }
        item.kind = ClipKind::FileClipItem;
        item.file_paths = vec![path.display().to_string()];
        assert_eq!(
            record_payload(&item, false, false, false, "chrome.exe")
                .unwrap()
                .0
                .len(),
            3
        );
        std::fs::write(&path, "invalid bitmap").unwrap();
        let fallback = record_payload(&item, false, false, false, "chrome.exe").unwrap();
        assert_eq!(fallback.0.len(), 1);
        assert_eq!(fallback.0[0].0, Format::Standard(15));
    }

    #[test]
    fn default_plain_text_paste_preserves_non_text_payloads_for_all_targets() {
        let dir = isolated_payload_root();
        let path = dir.path().join("image.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .unwrap();
        let mut item = image_record(&path);
        for kind in [ClipKind::ImageClipItem, ClipKind::FileClipItem] {
            item.kind = kind;
            item.file_paths = vec![path.display().to_string()];
            for process in [
                "chrome.exe",
                "qq.exe",
                "wechat.exe",
                "explorer.exe",
                "winword.exe",
            ] {
                let normal = record_payload(&item, false, false, false, process).unwrap();
                let plain_default = record_payload(&item, true, false, false, process).unwrap();
                assert_eq!(plain_default.0, normal.0, "{kind:?} -> {process}");
                assert!(!plain_default.0.is_empty());
            }
        }
        let file = dir.path().join("document.txt");
        std::fs::write(&file, "fixture").unwrap();
        item.file_paths = vec![file.display().to_string()];
        let payload = record_payload(&item, true, false, false, "chrome.exe").unwrap();
        assert_eq!(payload.0[0].0, Format::Standard(15));

        // The preference still strips both kinds of rich text to Unicode.
        item = ClipRecord::new_text("plain text", "payload-test");
        item.html_data = Some("<b>plain text</b>".into());
        item.qq_rich_text = Some("<QQRichEditFormat/>".into());
        for kind in [ClipKind::HtmlClipItem, ClipKind::QQRichTextItem] {
            item.kind = kind;
            assert_eq!(
                record_payload(&item, true, false, false, "qq.exe")
                    .unwrap()
                    .0,
                vec![(Format::Standard(13), utf16("plain text"))]
            );
        }
    }

    #[test]
    fn empty_or_missing_image_payload_is_rejected_before_commit() {
        let dir = isolated_payload_root();
        let path = dir.path().join("image.png");
        let item = image_record(&path);
        assert!(record_payload(&item, false, false, false, "").is_err());
        image::RgbaImage::new(1, 1).save(&path).unwrap();
        assert!(!record_payload(&item, true, false, false, "")
            .unwrap()
            .0
            .is_empty());
        assert!(text_and_files_payload("", &[]).is_err());
        assert!(Payload::default().prepare().is_err());
    }
}

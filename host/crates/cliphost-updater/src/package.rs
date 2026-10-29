use crate::{invalid, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

pub(crate) const MAX_ARCHIVE: u64 = 512 * 1024 * 1024;
const MAX_EXPANDED: u64 = 1024 * 1024 * 1024;
const MAX_MANIFEST: u64 = 1024 * 1024;
pub(crate) const EXE: &str = "cliphost-app.exe";
pub(crate) const MANIFEST: &str = "build-info.json";
pub(crate) const METADATA: [&str; 3] = [MANIFEST, "SHA256SUMS.txt", "README.txt"];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct Version([u32; 4]);
impl Version {
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let fields: Vec<_> = text.split('.').collect();
        if !(fields.len() == 3 || fields.len() == 4) {
            return Err(invalid("版本必须是三段或四段数字"));
        }
        let mut value = [0; 4];
        for (i, field) in fields.iter().enumerate() {
            if field.is_empty()
                || !field.bytes().all(|b| b.is_ascii_digit())
                || (field.len() > 1 && field.starts_with('0'))
            {
                return Err(invalid("版本包含非法字符"));
            }
            value[i] = field.parse().map_err(|_| invalid("版本数字过大"))?;
        }
        Ok(Self(value))
    }
    pub(crate) fn parse_release(text: &str) -> Result<Self> {
        if text.split('.').count() != 3 {
            return Err(invalid("正式发布版本必须为三段数字"));
        }
        Self::parse(text)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PayloadFile {
    pub name: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Manifest {
    pub product: String,
    pub version: String,
    pub executable: String,
    pub sha256: String,
    pub renderer: String,
    pub profile: String,
    pub architecture: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub local_deployment: bool,
    pub files: Vec<PayloadFile>,
}

pub(crate) fn valid_hash(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|c| c.is_ascii_hexdigit())
}

pub(crate) fn safe_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 100
        || name.ends_with('.')
        || name.ends_with(' ')
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
    {
        return false;
    }
    matches!(
        name,
        EXE | "Start-ClipHostDiagnostics.ps1" | "Start-ClipHostDiagnostics.cmd"
    ) || {
        let n = name.to_ascii_lowercase();
        n == name
            && (n.starts_with("vcruntime140")
                || n.starts_with("msvcp140")
                || n == "concrt140.dll"
                || n == "vccorlib140.dll")
            && n.ends_with(".dll")
    }
}

pub(crate) fn validate_manifest(
    info: &Manifest,
    expected_version: &str,
    incoming: bool,
) -> Result<()> {
    if info.product != "ClipHost Rust"
        || info.executable != EXE
        || info.renderer != "gpui"
        || info.profile != "release"
        || info.architecture != "x64"
        || Version::parse(&info.version)? != Version::parse(expected_version)?
        || (info.target.as_deref() != Some("x86_64-pc-windows-msvc")
            && (incoming || !info.local_deployment))
    {
        return Err(invalid("更新包的产品、版本、架构或Release身份不匹配"));
    }
    if incoming && info.local_deployment {
        return Err(invalid("不能分发本机临时部署包"));
    }
    if !valid_hash(&info.sha256) || info.files.is_empty() || info.files.len() > 64 {
        return Err(invalid("载荷清单无效"));
    }
    let mut names = HashSet::new();
    let mut size = 0_u64;
    for file in &info.files {
        if !safe_name(&file.name)
            || !valid_hash(&file.sha256)
            || !names.insert(file.name.to_ascii_lowercase())
            || file.size_bytes == 0
            || file.size_bytes > MAX_ARCHIVE
        {
            return Err(invalid("载荷包含重复、未知或不安全文件"));
        }
        size = size
            .checked_add(file.size_bytes)
            .ok_or_else(|| invalid("载荷过大"))?;
    }
    if size > MAX_EXPANDED || !names.contains(EXE) || !names.contains("vcruntime140.dll") {
        return Err(invalid("载荷过大或缺少程序运行文件"));
    }
    let executable = info
        .files
        .iter()
        .find(|f| f.name == EXE)
        .ok_or_else(|| invalid("缺少程序文件"))?;
    if !executable.sha256.eq_ignore_ascii_case(&info.sha256) {
        return Err(invalid("主程序摘要不匹配"));
    }
    Ok(())
}

pub(crate) fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    reject_links(path)?;
    let file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > limit {
        return Err(invalid("文件类型或大小不符合预期"));
    }
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err(invalid("文件超过大小限制"));
    }
    Ok(data)
}

pub(crate) fn read_manifest(root: &Path) -> Result<Manifest> {
    let bytes = read_limited(&root.join(MANIFEST), MAX_MANIFEST)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("程序清单不是UTF-8"))?
        .trim_start_matches('\u{feff}');
    Ok(serde_json::from_str(text)?)
}

pub(crate) fn reject_links(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(invalid("更新路径必须为绝对路径"));
    }
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor)?;
        if meta.file_type().is_symlink() {
            return Err(invalid("更新路径不能使用符号链接"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
                return Err(invalid("更新路径不能使用重解析点或云端占位文件"));
            }
        }
    }
    Ok(())
}

pub(crate) fn hash_file(path: &Path) -> Result<String> {
    reject_links(path)?;
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_ARCHIVE {
        return Err(invalid("程序文件大小无效"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(crate) fn verify_file(root: &Path, file: &PayloadFile) -> Result<()> {
    let path = root.join(&file.name);
    reject_links(&path)?;
    if fs::metadata(&path)?.len() != file.size_bytes
        || !hash_file(&path)?.eq_ignore_ascii_case(&file.sha256)
    {
        return Err(invalid(format!("程序文件校验失败：{}", file.name)));
    }
    Ok(())
}

pub(crate) fn verify_payload(root: &Path, info: &Manifest) -> Result<()> {
    reject_links(root)?;
    for file in &info.files {
        verify_file(root, file)?;
        if file.name.to_ascii_lowercase().ends_with(".dll") || file.name == EXE {
            verify_x64(&root.join(&file.name))?;
        }
    }
    Ok(())
}

fn verify_x64(exe: &Path) -> Result<()> {
    let mut file = File::open(exe)?;
    let mut dos = [0_u8; 64];
    file.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Err(invalid("主程序不是Windows可执行文件"));
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if offset > file.metadata()?.len().saturating_sub(26) {
        return Err(invalid("PE文件头无效"));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut pe = [0_u8; 26];
    file.read_exact(&mut pe)?;
    if &pe[..4] != b"PE\0\0"
        || u16::from_le_bytes([pe[4], pe[5]]) != 0x8664
        || u16::from_le_bytes([pe[24], pe[25]]) != 0x20b
    {
        return Err(invalid("主程序不是x64原生版本"));
    }
    Ok(())
}

pub(crate) fn validate_installed(executable: &Path, version: &str) -> Result<Manifest> {
    reject_links(executable)?;
    if executable.file_name().and_then(|n| n.to_str()) != Some(EXE) {
        return Err(invalid("只支持完整正式安装版或便携版"));
    }
    let root = executable
        .parent()
        .ok_or_else(|| invalid("无法识别安装目录"))?;
    let info = read_manifest(root)?;
    validate_manifest(&info, version, false)?;
    verify_payload(root, &info)?;
    Ok(info)
}

pub(crate) fn extract_archive(
    archive: &Path,
    destination: &Path,
    version: &str,
) -> Result<Manifest> {
    if fs::metadata(archive)?.len() > MAX_ARCHIVE {
        return Err(invalid("压缩包过大"));
    }
    reject_links(archive)?;
    let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
    if zip.len() == 0 || zip.len() > 72 {
        return Err(invalid("压缩包条目数量异常"));
    }
    let mut seen = HashSet::new();
    let mut root_seen = false;
    let mut expanded = 0_u64;
    // Validate every central-directory entry before writing any extracted bytes.
    for index in 0..zip.len() {
        let entry = zip.by_index(index)?;
        if entry.name() == "ClipHost-Rust/" && entry.is_dir() {
            if root_seen {
                return Err(invalid("压缩包根目录重复"));
            }
            root_seen = true;
            continue;
        }
        let name = entry
            .name()
            .strip_prefix("ClipHost-Rust/")
            .ok_or_else(|| invalid("压缩包根目录不匹配"))?;
        if entry.is_dir()
            || (!safe_name(name) && !METADATA.contains(&name))
            || !seen.insert(name.to_ascii_lowercase())
            || entry
                .unix_mode()
                .is_some_and(|m| !matches!(m & 0o170000, 0 | 0o100000))
            || entry.size() == 0
            || entry.size() > MAX_ARCHIVE
            || entry.size() > entry.compressed_size().max(1).saturating_mul(200)
        {
            return Err(invalid("压缩包包含链接、重复文件、越界路径或异常大小"));
        }
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| invalid("压缩包展开过大"))?;
        if expanded > MAX_EXPANDED {
            return Err(invalid("压缩包展开过大"));
        }
    }
    let info: Manifest = {
        let mut entry = zip.by_name("ClipHost-Rust/build-info.json")?;
        if entry.size() > MAX_MANIFEST {
            return Err(invalid("更新包清单过大"));
        }
        let mut bytes = Vec::new();
        (&mut entry)
            .take(MAX_MANIFEST + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MANIFEST {
            return Err(invalid("更新包清单过大"));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| invalid("清单编码无效"))?
            .trim_start_matches('\u{feff}');
        serde_json::from_str(text)?
    };
    validate_manifest(&info, version, true)?;
    let allowed: HashSet<_> = info
        .files
        .iter()
        .map(|f| f.name.to_ascii_lowercase())
        .chain(METADATA.iter().map(|n| n.to_ascii_lowercase()))
        .collect();
    if seen != allowed {
        return Err(invalid("压缩包文件与发布清单不一致"));
    }
    fs::create_dir(destination)?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry
            .name()
            .strip_prefix("ClipHost-Rust/")
            .unwrap()
            .to_string();
        let size = entry.size();
        let expected = info
            .files
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.size_bytes);
        if expected.is_some_and(|n| n != size)
            || (METADATA.contains(&name.as_str()) && size > MAX_MANIFEST)
        {
            return Err(invalid("压缩包文件大小与清单不匹配"));
        }
        let mut output = File::options()
            .write(true)
            .create_new(true)
            .open(destination.join(name))?;
        let copied = std::io::copy(&mut (&mut entry).take(size + 1), &mut output)?;
        if copied != size {
            return Err(invalid("压缩包实际解压大小异常"));
        }
        output.flush()?;
        output.sync_all()?;
    }
    verify_payload(destination, &info)?;
    Ok(info)
}

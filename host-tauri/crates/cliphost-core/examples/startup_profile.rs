//! Seed large isolated histories and measure store/host startup and list paging
//! in-process, without a window, clipboard monitor or the user's data.
//!
//! startup_profile <empty dir under workspace .tmp> [--template <isolated C# data root>]
//!     [--counts 0,1000,10000,20000] [--repeat 5]
//!
//! `--template` is an isolated data root in which the C#/WPF host created an empty
//! database; it is copied whole and seeding then goes through the in-place
//! document store (the format the Rust host reuses from C#/WPF), so the dataset
//! also opens in that host. Without it only the native format is seeded.
//! Datasets are left under `<root>/datasets/<format>-<count>` for desktop runs.
use chrono::{Duration, TimeZone, Utc};
use cliphost_core::{
    clipboard,
    host::HostState,
    model::{ClipKind, ClipRecord},
    paths::AppPaths,
    store::AppStore,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    let n = samples.len();
    if n % 2 == 1 { samples[n / 2] } else { (samples[n / 2 - 1] + samples[n / 2]) / 2.0 }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Deterministic mix close to real history: mostly short text, some long text,
/// HTML, files, links and small images; about 1% pinned.
fn record(index: usize, image_root: &Path) -> Result<ClipRecord, Box<dyn std::error::Error>> {
    let time = Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap();
    let line = "剪贴板 perf 测试 clipboard history line with some words; ";
    let body = match index % 10 {
        0..=5 => format!("短文本 {index} {}", &line[..line.len().min(40 + index % 40)]),
        6 | 7 => format!("中等文本 {index} needle {}", line.repeat(6 + index % 6)),
        _ => format!("长文本 {index} {}", line.repeat(60 + index % 80)),
    };
    let mut item = ClipRecord::new_text(body.clone(), ["chrome.exe", "Code.exe", "WeChat.exe", "explorer.exe"][index % 4]);
    match index % 20 {
        14 | 15 => {
            item.kind = ClipKind::HtmlClipItem;
            item.html_data = Some(format!("<html><body><p><b>{index}</b> {}</p></body></html>", line.repeat(20)));
        }
        16 => {
            item.kind = ClipKind::FileClipItem;
            item.body_text = None;
            item.file_paths = (0..1 + index % 3).map(|n| format!("C:\\PerfFixture\\dir{}\\file-{index}-{n}.txt", index % 50)).collect();
            item.content_hash = cliphost_core::store::hash_file_list(&item.file_paths);
        }
        17 => {
            item.kind = ClipKind::ImageClipItem;
            item.body_text = None;
            let name = format!("Perf/img-{index:06}.png");
            let shade = (index * 37 % 255) as u8;
            image::RgbImage::from_fn(96, 64, |x, y| image::Rgb([shade, (x * 2) as u8, (y * 3) as u8]))
                .save(image_root.join(&name))?;
            item.image_path = Some(name);
            item.content_hash = format!("perf-image-{index}");
        }
        18 => {
            item.kind = ClipKind::LinkClipItem;
            item.body_text = Some(format!("https://example.com/perf/{index}?q=needle"));
        }
        _ => {}
    }
    item.id = format!("perf{index:08}");
    item.create_time = time - Duration::seconds(index as i64 * 7);
    item.update_time = item.create_time;
    item.is_top = index % 100 == 3;
    if index % 5 == 0 {
        item.last_access = Some(item.create_time + Duration::minutes(3));
        item.usage_count = (index % 9) as i32;
    }
    if item.kind == ClipKind::TextClipItem {
        item.content_hash = format!("perf-text-{index}");
    }
    Ok(item)
}

fn seed(dataset: &Path, template: Option<&Path>, count: usize) -> Result<f64, Box<dyn std::error::Error>> {
    if let Some(template) = template {
        copy_dir(template, dataset)?;
    }
    let paths = AppPaths::from_data_root(dataset.to_path_buf());
    let images = paths.image_library.join("Perf");
    fs::create_dir_all(&images)?;
    fs::create_dir_all(&paths.configs)?;
    // Retention must not prune the fixture: both hosts read PascalCase keys.
    fs::write(paths.configs.join("ClipConfig.json"), r#"{"MaxItemSize":200000}"#)?;
    let records = (0..count).map(|i| record(i, &paths.image_library)).collect::<Result<Vec<_>, _>>()?;
    let started = Instant::now();
    let store = AppStore::open(&paths.db)?;
    for chunk in records.chunks(2_000) {
        store.insert_clips(chunk)?;
    }
    let seeded = ms(started);
    let (unpinned, pinned) = store.counts()?;
    if (unpinned + pinned) as usize != count {
        return Err(format!("seeded {} of {count} records", unpinned + pinned).into());
    }
    Ok(seeded)
}

fn page(host: &HostState, skip: usize, query: &str) -> Result<(f64, Value), String> {
    let request = json!({"webSessionId": "startup-profile", "skip": skip, "take": 100, "query": query});
    let started = Instant::now();
    let result = host.search_items(&request)?;
    Ok((ms(started), result))
}

fn measure(dataset: &Path, work: &Path, count: usize) -> Result<Value, Box<dyn std::error::Error>> {
    // Each sample gets an untouched copy: opening may add tables or migrate configs.
    copy_dir(dataset, work)?;
    let paths = AppPaths::from_data_root(work.to_path_buf());
    let started = Instant::now();
    let store = AppStore::open(&paths.db)?;
    let store_open = ms(started);
    drop(store);
    let started = Instant::now();
    let host = HostState::open(paths.clone(), 1)?;
    let host_open = ms(started);
    let (first_page, first) = page(&host, 0, "")?;
    if first["total"].as_u64() != Some(count as u64) {
        return Err(format!("list total {} != {count}", first["total"]).into());
    }
    let (second_page, _) = page(&host, 100, "")?;
    let (deep_page, _) = page(&host, count.saturating_sub(100), "")?;
    let (query, _) = page(&host, 0, "needle")?;
    let started = Instant::now();
    host.add_text(&json!({"text": format!("capture after {count} records")}))?;
    let capture = ms(started);
    Ok(json!({"storeOpenMs": store_open, "hostOpenMs": host_open, "firstPageMs": first_page,
        "secondPageMs": second_page, "deepPageMs": deep_page, "queryNeedleMs": query,
        "captureOneMs": capture}))
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().ok_or("pass an empty directory under workspace .tmp")?).canonicalize()?;
    let temporary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp").canonicalize()?;
    if root == temporary || !root.starts_with(&temporary) || root.read_dir()?.next().is_some() {
        return Err("output must be an empty dedicated directory under workspace .tmp".into());
    }
    let (mut template, mut counts, mut repeat) = (None, vec![0, 1_000, 10_000, 20_000], 5usize);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--template" => template = Some(PathBuf::from(value).canonicalize()?),
            "--counts" => counts = value.split(',').map(str::parse).collect::<Result<_, _>>()?,
            "--repeat" => repeat = value.parse()?,
            _ => return Err(format!("unknown flag {flag}").into()),
        }
    }
    clipboard::force_headless(true);
    let mut formats = vec![("native", None)];
    if let Some(template) = template.as_deref() {
        formats.push(("csharp", Some(template)));
    }
    let mut scenarios = Vec::new();
    for (format, template) in formats {
        for &count in &counts {
            let dataset = root.join("datasets").join(format!("{format}-{count}"));
            let seed_ms = seed(&dataset, template, count)?;
            let db_bytes = fs::metadata(dataset.join("userdata.db"))?.len();
            let samples = (0..repeat)
                .map(|r| measure(&dataset, &root.join("work").join(format!("{format}-{count}-{r}")), count))
                .collect::<Result<Vec<_>, _>>()?;
            let mut medians = serde_json::Map::new();
            for key in samples[0].as_object().unwrap().keys() {
                let mut values: Vec<f64> = samples.iter().filter_map(|s| s[key].as_f64()).collect();
                if !values.is_empty() {
                    medians.insert(key.clone(), json!((median(&mut values) * 100.0).round() / 100.0));
                }
            }
            let row = json!({"format": format, "count": count, "dbBytes": db_bytes, "seedMs": seed_ms,
                "median": medians, "samples": samples});
            println!("{}", json!({"format": format, "count": count, "dbMB": db_bytes as f64 / 1048576.0, "median": row["median"]}));
            scenarios.push(row);
        }
    }
    // Background host threads may still hold the last copy open; it is only scratch.
    let _ = fs::remove_dir_all(root.join("work"));
    let report = json!({"profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "repeat": repeat, "fileCache": "warm (dataset written just before)", "scenarios": scenarios});
    fs::write(root.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

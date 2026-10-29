//! Measure actual host search on deterministic data, without a window or clipboard.
#![cfg_attr(windows, windows_subsystem = "windows")]

use chrono::{Duration, TimeZone, Utc};
use cliphost_core::{
    clipboard, host::HostState, model::ClipRecord, paths::AppPaths, store::AppStore,
};
use serde_json::{json, Value};
use std::{fs, path::Path, time::Instant};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args()
        .nth(1)
        .ok_or("pass a new workspace .tmp output directory")?;
    let root = Path::new(&root).canonicalize()?;
    let temporary = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.tmp")
        .canonicalize()?;
    if root == temporary || !root.starts_with(&temporary) || root.read_dir()?.next().is_some() {
        return Err("output must be an empty dedicated directory under workspace .tmp".into());
    }
    clipboard::force_headless(true);
    let time = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
    let padding = "项目资料 background notes for the clipboard. ".repeat(24);
    let mut results = Vec::new();
    for count in [100, 1_000] {
        let paths = AppPaths::from_data_root(root.join(format!("data-{count}")));
        {
            let store = AppStore::open(&paths.db)?;
            let records = (0..count)
                .map(|index| {
                    let (title, body) = match index % 4 {
                        0 => (format!("needle {index}"), format!("needle {padding}")),
                        1 => (format!("笔记 {index} needle"), format!("{padding} needle")),
                        2 => (format!("文件记录 {index}"), format!("文件说明 {padding}")),
                        _ => (format!("项目 {index}"), format!("{padding} needle 文件")),
                    };
                    let mut item = ClipRecord::new_text(body, "search-profile");
                    item.id = format!("fixture-{index:06}");
                    item.title = Some(title);
                    item.create_time = time - Duration::seconds((index * 37 % 97) as i64);
                    item.update_time = item.create_time;
                    item.last_access = Some(time + Duration::seconds((index * 23 % 79) as i64));
                    item.is_top = index % 7 == 0;
                    item.sub_index = index % 3;
                    item
                })
                .collect::<Vec<_>>();
            store.insert_clips(&records)?;
        }
        let host = HostState::open(paths, 28965)?;
        for query in ["", "needle", "wenjian"] {
            for sort in [0, 1] {
                let request =
                    json!({"webSessionId":"profile", "query":query, "sortType":sort, "take":50});
                let warmup = host.search_items(&request)?;
                let baseline = serde_json::to_vec(&warmup)?;
                let mut samples = Vec::new();
                for _ in 0..3 {
                    let start = Instant::now();
                    let result = host.search_items(&request)?;
                    samples.push(start.elapsed().as_secs_f64() * 1000.0);
                    assert_eq!(
                        serde_json::to_vec(&result)?,
                        baseline,
                        "search changed without a mutation"
                    );
                }
                samples.sort_by(f64::total_cmp);
                let row = json!({"count":count, "query":query, "sortType":sort, "medianMs":samples[1],
                    "samplesMs":samples, "total":warmup["total"], "responseHash":blake3::hash(&baseline).to_hex().to_string()});
                println!("{row}");
                results.push(row);
            }
        }
    }
    let report: Value = json!({"profile":"release", "nativeInputUsed":false, "scenarios":results});
    fs::write(
        root.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

//! Compare normal quick-paste candidate loading without a window or clipboard.
//! Pass an existing empty directory under workspace .tmp as the sole argument.
#![cfg_attr(windows, windows_subsystem = "windows")]

use chrono::{Duration, TimeZone, Utc};
use cliphost_core::{
    clipboard,
    model::ClipRecord,
    quick_paste::select_session_items,
    store::{AppStore, StoreError},
};
use serde_json::json;
use std::{fs, path::Path, time::Instant};

const LIMIT: i32 = 25;
const SAMPLES: usize = 8;
const PINNED: i32 = 5;

fn measure(
    query: impl FnOnce() -> Result<Vec<ClipRecord>, StoreError>,
) -> Result<(Vec<ClipRecord>, f64), StoreError> {
    let start = Instant::now();
    let result = query()?;
    Ok((result, start.elapsed().as_secs_f64() * 1_000.0))
}

fn median(samples: &[f64]) -> f64 {
    (samples[SAMPLES / 2 - 1] + samples[SAMPLES / 2]) / 2.0
}

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
    let time = Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap();
    let padding = "0123456789abcdef".repeat(64);
    let mut results = Vec::new();
    for count in [100_i32, 1_000] {
        let data = root.join(format!("data-{count}"));
        fs::create_dir_all(&data)?;
        let db = data.join("userdata.db");
        {
            let store = AppStore::open(&db)?;
            let records = (0..count)
                .map(|index| {
                    let mut item = ClipRecord::new_text(
                        format!("record {index:04}: {padding}"),
                        "quick-paste-profile",
                    );
                    item.id = format!("fixture-{index:06}");
                    // Same indexed millisecond, fractional date, and SubIndex
                    // ties exercise the exact boundary ordering at LIMIT.
                    item.create_time = time - Duration::milliseconds((index / 8) as i64)
                        + Duration::nanoseconds(((index % 8) / 3) as i64 * 100_000);
                    item.update_time = item.create_time;
                    item.sub_index = index % 3;
                    item.is_top = index >= count - PINNED;
                    item.last_access = Some(time + Duration::seconds((index % 17) as i64));
                    item.usage_count = index % 7;
                    item
                })
                .collect::<Vec<_>>();
            store.insert_clips(&records)?;
        }
        // Reopening imports the deterministic usage records before timing.
        let store = AppStore::open(&db)?;
        let old_query = || Ok(select_session_items(&store.list_clips()?, LIMIT));
        let new_query = || store.list_quick_paste_candidates(LIMIT);
        let baseline = serde_json::to_vec(&old_query()?)?;
        assert_eq!(serde_json::to_vec(&new_query()?)?, baseline);
        let mut old_samples = Vec::with_capacity(SAMPLES);
        let mut new_samples = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            // Alternate order so one implementation is not always second.
            let ((old, old_ms), (new, new_ms)) = if sample % 2 == 0 {
                (measure(old_query)?, measure(new_query)?)
            } else {
                let new = measure(new_query)?;
                (measure(old_query)?, new)
            };
            assert_eq!(serde_json::to_vec(&old)?, baseline, "old path changed");
            assert_eq!(serde_json::to_vec(&new)?, baseline, "candidate parity changed");
            old_samples.push(old_ms);
            new_samples.push(new_ms);
        }
        old_samples.sort_by(f64::total_cmp);
        new_samples.sort_by(f64::total_cmp);
        let old_median = median(&old_samples);
        let new_median = median(&new_samples);
        let row = json!({
            "records": count, "normalRecords": count - PINNED, "pinnedRecords": PINNED,
            "bodyPaddingBytes": padding.len(), "limit": LIMIT, "selectedRecords": LIMIT + PINNED,
            "serializedParity": true,
            "oldMedianMs": old_median, "newMedianMs": new_median,
            "oldMinMs": old_samples[0], "oldMaxMs": old_samples[SAMPLES - 1],
            "newMinMs": new_samples[0], "newMaxMs": new_samples[SAMPLES - 1],
            "medianSpeedup": old_median / new_median
        });
        println!("{row}");
        results.push(row);
    }
    let output = root.join("results");
    fs::create_dir_all(&output)?;
    fs::write(
        output.join("profile.json"),
        serde_json::to_vec_pretty(&json!({
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "nativeInputUsed": false, "samplesPerPath": SAMPLES, "scenarios": results
        }))?,
    )?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

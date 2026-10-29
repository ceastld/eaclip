//! Isolated synthetic CeaBox measurements. No listener, GUI, capture or user data.
//! Run from host-tauri; output must be a new directory beneath clip/.tmp.
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use chrono::{TimeZone, Utc};
use cliphost_core::{
    ceabox::CeaBoxSource,
    clipboard,
    host::HostState,
    model::{ClipKind, ClipRecord, FavorRecord},
    paths::AppPaths,
    store::AppStore,
};
use serde_json::{json, Value};

fn micros(start: Instant) -> u64 {
    start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let output = PathBuf::from(args.first().ok_or("output directory required")?);
    let count: usize = args
        .get(1)
        .map(|v| v.to_string_lossy().parse())
        .transpose()?
        .unwrap_or(10_000);
    let rounds: usize = args
        .get(2)
        .map(|v| v.to_string_lossy().parse())
        .transpose()?
        .unwrap_or(3);
    let mode = args
        .get(3)
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_else(|| "stream".into());
    if mode != "sync" && mode != "stream" {
        return Err("mode must be sync or stream".into());
    }
    if count == 0 || count > 100_000 || rounds == 0 || rounds > 20 {
        return Err("history count must be 1..100000; rounds must be 1..20".into());
    }
    let temporary = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&temporary)?;
    let temporary = temporary.canonicalize()?;
    let parent = output
        .parent()
        .ok_or("output parent required")?
        .canonicalize()?;
    if !parent.starts_with(&temporary) || output.exists() {
        return Err("output must be a new directory beneath clip/.tmp".into());
    }
    std::fs::create_dir(&output)?;
    let root = tempfile::Builder::new()
        .prefix("synthetic-data-")
        .tempdir_in(&output)?;
    let paths = AppPaths::from_data_root(root.path().to_path_buf());
    paths.ensure()?;
    let seed_started = Instant::now();
    {
        let store = AppStore::open(&paths.db)?;
        let base = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let records: Vec<_> = (0..count)
            .map(|index| {
                let text = match index % 5 {
                    0 => format!(
                        "项目计划 {index} 会议 资料 {}",
                        "正文内容 abc alpha ".repeat(12)
                    ),
                    1 => format!(
                        "Project alpha {index} design notes {}",
                        "Synthetic body text ".repeat(12)
                    ),
                    2 => format!("工作周报 {index} 任务清单 {}", "测试固定样例 ".repeat(12)),
                    3 => format!("https://example.invalid/project/{index}"),
                    _ => format!("Archive record {index} {}", "unrelated content ".repeat(12)),
                };
                let mut item = ClipRecord::new_text(text, "synthetic.exe");
                item.id = format!("clip-{index:06}");
                item.create_time = base + chrono::Duration::seconds(index as i64);
                if index % 10 == 0 {
                    item.kind = ClipKind::HtmlClipItem;
                    item.html_data = Some(format!(
                        "<div>{}</div>",
                        "Synthetic HTML payload ".repeat(100)
                    ));
                }
                item
            })
            .collect();
        store.insert_clips(&records)?;
        for index in 0..count / 10 {
            let mut item = FavorRecord::from_clip(&records[index % records.len()], None);
            item.id = format!("favor-{index:06}");
            item.name = if index % 2 == 0 {
                format!("项目计划 {index}")
            } else {
                format!("Project alpha {index}")
            };
            item.create_time = base + chrono::Duration::seconds(index as i64);
            item.update_time = base + chrono::Duration::seconds(index as i64);
            store.insert_favor(&item)?;
        }
    }
    let seeded_us = micros(seed_started);
    clipboard::force_headless(true);
    let open_started = Instant::now();
    let host = HostState::open(paths, 29065)?;
    let open_us = micros(open_started);
    let mut measurements = Vec::<Value>::new();
    for (name, query) in [
        ("short", "项目"),
        ("multiword", "项目 会议"),
        ("pinyin", "xmjh"),
        ("latin", "alpha"),
    ] {
        for round in 0..rounds {
            let started = Instant::now();
            let mut updates = Vec::new();
            let page = if mode == "stream" {
                host.ceabox_search_stream(
                    query,
                    CeaBoxSource::All,
                    0,
                    40,
                    || false,
                    |update| {
                        updates.push(
                            json!({"elapsed_us":micros(started), "complete":update.complete,
                        "rows":update.page.items.len(),"total":update.page.total}),
                        );
                    },
                )?
                .ok_or("unexpected cancellation")?
            } else {
                host.ceabox_search(query, CeaBoxSource::All, 0, 40)?
            };
            let elapsed_us = micros(started);
            let first_nonempty_us = if mode == "stream" {
                updates
                    .iter()
                    .find(|v| v["rows"].as_u64().is_some_and(|n| n > 0))
                    .and_then(|v| v["elapsed_us"].as_u64())
            } else {
                Some(elapsed_us)
            };
            measurements.push(
                json!({"case":name,"round":round,"first_nonempty_us":first_nonempty_us,
                "complete_us":elapsed_us,"updates":if mode == "stream" {updates.len()} else {1},
                "progress":updates,"total":page.total,
                "ids":page.items.iter().map(|v| &v.id).collect::<Vec<_>>() }),
            );
        }
    }
    // An unseen query uses the already prepared document index but cannot take
    // the exact-query page cache. This measures cooperative CPU cancellation,
    // not input scheduling, UI painting, RPC cancellation or a hard deadline.
    let cancel_probe = if mode == "stream" {
        let started = Instant::now();
        let mut updates = 0usize;
        let mut final_emitted = false;
        let page = host.ceabox_search_stream(
            "xm",
            CeaBoxSource::All,
            0,
            40,
            || started.elapsed() >= Duration::from_millis(2),
            |update| {
                updates += 1;
                final_emitted |= update.complete;
            },
        )?;
        let elapsed_us = micros(started);
        if page.is_some() || final_emitted {
            return Err("unseen query did not stop cooperatively at the cancellation probe".into());
        }
        json!({"request_after_us":2000,"return_us":elapsed_us,
            "cancelled":true,"updates":updates,"final_emitted":final_emitted,
            "scope":"Core CPU cancellation; elapsed time includes scheduling variation, not a strict real-time upper bound"})
    } else {
        Value::Null
    };
    let result = json!({"schema":1,"mode":mode,"profile":if cfg!(debug_assertions){"debug"}else{"release"},
        "fixture_only":true,"history_count":count,"favorite_count":count/10,"action_rpc_connected":false,
        "capture_started":false,"api_started":false,"gui_started":false,"seed_us":seeded_us,"open_us":open_us,
        "rounds_per_query":rounds,"measurements":measurements,"cooperative_cancel_probe":cancel_probe});
    std::fs::write(
        output.join("measurements.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    println!(
        "{}",
        json!({"output":output,"history_count":count,"measurements":result["measurements"].as_array().map(Vec::len),
            "cooperative_cancel_probe":result["cooperative_cancel_probe"]})
    );
    Ok(())
}

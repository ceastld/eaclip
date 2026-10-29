//! Explicit synthetic timing, never a production data probe or a timing gate.
use super::*;

#[test]
#[ignore = "explicit synthetic scale measurement; writes only repository .tmp"]
fn synthetic_candidate_scale_measurement() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap()
        .join(".tmp/storage-v2-benchmarks");
    fs::create_dir_all(&root).unwrap();
    let run = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(&root)
        .unwrap()
        .keep();
    let mut results = Vec::new();
    for count in [1_000usize, 10_000, 50_000] {
        let source = run.join(format!("source-{count}"));
        fs::create_dir(&source).unwrap();
        let mut c = Connection::open(source.join("userdata.db")).unwrap();
        c.execute_batch("CREATE TABLE clip_item(Id TEXT PRIMARY KEY,Data TEXT NOT NULL);CREATE TABLE favor_item(Id TEXT PRIMARY KEY,Data TEXT NOT NULL);").unwrap();
        {
            let tx = c.transaction().unwrap();
            let mut insert = tx.prepare("INSERT INTO clip_item VALUES(?1,?2)").unwrap();
            for n in 0..count {
                let id = format!("synthetic-{n:08}");
                let payload = if n % 10 == 0 {
                    json!({"ObjType":10,"Text":"synthetic 中文😀","HtmlData":format!("<html><head><style>p{{white-space:pre}}</style></head><body><p>{} 中文😀  \r\n</p></body></html>","x".repeat(256))})
                } else {
                    json!({"ObjType":1,"Text":format!("{n} {} 中文😀\r\n","x".repeat(512))})
                };
                let document = json!({"Id":id,"CreateTime":format!("2026-10-16T01:02:03.{:09}Z",n),"SourceProcessName":"synthetic-benchmark","Item":payload});
                insert
                    .execute(params![id, serde_json::to_string(&document).unwrap()])
                    .unwrap();
            }
            drop(insert);
            tx.commit().unwrap();
        }
        drop(c);
        let target = run.join(format!("candidate-{count}"));
        let mut phase_counts = BTreeMap::<String, u64>::new();
        let start = Instant::now();
        let report = build_candidate(
            &MigrationOptions {
                source_root: source.clone(),
                candidate_root: target.clone(),
            },
            &mut |p| {
                *phase_counts.entry(p.phase.clone()).or_default() += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(report.history_count, count);
        assert!(!report.production_ready);
        let result = json!({"historyCount":count,"sourceDatabaseBytes":fs::metadata(source.join("userdata.db")).unwrap().len(),"candidateDatabaseBytes":fs::metadata(target.join("userdata.db")).unwrap().len(),"wallMs":millis(start),"phaseMs":report.phase_timings_ms,"eventCounts":phase_counts,"buildProfile":"debug","fixture":"90% text, 10% HTML, no image I/O"});
        println!("{result}");
        results.push(result);
    }
    write_synced(
        &run.join("measurements.json"),
        &serde_json::to_vec_pretty(&results).unwrap(),
    )
    .unwrap();
    println!(
        "SYNTHETIC_MEASUREMENTS={}",
        run.join("measurements.json").display()
    );
}

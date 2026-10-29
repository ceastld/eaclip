//! Explicit offline candidate builder; never starts a host or activates v2.
use cliphost_core::storage_v2::{build_candidate_with_source_aliases, MigrationOptions};
use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("Storage migration candidate failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut source = None;
    let mut candidate = None;
    let mut source_aliases = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--source" => {
                source = Some(PathBuf::from(
                    args.next().ok_or("--source needs an explicit path")?,
                ))
            }
            "--candidate" => {
                candidate = Some(PathBuf::from(
                    args.next().ok_or("--candidate needs an explicit path")?,
                ))
            }
            "--source-alias-root" => {
                source_aliases
                    .push(PathBuf::from(args.next().ok_or(
                        "--source-alias-root needs an explicit former data root",
                    )?));
            }
            "--help" | "-h" => {
                println!("storage_v2_migrate --source ABSOLUTE_ROOT --candidate ABSOLUTE_NEW_DIRECTORY [--source-alias-root FORMER_ABSOLUTE_DATA_ROOT]\nCreates a verified isolated candidate only. Never changes the selected/live data root.\nAliases map former absolute attachment roots into the selected local source copy; former roots are never read.\nThe source must be local; cloud placeholders and links are rejected. Close legacy writers before preparing a production migration.");
                return Ok(());
            }
            _ => return Err(format!("Unknown option: {arg}")),
        }
    }
    let mut last_phase = String::new();
    let mut last_print = std::time::Instant::now();
    let report = build_candidate_with_source_aliases(
        &MigrationOptions {
            source_root: source.ok_or(
                "An explicit --source is required; production directories are never auto-detected",
            )?,
            candidate_root: candidate.ok_or("An explicit --candidate is required")?,
        },
        &source_aliases,
        &mut |progress| {
            // Checkpoints stay frequent for cancellation; presentation is bounded.
            if progress.phase != last_phase
                || progress.total == Some(progress.completed)
                || last_print.elapsed() >= std::time::Duration::from_millis(100)
            {
                println!(
                    "{}",
                    serde_json::to_string(progress).map_err(|e| e.to_string())?
                );
                last_phase = progress.phase.clone();
                last_print = std::time::Instant::now();
            }
            Ok(())
        },
    )?;
    println!(
        "{}",
        serde_json::to_string(&report).map_err(|e| e.to_string())?
    );
    Ok(())
}

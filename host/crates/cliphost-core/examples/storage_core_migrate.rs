//! Explicit isolated candidate construction. No host startup or activation.
use cliphost_core::storage_core::migrate::{build_candidate_with_settings, MigrationOptions, MigrationSettings};
use std::path::PathBuf;

fn main() {
    if let Err(error)=run(){eprintln!("Core candidate failed: {error}");std::process::exit(1);}
}
fn run()->Result<(),String>{
    let mut source=None;let mut candidate=None;let mut settings=MigrationSettings::default();
    let mut args=std::env::args().skip(1);
    while let Some(arg)=args.next(){match arg.as_str(){
        "--source"=>source=Some(PathBuf::from(args.next().ok_or("--source requires an explicit local clone")?)),
        "--candidate"=>candidate=Some(PathBuf::from(args.next().ok_or("--candidate requires a nonexistent directory")?)),
        "--source-alias-root"=>settings.source_aliases.push(PathBuf::from(args.next().ok_or("--source-alias-root requires a former lexical root")?)),
        "--source-local-offset-seconds"=>settings.source_local_offset_seconds=Some(args.next().ok_or("--source-local-offset-seconds needs an integer")?.parse::<i32>().map_err(|_|"Invalid source offset")?),
        "--help"|"-h"=>{println!("storage_core_migrate --source ABSOLUTE_LOCAL_CLONE --candidate ABSOLUTE_NEW_DIRECTORY [--source-alias-root FORMER_ROOT] [--source-local-offset-seconds INTEGER]\nCreates a complete isolated CORE candidate and protected original snapshot. No production root detection, host startup, or activation. Former roots are never opened. A source with local timestamps needs its explicit historical UTC offset.");return Ok(());},
        _=>return Err(format!("Unknown option: {arg}")),
    }}
    let mut last_phase=String::new();let mut last_print=std::time::Instant::now();
    let report=build_candidate_with_settings(&MigrationOptions{source_root:source.ok_or("Explicit --source is required")?,candidate_root:candidate.ok_or("Explicit --candidate is required")?},&settings,&mut|p|{
        if p.phase!=last_phase||p.total==Some(p.completed)||last_print.elapsed()>=std::time::Duration::from_millis(150){println!("{}",serde_json::to_string(p).map_err(|e|e.to_string())?);last_phase=p.phase.clone();last_print=std::time::Instant::now();}Ok(())
    })?;
    println!("{}",serde_json::to_string(&report).map_err(|e|e.to_string())?);Ok(())
}

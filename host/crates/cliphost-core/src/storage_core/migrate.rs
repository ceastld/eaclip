//! Explicit offline CORE candidates. This module never opens an AppStore, selects
//! a production root, starts a service, or activates a migrated database.
use super::{install_aux_guards, register_writer, schema_checksum, WriteScope, AUX_TABLES, CORE_SCHEMA};
use crate::storage_v2::{self as safe, convert::{self, ArchivedRow, ConvertedItem, SourceFormat}, protected_archive};
pub use crate::storage_v2::{MigrationOptions, MigrationProgress};
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection, DatabaseName};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue, Value};
use std::{collections::{BTreeMap, BTreeSet}, fs, path::{Path, PathBuf}, time::Instant};

const LIMIT: u64 = 256 * 1024 * 1024;
const REPORT_PATH: &str = "migration/core-report.json";
const SNAPSHOT_PATH: &str = "migration/source.sqlite3.dpapi";
#[path="migrate_undo.rs"]
mod undo;

#[derive(Debug, Clone, Default)]
pub struct MigrationSettings {
    /// Lexical aliases only. The former roots are never probed or opened.
    pub source_aliases: Vec<PathBuf>,
    /// Required for non-sentinel DateTime values which have no UTC offset.
    pub source_local_offset_seconds: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateReport {
    pub migration_id: String,
    pub dataset_id: String,
    pub source_format: String,
    pub history_count: usize,
    pub favorite_count: usize,
    pub usage_count: usize,
    pub orphan_usage_count: usize,
    pub saved_undo: String,
    pub auxiliary_tables: BTreeMap<String, usize>,
    pub auxiliary_sources: BTreeMap<String, AuxiliarySource>,
    pub source_aliases: Vec<PathBuf>,
    pub source_local_offset_seconds: Option<i32>,
    pub source_snapshot_sha256: String,
    pub source_archive_path: String,
    pub source_archive_sha256: String,
    pub source_archive_protected: bool,
    pub candidate_database_sha256: String,
    pub schema_sha256: String,
    pub report_path: String,
    pub resource_manifest_path: String,
    pub copied_files: usize,
    pub candidate_only: bool,
    pub production_ready: bool,
    pub requires_original_resource_tree: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CopiedFile {
    pub source_relative_path: String,
    pub target_relative_path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(rename_all="camelCase")]
pub struct AuxiliarySource {
    pub source_table:Option<String>,
    pub adapter:String,
    pub archived_tables:Vec<String>,
}

pub fn build_candidate(options: &MigrationOptions, observer: &mut impl FnMut(&MigrationProgress) -> Result<(), String>) -> Result<CandidateReport, String> {
    build_candidate_with_settings(options, &MigrationSettings::default(), observer)
}

pub fn build_candidate_with_settings(options: &MigrationOptions, settings: &MigrationSettings, observer: &mut impl FnMut(&MigrationProgress) -> Result<(), String>) -> Result<CandidateReport, String> {
    let started = Instant::now();
    progress(observer, started, "preflight", 0, None, "steps")?;
    let source = safe::checked_existing_root(&options.source_root)?;
    super::resources::ResourceResolver::new(&source, &[])?;
    let target = safe::checked_target(&source, &options.candidate_root)?;
    let parent = target.parent().ok_or("Candidate has no parent")?;
    let _lock = safe::MigrationLock::acquire(&parent.join(format!(".{}.migration.lock", target.file_name().unwrap().to_string_lossy())))?;
    safe::require_absent(&target)?;
    // A partially committed RootChildren change cannot be guessed from a stale
    // clone. Recovery is a separate operation before obtaining this snapshot.
    require_missing(&source.join("clip-host-favorite-drag-journal.json"), "CORE_PENDING_ROOT_JOURNAL: finish the original recovery protocol before cloning")?;
    let database = source.join("userdata.db");
    safe::check_local_path(&database, false)?;
    if fs::metadata(&database).map_err(err)?.len() > LIMIT { return Err("CORE_SOURCE_LIMIT: source exceeds 256 MiB".into()); }
    require_missing(&source.join("userdata.db-journal"), "CORE_SOURCE_JOURNAL: offline rollback recovery is required")?;
    let wal = file_exists(&source.join("userdata.db-wal"))?;
    if wal && !file_exists(&source.join("userdata.db-shm"))? { return Err("CORE_WAL_WITHOUT_SHM: no source sidecar will be created".into()); }
    let _source_guard = safe::SourceGuard::acquire(&database)?;
    let input = if wal { safe::open_readonly(&database)? } else { safe::open_immutable_guarded(&database)? };
    safe::reject_unsupported_runtime(&input)?;
    input.execute_batch("BEGIN DEFERRED; SELECT count(*) FROM sqlite_master;").map_err(err)?;
    let original_files = safe::seal_tree(&source)?;
    let version: i64 = input.query_row("PRAGMA data_version", [], |r| r.get(0)).map_err(err)?;
    let snapshot = safe::backup_snapshot(&input, started, observer)?;
    let serialized = snapshot.serialize(DatabaseName::Main).map_err(err)?;
    if serialized.len() as u64 > LIMIT { return Err("CORE_SOURCE_LIMIT: committed WAL snapshot exceeds 256 MiB".into()); }
    let source_snapshot_sha256 = safe::hash_bytes(&serialized);
    let protected = protected_archive::protect(&serialized)?;
    if protected_archive::unprotect(&protected)?.as_slice() != &*serialized { return Err("CORE_ARCHIVE_ROUNDTRIP_FAILED".into()); }
    drop(serialized);
    let stage = tempfile::Builder::new().prefix(".cliphost-core-candidate-").tempdir_in(parent).map_err(err)?;
    fs::create_dir(stage.path().join("migration")).map_err(err)?;
    safe::write_synced(&stage.path().join(SNAPSHOT_PATH), &protected)?;
    let source_archive_sha256 = safe::hash_bytes(&protected);
    drop(protected);
    progress(observer, started, "convert", 0, None, "items")?;
    let mut converted = convert::read_source_core(&snapshot, &source.join("Image"))?;
    let roots = read_roots(&source,&original_files)?;
    // Use the mature graph validator on a disposable copy. The CORE layout
    // retains original ParentId and Children rather than adding a system root
    // or materializing compatibility fallback edges into stored documents.
    let mut validation_copy = converted.clone();
    convert::normalize_favorites(&mut validation_copy, &roots, "__core_validation_root__")?;
    let mut rows = BTreeMap::new();
    for row in &converted.archive {
        if row.table.eq_ignore_ascii_case("clip_item") || row.table.eq_ignore_ascii_case("favor_item") {
            let scope = if row.table.eq_ignore_ascii_case("clip_item") { "history" } else { "favorite" };
            rows.insert((scope.to_owned(), row.required_text("id")?), row.clone());
        }
    }
    let source_format = converted.format;
    let mut core_items = converted.items.iter_mut().map(|item| {
        let row = rows.get(&(item.scope.clone(), item.id.clone())).ok_or("Missing original row")?;
        prepare_item(item, row, source_format, settings)
    }).collect::<Result<Vec<_>, String>>()?;
    let usage = collect_usage(&snapshot, converted.format, &converted.items, settings)?;
    let candidate_db = stage.path().join("userdata.db");
    let mut output = Connection::open(&candidate_db).map_err(err)?;
    output.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA temp_store=MEMORY;").map_err(err)?;
    output.execute_batch(CORE_SCHEMA).map_err(err)?;
    let writer = register_writer(&output)?;
    let migration_id = uuid::Uuid::new_v4().to_string();
    let dataset_id = uuid::Uuid::new_v4().to_string();
    let mut aliases = vec![alias_spelling(&source)?];
    for alias in &settings.source_aliases {
        if !alias.is_absolute() || alias.components().any(|c| matches!(c, std::path::Component::ParentDir)) { return Err("Source aliases must be lexical absolute roots without parent traversal".into()); }
        let alias = alias_spelling(alias)?;
        if !aliases.iter().any(|old|old.to_string_lossy().eq_ignore_ascii_case(&alias.to_string_lossy())) { aliases.push(alias); }
    }
    super::resources::ResourceResolver::new(stage.path(), &aliases)?;
    for item in &mut core_items {
        if item.converted.kind=="image" {
            item.converted.dedupe_key=super::records::image_dedupe_key(&item.payload,stage.path(),&aliases)?;
        }
    }
    let tx = output.transaction().map_err(err)?;
    let legacy = clone_source_tables(&snapshot, &tx)?;
    let (auxiliary_tables,auxiliary_sources) = copy_auxiliary(&snapshot, &tx, converted.format,settings)?;
    for (index, item) in core_items.iter().enumerate() {
        progress(observer, started, "items", index as u64, Some(core_items.len() as u64), "items")?;
        let _lease = writer.enter(if item.converted.scope == "history" { WriteScope::History } else { WriteScope::Favorite })?;
        insert_item(&tx, item)?;
    }
    {
        let _lease = writer.enter(WriteScope::Migration)?;
        for (id, time, count) in &usage {
            tx.execute("INSERT INTO usage_info(id,last_use_time,usage_count) VALUES(?1,?2,?3)", params![id,time,count]).map_err(err)?;
        }
        tx.execute("INSERT INTO store_info(singleton,dataset_id,format_family,format_version,layout_id,min_writer_generation,migration_id) VALUES(1,?1,'cliphost',2,'core',2,?2)", params![dataset_id,migration_id]).map_err(err)?;
        tx.execute("INSERT INTO schema_migrations(version,checksum,applied_at) VALUES(1,?1,?2)", params![schema_checksum(),convert::canonical_time(Utc::now())?]).map_err(err)?;
    }
    tx.execute_batch("CREATE TABLE storage_core_candidate(singleton INTEGER PRIMARY KEY CHECK(singleton=1), migration_id TEXT NOT NULL, source_roots_json TEXT NOT NULL DEFAULT '[]');").map_err(err)?;
    tx.execute("INSERT INTO storage_core_candidate VALUES(1,?1,?2)",params![migration_id,serde_json::to_string(&aliases).map_err(err)?]).map_err(err)?;
    install_aux_guards(&tx)?;
    for name in legacy { seal_table(&tx, &name)?; }
    let runtime=super::Runtime{writer:writer.clone(),root:stage.path().to_owned(),historical_roots:aliases.clone()};
    let saved_undo=undo::migrate(&snapshot,&tx,&runtime,&converted,&roots,&aliases,&target,settings)?;
    tx.commit().map_err(err)?;
    safe::verify_sqlite(&output)?;
    verify_items(&output, &core_items, &usage)?;
    // Close SQLite before hashing/publishing. No writable source connection or
    // plaintext original SQLite archive has ever been created on disk.
    output.close().map_err(|(_,e)|e.to_string())?;
    let copied = copy_source_tree(&source, stage.path(), &original_files, observer, started)?;
    validate_json_files(stage.path(), &copied)?;
    // The source adapter and SQL readback alone do not prove that the complete
    // candidate is readable by the new repository. Validate the exact staged
    // database through its normal core reader before publishing any directory.
    // Immutable opening is safe here only while this guard excludes foreign
    // writes; it also avoids creating SQLite sidecars or changing the DB hash.
    {
        let _candidate_guard = safe::SourceGuard::acquire(&candidate_db)?;
        let validation = safe::open_immutable_guarded(&candidate_db)?;
        super::Runtime::open(&validation, &candidate_db)
            .map_err(|error| format!("CORE_CANDIDATE_RUNTIME_INVALID: {error}"))?;
        validation.close().map_err(|(_, error)| error.to_string())?;
    }
    let manifest_path = "migration/resource-manifest.json";
    safe::write_synced(&stage.path().join(manifest_path), &serde_json::to_vec_pretty(&copied).map_err(err)?)?;
    let identities: BTreeSet<_> = core_items.iter().map(|i|(i.converted.scope.as_str(), i.converted.id.as_str())).collect();
    let orphan_usage_count = usage.iter().filter(|(key,_,_)| {
        let identity = key.strip_prefix("clip_item_").map(|id|("history",id)).or_else(||key.strip_prefix("favor_item_").map(|id|("favorite",id)));
        !identity.is_some_and(|id|identities.contains(&id))
    }).count();
    let mut report = CandidateReport {
        migration_id,dataset_id,source_format:match converted.format {SourceFormat::Documents=>"documents",SourceFormat::Native=>"native"}.into(),
        history_count:core_items.iter().filter(|i|i.converted.scope=="history").count(), favorite_count:core_items.iter().filter(|i|i.converted.scope=="favorite").count(),
        usage_count:usage.len(),orphan_usage_count,saved_undo:saved_undo.into(),auxiliary_tables,auxiliary_sources,source_aliases:aliases,source_local_offset_seconds:settings.source_local_offset_seconds,
        source_snapshot_sha256,source_archive_path:SNAPSHOT_PATH.into(),source_archive_sha256,source_archive_protected:true,
        candidate_database_sha256:safe::hash_file(&candidate_db)?,schema_sha256:schema_checksum(),report_path:REPORT_PATH.into(),resource_manifest_path:manifest_path.into(),copied_files:copied.len(),
        candidate_only:true,production_ready:false,requires_original_resource_tree:false,elapsed_ms:0,
    };
    let stage_seal = safe::seal_tree(stage.path())?;
    progress(observer, started, "ready_to_publish", 1, Some(1), "candidates")?;
    // No callback after these checks: an arbitrarily slow observer cannot open
    // a stale-validation window before the atomic no-replace publication.
    if input.query_row("PRAGMA data_version",[],|r|r.get::<_,i64>(0)).map_err(err)? != version || safe::seal_tree(&source)? != original_files { return Err("CORE_SOURCE_CHANGED: source tree changed before publication".into()); }
    if safe::seal_tree(stage.path())? != stage_seal { return Err("CORE_CANDIDATE_CHANGED: staged candidate changed before publication".into()); }
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    safe::write_synced(&stage.path().join(REPORT_PATH), &serde_json::to_vec_pretty(&report).map_err(err)?)?;
    safe::require_absent(&target)?;
    safe::publish_new_directory(stage.path(), &target)?;
    Ok(report)
}

fn err(error: impl std::fmt::Display) -> String { error.to_string() }
fn progress(observer:&mut impl FnMut(&MigrationProgress)->Result<(),String>,started:Instant,phase:&str,completed:u64,total:Option<u64>,unit:&str)->Result<(),String> {
    observer(&MigrationProgress{phase:phase.into(),completed,total,unit:unit.into(),elapsed_ms:started.elapsed().as_millis() as u64})
}
fn file_exists(path:&Path)->Result<bool,String>{match fs::symlink_metadata(path){Ok(_)=>{safe::check_local_path(path,true)?;Ok(true)},Err(e)if e.kind()==std::io::ErrorKind::NotFound=>Ok(false),Err(e)=>Err(err(e))}}
fn require_missing(path:&Path,message:&str)->Result<(),String>{if file_exists(path)? {Err(message.into())}else{Ok(())}}
fn quote(name:&str)->String{format!("\"{}\"",name.replace('"',"\"\""))}
fn alias_spelling(path:&Path)->Result<PathBuf,String>{
    let spelling=path.to_str().ok_or("Non-UTF8 source alias")?;
    if let Some(rest)=spelling.strip_prefix(r"\\?\") {
        if rest.as_bytes().get(1)==Some(&b':'){return Ok(PathBuf::from(rest));}
        return Err("Network/device source aliases are forbidden".into());
    }
    Ok(path.to_owned())
}

fn copy_source_tree(source:&Path,target:&Path,files:&BTreeMap<PathBuf,String>,observer:&mut impl FnMut(&MigrationProgress)->Result<(),String>,started:Instant)->Result<Vec<CopiedFile>,String>{
    let mut result=Vec::new();
    let mut pending=vec![source.to_owned()];
    while let Some(directory)=pending.pop(){
        safe::check_local_path(&directory,true)?;
        for entry in fs::read_dir(&directory).map_err(err)?{
            let path=entry.map_err(err)?.path();safe::check_local_path(&path,true)?;
            if path.is_dir(){let relative=path.strip_prefix(source).map_err(err)?;
                if relative.components().next().is_some_and(|c|c.as_os_str().to_string_lossy().eq_ignore_ascii_case("migration")){return Err("Source contains a reserved migration namespace".into());}
                fs::create_dir_all(target.join(relative)).map_err(err)?;pending.push(path);
            }
        }
    }
    for (index,(relative,digest)) in files.iter().enumerate(){
        let name=relative.to_string_lossy();
        if matches!(name.as_ref(),"userdata.db"|"userdata.db-wal"|"userdata.db-shm"|"userdata.db-journal"){continue;}
        if relative.components().next().is_some_and(|c|c.as_os_str().to_string_lossy().eq_ignore_ascii_case("migration")){return Err("Source already contains a migration namespace; refusing an ambiguous full-tree clone".into());}
        progress(observer,started,"copy_tree",index as u64,Some(files.len() as u64),"files")?;
        let source_file=source.join(relative);let destination=target.join(relative);
        let parent=destination.parent().ok_or("Copy destination has no parent")?;
        fs::create_dir_all(parent).map_err(err)?;
        let actual=super::resources::publish_copy(&source_file,parent,destination.file_name().ok_or("Missing filename")?.to_str().ok_or("Non-UTF8 filename")?)
            .map_err(|error|format!("CORE_RESOURCE_PUBLICATION_FAILED: relative_sha256={} source_utf16={} target_utf16={} cause={error}",
                safe::hash_bytes(name.as_bytes()),source_file.as_os_str().to_string_lossy().encode_utf16().count(),
                destination.as_os_str().to_string_lossy().encode_utf16().count()))?;
        // The publisher returns a normal DOS path, while checked_target/tempdir
        // can carry Windows' equivalent verbatim DOS prefix. Compare the same
        // lexical spelling so that prefix normalization is not a collision.
        if alias_spelling(&actual)?!=alias_spelling(&destination)? {return Err("CORE_RESOURCE_COPY_RENAMED: refusing a renamed mirror entry".into());}
        if safe::hash_file(&destination)? != *digest {return Err("CORE_RESOURCE_COPY_CHANGED: source or destination differs from the frozen manifest".into());}
        result.push(CopiedFile{source_relative_path:relative.to_string_lossy().replace('\\',"/"),target_relative_path:relative.to_string_lossy().replace('\\',"/"),bytes:fs::metadata(&destination).map_err(err)?.len(),sha256:digest.clone()});
    }
    Ok(result)
}
fn validate_json_files(root:&Path,files:&[CopiedFile])->Result<(),String>{
    for file in files {
        if file.source_relative_path.starts_with("Configs/") && file.source_relative_path.to_ascii_lowercase().ends_with(".json") {
            let path=root.join(&file.target_relative_path);safe::check_local_path(&path,false)?;
            let raw=read_bounded(&path,16*1024*1024)?;let raw=raw.strip_prefix(&[0xef,0xbb,0xbf]).unwrap_or(&raw);
            let _:Box<RawValue>=serde_json::from_slice(raw).map_err(|_|"CORE_CONFIG_INVALID_JSON: configuration was not replaced with defaults".to_owned())?;
        }
    }
    // Existing Rust identity must be a coherent complete group, not merely three
    // readable files; otherwise ordinary LAN startup would silently regenerate.
    let dir=root.join("Configs/LanShare");
    let identity_exists=file_exists(&dir.join("identity.json"))?;
    let cert_exists=file_exists(&dir.join("cert.der"))?;
    let key_exists=file_exists(&dir.join("key.der"))?;
    if (identity_exists||cert_exists||key_exists)&&!(identity_exists&&cert_exists&&key_exists){
        return Err("CORE_LAN_IDENTITY_INCOMPLETE: existing identity, certificate and key must be preserved as a complete group".into());
    }
    if identity_exists {
        let meta:Value=serde_json::from_slice(&read_bounded(&dir.join("identity.json"),1024*1024)?).map_err(err)?;
        let cert=read_bounded(&dir.join("cert.der"),1024*1024)?;
        let key=read_bounded(&dir.join("key.der"),1024*1024)?;
        if meta.get("deviceId").and_then(Value::as_str).is_none_or(str::is_empty) || meta.get("fingerprint").and_then(Value::as_str)!=Some(safe::hash_bytes(&cert).as_str()) || key.is_empty() {return Err("CORE_LAN_IDENTITY_INVALID: identity must not be regenerated during migration".into());}
    }
    Ok(())
}
fn read_bounded(path:&Path,limit:u64)->Result<Vec<u8>,String>{
    use std::io::Read;
    safe::check_local_path(path,false)?;
    let mut bytes=Vec::new();fs::File::open(path).map_err(err)?.take(limit+1).read_to_end(&mut bytes).map_err(err)?;
    if bytes.len() as u64>limit{return Err("CORE_INPUT_LIMIT: file exceeds bounded reader".into());}Ok(bytes)
}
fn read_roots(source:&Path,files:&BTreeMap<PathBuf,String>)->Result<Vec<String>,String>{
    let mut matches=files.iter().filter(|(relative,_)|relative.to_string_lossy().replace('\\',"/").eq_ignore_ascii_case("Configs/FavorConfig.json"));
    let expected=matches.next().map(|(_,digest)|digest);
    if matches.next().is_some(){return Err("CORE_CONFIG_AUTHORITY_COLLISION: multiple favorite configuration spellings".into());}
    let path=source.join("Configs/FavorConfig.json");
    let Some(expected)=expected else{require_missing(&path,"CORE_SOURCE_CHANGED: favorite configuration appeared after the frozen inventory")?;return Ok(Vec::new());};
    let bytes=read_bounded(&path,16*1024*1024)?;
    if safe::hash_bytes(&bytes)!=*expected{return Err("CORE_SOURCE_CHANGED: favorite configuration differs from the frozen snapshot".into());}
    let object=RawFields::parse(std::str::from_utf8(bytes.strip_prefix(&[0xef,0xbb,0xbf]).unwrap_or(&bytes)).map_err(err)?)?;
    match object.get("RootChildren")?{None=>Ok(Vec::new()),Some(v)=>serde_json::from_str(v).map_err(|_|"CORE_ROOT_CHILDREN_INVALID".into())}
}

#[derive(Clone)]
struct CoreItem { converted:ConvertedItem,payload:String,extensions:String,revision:String }

/// RawValue is used at every opaque boundary. In particular, neither a large
/// unknown integer nor the ActionItem's original nested JSON goes through f64.
#[derive(Default)]
struct RawFields(Vec<(String,Box<RawValue>)>);
impl<'de> Deserialize<'de> for RawFields{
    fn deserialize<D:serde::Deserializer<'de>>(d:D)->Result<Self,D::Error>{
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor{
            type Value=RawFields;
            fn expecting(&self,f:&mut std::fmt::Formatter)->std::fmt::Result{f.write_str("an object with unambiguous field names")}
            fn visit_map<M:serde::de::MapAccess<'de>>(self,mut map:M)->Result<Self::Value,M::Error>{let mut fields=Vec::new();let mut keys=BTreeSet::new();while let Some((k,v))=map.next_entry::<String,Box<RawValue>>()?{if !keys.insert(k.clone()){return Err(serde::de::Error::custom("Duplicate JSON key"));}fields.push((k,v));}Ok(RawFields(fields))}
        }
        d.deserialize_map(Visitor)
    }
}
impl RawFields{
    fn parse(raw:&str)->Result<Self,String>{serde_json::from_str(raw).map_err(err)}
    fn get(&self,key:&str)->Result<Option<&str>,String>{
        let mut matches=self.0.iter().filter(|(k,_)|k.eq_ignore_ascii_case(key));
        let value=matches.next().map(|(_,v)|v.get());
        if matches.next().is_some(){return Err(format!("CORE_AMBIGUOUS_KNOWN_FIELD: {key}"));}
        Ok(value)
    }
    fn extensions(&self,consumed:&[&str])->Result<String,String>{
        for key in consumed{self.get(key)?;}
        let mut fields=Vec::new();
        for(k,v)in &self.0{if !consumed.iter().any(|c|k.eq_ignore_ascii_case(c)){
            let lower=k.to_ascii_lowercase();
            if ["execute","script","sync","parent","children","version","objtype"].iter().any(|danger|lower.contains(danger)){return Err("CORE_UNKNOWN_SEMANTICS: unknown execution/structure/version field requires an adapter".into());}
            fields.push(format!("{}:{}",serde_json::to_string(k).map_err(err)?,v.get()));
        }}Ok(format!("{{{}}}",fields.join(",")))
    }
}
/// Some historical C# serializers emitted the inherited numeric discriminator
/// twice in Item. Adapt only that exact envelope, with the same supported
/// integer both times. Its complete raw object is retained as provenance.
struct LegacyItemFields { fields:RawFields, adapted:bool }
impl<'de> Deserialize<'de> for LegacyItemFields{
    fn deserialize<D:serde::Deserializer<'de>>(d:D)->Result<Self,D::Error>{
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor{
            type Value=LegacyItemFields;
            fn expecting(&self,f:&mut std::fmt::Formatter)->std::fmt::Result{f.write_str("a legacy Item object")}
            fn visit_map<M:serde::de::MapAccess<'de>>(self,mut map:M)->Result<Self::Value,M::Error>{
                let mut fields:Vec<(String,Box<RawValue>)>=Vec::new();let mut adapted=false;
                while let Some((key,value))=map.next_entry::<String,Box<RawValue>>()?{
                    if let Some((_,previous))=fields.iter().find(|(name,_)|name==&key){
                        let same=serde_json::from_str::<i64>(previous.get()).ok().zip(serde_json::from_str::<i64>(value.get()).ok())
                            .is_some_and(|(a,b)|a==b&&[1,2,3,10,11,20,21,30,64,65,100].contains(&a));
                        if key!="ObjType"||!same{return Err(serde::de::Error::custom("CORE_DUPLICATE_ITEM_FIELD: only equal supported numeric ObjType duplicates have an adapter"));}
                        adapted=true;continue;
                    }
                    fields.push((key,value));
                }
                Ok(LegacyItemFields{fields:RawFields(fields),adapted})
            }
        }
        d.deserialize_map(Visitor)
    }
}
fn require_unique_nested_json(raw:&str)->Result<(),String>{
    let raw=raw.trim();
    if raw.starts_with('{'){for(_,value)in RawFields::parse(raw)?.0{require_unique_nested_json(value.get())?;}}
    else if raw.starts_with('['){for value in serde_json::from_str::<Vec<Box<RawValue>>>(raw).map_err(err)?{require_unique_nested_json(value.get())?;}}
    Ok(())
}
fn prepare_item(item:&mut ConvertedItem,row:&ArchivedRow,format:SourceFormat,settings:&MigrationSettings)->Result<CoreItem,String>{
    let mut payload=item.payload_json.as_object().ok_or("Converted payload is not an object")?.clone();payload.remove("sourceExtensions");
    let mut extension;
    let mut raw_payload=None;
    let mut image_locator=None;
    let content_hash;
    if format==SourceFormat::Documents{
        let document=RawFields::parse(&row.required_text("Data")?)?;
        let original_item=document.get("Item")?.ok_or("Missing original Item")?;
        let legacy_item:LegacyItemFields=serde_json::from_str(original_item).map_err(err)?;
        for(name,value)in &document.0{if !name.eq_ignore_ascii_case("Item"){require_unique_nested_json(value.get())?;}}
        for(_,value)in &legacy_item.fields.0{require_unique_nested_json(value.get())?;}
        let raw=legacy_item.fields;
        item.created_at=parse_time(document.get("CreateTime")?.ok_or("Missing CreateTime")?,settings)?.ok_or("Missing CreateTime")?;
        item.updated_at=match document.get("UpdateTime")?{Some(v)if v!="null"=>parse_time(v,settings)?.ok_or("Invalid UpdateTime")?,_=>item.created_at.clone()};
        let parsed:Value=serde_json::from_str(&row.required_text("Data")?).map_err(err)?;
        content_hash=crate::csharp_store_document::parse_clip(&item.id,&parsed)?.content_hash;
        let mut consumed=vec!["ObjType","Type"];
        let mapping:&[(&str,&str)]=match item.kind.as_str(){
            "text"=>&[("Text","text")],"html"=>&[("Text","text"),("HtmlData","html"),("HtmlSourceUrl","sourceUrl")],
            "im_rich"=>&[("Text","text"),("QQRichText","xml"),("HtmlData","html"),("HtmlSourceUrl","sourceUrl")],
            "image"=>&[("PerceptualHash","perceptualHash"),("ImageHash","sourceHash"),("ImageMD5","sourceMd5")],
            "files"=>&[("FileList","paths")],"link"=>&[("Url","url"),("Title","sourceTitle"),("Icon","sourceIcon")],
            "command"=>&[("Command","command")],"steps"=>&[("StepData","data"),("StepSummary","sourceSummary")],
            "action"=>&[("ExecuteParam","executeParam")],"folder"=>&[],_=>return Err("Unknown core kind".into())};
        let mut fields=BTreeMap::<String,String>::new();
        for(old,new)in mapping{consumed.push(old);if let Some(value)=raw.get(old)?{fields.insert((*new).into(),value.into());}}
        if item.kind=="action"{consumed.push("ActionItem");if let Some(action)=raw.get("ActionItem")?{fields.insert("data".into(),serde_json::to_string(action).map_err(err)?);}}
        if item.kind=="im_rich"{if let Some(v)=payload.get("origin"){fields.insert("origin".into(),v.to_string());}}
        // The RSource object can carry future nested path metadata. Keep its
        // entire raw token as provenance, never as a writable locator fallback.
        if item.kind=="image"{consumed.extend(["ImagePath","ImageSource"]);image_locator=Some(document_image_locator(&raw)?);}
        let record_consumed=["Id","Item","Title","Description","Icon","CreateTime","UpdateTime","SourceProcessName","IsTop","SubIndex","ParentId","Children","SourceClipId"];
        let discriminator_evidence=if legacy_item.adapted{format!(",\"rawItemDocument\":{}",serde_json::to_string(original_item).map_err(err)?)}else{String::new()};
        extension=format!("{{\"record\":{},\"payload\":{}{discriminator_evidence}}}",document.extensions(&record_consumed)?,raw.extensions(&consumed)?);
        raw_payload=Some(fields);
    }else{
        let raw=row.text("extra_json")?;
        let native=raw.as_deref().map(RawFields::parse).transpose()?;
        let sql_hash=row.text("content_hash")?;
        let json_hash=native.as_ref().map(|v|v.get("contentHash")).transpose()?.flatten().map(serde_json::from_str::<String>).transpose().map_err(err)?;
        if sql_hash.as_ref().zip(json_hash.as_ref()).is_some_and(|(sql,json)|sql!=json){return Err("CORE_NATIVE_CONTENT_HASH_CONFLICT: indexed and full-record identities disagree".into());}
        content_hash=json_hash.or(sql_hash).unwrap_or_default();
        // Typed cell evidence also retains native columns absent from the
        // current flattened model. It never competes with active fields.
        let native_columns=serde_json::to_string(&row.cells).map_err(err)?;
        extension=format!("{{\"nativeColumns\":{native_columns}}}");
        if let Some(native)=native{
            // Keep every native field as source evidence. It is never merged
            // back into active known fields or used as a second authority.
            extension=format!("{{\"nativeRecord\":{},\"nativeColumns\":{native_columns}}}",raw.as_deref().unwrap());
            if let Some(v)=native.get("createTime")?{item.created_at=parse_time(v,settings)?.ok_or("Native createTime missing")?;}
            if let Some(v)=native.get("updateTime")?{item.updated_at=parse_time(v,settings)?.ok_or("Native updateTime missing")?;}
        }
    }
    if item.kind=="image"{
        let (locator,base)=match image_locator{Some(v)=>v,None=>{
            let locator=item.asset_candidates.iter().find(|v|v.slot=="image").ok_or("Image has no locator")?.original_locator.clone();
            let base=if absolute_locator(&locator){"absolute"}else{"image_root"};(locator,base)
        }};
        payload.insert("sourceLocator".into(),json!(locator));payload.insert("locatorBase".into(),json!(base));payload.insert("sourceContentHash".into(),json!(content_hash));
        if let Some(ref mut fields)=raw_payload{for key in ["sourceLocator","locatorBase","sourceContentHash"]{fields.insert(key.into(),payload[key].to_string());}}
    }
    let payload=if let Some(fields)=raw_payload{format!("{{{}}}",fields.into_iter().map(|(k,v)|format!("{}:{v}",serde_json::to_string(&k).unwrap())).collect::<Vec<_>>().join(","))}else{Value::Object(payload).to_string()};
    Ok(CoreItem{converted:item.clone(),payload,extensions:extension,revision:format!("core2:{}",uuid::Uuid::new_v4())})
}
fn absolute_locator(locator:&str)->bool{Path::new(locator).is_absolute()||locator.to_ascii_lowercase().starts_with("file:")}
fn document_image_locator(raw:&RawFields)->Result<(String,&'static str),String>{
    fn text(raw:&str)->Result<String,String>{serde_json::from_str(raw).map_err(err)}
    // These bases follow the fields' constructors, not the locator's prefix:
    // modern ImagePath is relative to ImageLibrary; legacy RPath explicitly
    // records that same base. APath/ImageSource were full paths. Ambiguous
    // relative spellings of the latter need a source-specific adapter.
    if let Some(value)=raw.get("ImagePath")?{let locator=text(value)?;let base=if absolute_locator(&locator){"absolute"}else{"image_root"};return Ok((locator,base));}
    if let Some(value)=raw.get("RSource")?.filter(|v|*v!="null"){
        let source=RawFields::parse(value)?;
        let in_image=source.get("IsInImageDir")?.map(serde_json::from_str::<bool>).transpose().map_err(err)?.unwrap_or(false);
        let locator=text(source.get(if in_image{"RPath"}else{"APath"})?.ok_or("Image source has no declared locator")?)?;
        if in_image{return Ok((locator,"image_root"));}
        if !absolute_locator(&locator){return Err("CORE_AMBIGUOUS_IMAGE_BASE: relative RSource.APath requires an explicit adapter".into());}
        return Ok((locator,"absolute"));
    }
    let locator=text(raw.get("ImageSource")?.ok_or("Image has no source locator")?)?;
    if !absolute_locator(&locator){return Err("CORE_AMBIGUOUS_IMAGE_BASE: relative ImageSource requires an explicit adapter".into());}
    Ok((locator,"absolute"))
}
fn parse_time(raw:&str,settings:&MigrationSettings)->Result<Option<String>,String>{
    if raw=="null"{return Ok(None);}let text:String=serde_json::from_str(raw).map_err(err)?;
    let utc=match DateTime::parse_from_rfc3339(&text){Ok(v)=>v.with_timezone(&Utc),Err(_)=>{
        let offset=settings.source_local_offset_seconds.ok_or("CORE_NAIVE_TIME_POLICY_REQUIRED: supply the source UTC offset explicitly")?;
        let zone=FixedOffset::east_opt(offset).ok_or("Invalid source UTC offset")?;
        let naive=NaiveDateTime::parse_from_str(&text,"%Y-%m-%dT%H:%M:%S%.f").map_err(err)?;
        zone.from_local_datetime(&naive).single().ok_or("Ambiguous source local time")?.with_timezone(&Utc)
    }};Ok(Some(convert::canonical_time(utc)?))
}
fn parse_usage_time(raw:&str,settings:&MigrationSettings)->Result<Option<String>,String>{
    if raw!="null"{let text:String=serde_json::from_str(raw).map_err(err)?;if text.starts_with("0001-01-01T00:00:00"){return Ok(None);}}
    parse_time(raw,settings)
}
fn collect_usage(snapshot:&Connection,format:SourceFormat,items:&[ConvertedItem],settings:&MigrationSettings)->Result<Vec<(String,Option<String>,i64)>,String>{
    let mut usage:BTreeMap<String,(Option<String>,i64)>=BTreeMap::new();
    // Embedded native counters are only a fallback. Dedicated rows override
    // even with count=0/time=NULL, and every orphan/unknown dedicated key stays.
    for i in items{if i.usage_count!=0||i.last_used_at.is_some(){usage.insert(format!("{}_{}",if i.scope=="history"{"clip_item"}else{"favor_item"},i.id),(i.last_used_at.clone(),i.usage_count));}}
    let table=if format==SourceFormat::Documents{"UsageInfo"}else{"usage_info"};
    if table_exists(snapshot,table)?{
        for row in convert::read_rows(snapshot,table)?{let id=row.required_text("id")?;if id.is_empty(){return Err("Empty usage key".into());}
            let (time,count)=if format==SourceFormat::Documents{
                let raw=RawFields::parse(&row.required_text("Data")?)?;
                let document_id=raw.get("Id")?.map(serde_json::from_str::<String>).transpose().map_err(err)?;
                if document_id.as_deref().is_some_and(|v|v!=id){return Err("CORE_USAGE_ID_MISMATCH".into());}
                let count=raw.get("UsageCount")?.map(serde_json::from_str::<i64>).transpose().map_err(err)?.unwrap_or(0);
                let time=raw.get("LastUseTime")?.map(|v|parse_usage_time(v,settings)).transpose()?.flatten();(time,count)
            }else{let count=row.integer("usage_count")?.ok_or("Missing native usage count")?;let ms=row.integer("last_use_time")?.ok_or("Missing native usage time")?;
                let time=if ms==0&&count==0{None}else{Some(convert::canonical_time(Utc.timestamp_millis_opt(ms).single().ok_or("Native usage time out of range")?)?)};(time,count)};
            if count<0{return Err("Negative usage count".into());}usage.insert(id,(time,count));
        }
    }
    Ok(usage.into_iter().map(|(id,(time,count))|(id,time,count)).collect())
}
fn table_exists(c:&Connection,name:&str)->Result<bool,String>{c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 COLLATE NOCASE)",[name],|r|r.get(0)).map_err(err)}
fn source_tables(c:&Connection)->Result<Vec<String>,String>{let mut s=c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").map_err(err)?;let result=s.query_map([],|r|r.get(0)).map_err(err)?.collect::<Result<Vec<_>,_>>().map_err(err);result}
type Column=(String,String,i64,i64,bool,Option<String>);
fn column_info(c:&Connection,table:&str)->Result<Vec<Column>,String>{let mut s=c.prepare(&format!("PRAGMA table_xinfo({})",quote(table))).map_err(err)?;let rows=s.query_map([],|r|Ok((r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(5)?,r.get::<_,i64>(6)?,r.get::<_,i64>(3)?!=0,r.get::<_,Option<String>>(4)?))).map_err(err)?.collect::<Result<Vec<_>,_>>().map_err(err)?;if rows.iter().any(|r|r.3!=0){return Err("CORE_UNSUPPORTED_GENERATED_COLUMN".into());}Ok(rows)}
fn clone_source_tables(source:&Connection,target:&Connection)->Result<Vec<String>,String>{
    let mut names=Vec::new();
    for table in source_tables(source)?{
        if ["items","store_info","schema_migrations","storage_core_candidate"].iter().any(|n|table.eq_ignore_ascii_case(n)){return Err("CORE_RESERVED_SOURCE_TABLE: source format needs an explicit upgrade chain".into());}
        let name=if table.eq_ignore_ascii_case("usage_info")||AUX_TABLES.iter().any(|n|table.eq_ignore_ascii_case(n)){format!("legacy_source_{table}")}else{table.clone()};
        if table_exists(target,&name)?{return Err("CORE_SOURCE_TABLE_COLLISION".into());}
        let columns=column_info(source,&table)?;
        // Evidence tables deliberately have no affinity or executable source
        // constraints/triggers. Exact original DDL is in the protected snapshot;
        // SQL cell types/bytes are copied without executing untrusted DDL.
        target.execute_batch(&format!("CREATE TABLE {}({})",quote(&name),columns.iter().map(|c|quote(&c.0)).collect::<Vec<_>>().join(","))).map_err(err)?;
        copy_rows(source,&table,target,&name,&columns)?;
        names.push(name);
    }Ok(names)
}
fn copy_rows(source:&Connection,from:&str,target:&Connection,to:&str,columns:&[Column])->Result<usize,String>{
    let names=columns.iter().map(|c|quote(&c.0)).collect::<Vec<_>>().join(",");
    let mut source_rows=source.prepare(&format!("SELECT {names} FROM {}",quote(from))).map_err(err)?;
    let mut cursor=source_rows.query([]).map_err(err)?;
    let insert=format!("INSERT INTO {}({names}) VALUES({})",quote(to),vec!["?";columns.len()].join(","));
    let mut statement=target.prepare(&insert).map_err(err)?;let mut count=0;
    while let Some(row)=cursor.next().map_err(err)?{
        let values=(0..columns.len()).map(|i|row.get::<_,SqlValue>(i)).collect::<Result<Vec<_>,_>>().map_err(err)?;
        statement.execute(params_from_iter(values)).map_err(err)?;count+=1;
    }Ok(count)
}
fn aux_columns(table:&str)->&'static[&'static str]{match table{"kv"=>&["key","value"],"lan_message"=>&["id","conversation_id","payload","created_at"],"explorer_record"=>&["path","update_time"],"process_icon_meta"=>&["id","exe_path","update_time"],_=>&["id","payload"]}}
// Only inspect unquoted SQL words. A literal/default/quoted identifier named
// "check" is ordinary data; executable table constraints require an adapter.
fn schema_words(sql:&str)->Result<Vec<String>,String>{
    let bytes=sql.as_bytes();let mut i=0;let mut words=Vec::new();
    while i<bytes.len(){
        let b=bytes[i];
        if b==b'-'&&bytes.get(i+1)==Some(&b'-'){i+=2;while i<bytes.len()&&bytes[i]!=b'\n'{i+=1;}continue;}
        if b==b'/'&&bytes.get(i+1)==Some(&b'*'){i+=2;while i+1<bytes.len()&& &bytes[i..i+2]!=b"*/"{i+=1;}if i+1>=bytes.len(){return Err("CORE_AUX_SCHEMA_UNSUPPORTED: unterminated comment".into());}i+=2;continue;}
        if matches!(b,b'\''|b'"'|b'`'|b'['){let closing=if b==b'['{b']'}else{b};i+=1;loop{if i>=bytes.len(){return Err("CORE_AUX_SCHEMA_UNSUPPORTED: unterminated quoted value".into());}if bytes[i]==closing{if closing!=b']'&&bytes.get(i+1)==Some(&closing){i+=2;continue;}i+=1;break;}i+=1;}continue;}
        if b.is_ascii_alphabetic()||b==b'_'{let start=i;i+=1;while i<bytes.len()&&(bytes[i].is_ascii_alphanumeric()||bytes[i]==b'_'){i+=1;}words.push(sql[start..i].to_ascii_uppercase());}else{i+=1;}
    }Ok(words)
}
fn safe_default(value:&str)->Result<(),String>{
    // Re-emit only a single constant expression. In particular, an untrusted
    // source DEFAULT cannot install a function call for a future host insert.
    let mut value=value.trim();
    while value.starts_with('(')&&value.ends_with(')'){value=value[1..value.len()-1].trim();}
    if value.eq_ignore_ascii_case("NULL")||["CURRENT_TIME","CURRENT_DATE","CURRENT_TIMESTAMP","TRUE","FALSE"].iter().any(|v|value.eq_ignore_ascii_case(v)){return Ok(());}
    let quoted=if value.starts_with(['\'','"']){Some(value)}else if value.starts_with(['x','X'])&&value.as_bytes().get(1)==Some(&b'\''){Some(&value[1..])}else{None};
    if let Some(quoted)=quoted{let bytes=quoted.as_bytes();let closing=bytes[0];let mut i=1;while i<bytes.len(){if bytes[i]==closing{if bytes.get(i+1)==Some(&closing){i+=2;continue;}if i==bytes.len()-1{return Ok(());}break;}i+=1;}return Err("CORE_AUX_DEFAULT_UNSUPPORTED: default is not a single literal".into());}
    if !value.is_empty()&&value.bytes().all(|b|b.is_ascii_digit()||b"+-.eE".contains(&b))&&value.parse::<f64>().is_ok(){return Ok(());}
    Err("CORE_AUX_DEFAULT_UNSUPPORTED: explicit default-expression adapter required".into())
}
fn default_aux_columns(table:&str)->Vec<Column>{aux_columns(table).iter().enumerate().map(|(i,c)|((*c).into(),if *c=="created_at"||*c=="update_time"{"INTEGER".into()}else{"TEXT".into()},i64::from(i==0),0,true,None)).collect()}
fn validate_aux_schema(source:&Connection,from:&str,columns:&[Column],key:&str)->Result<(),String>{
    let foreign:i64=source.query_row("SELECT count(*) FROM pragma_foreign_key_list(?1)",[from],|r|r.get(0)).map_err(err)?;
    let triggers:i64=source.query_row("SELECT count(*) FROM sqlite_master WHERE type='trigger' AND tbl_name=?1 COLLATE NOCASE",[from],|r|r.get(0)).map_err(err)?;
    if foreign!=0||triggers!=0{return Err("CORE_AUX_EXECUTABLE_SCHEMA_UNSUPPORTED: explicit domain adapter required".into());}
    let unique:i64=source.query_row("SELECT count(*) FROM pragma_index_list(?1) WHERE \"unique\"<>0 AND origin<>'pk'",[from],|r|r.get(0)).map_err(err)?;
    let ddl:String=source.query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name=?1 COLLATE NOCASE",[from],|r|r.get(0)).map_err(err)?;
    let words=schema_words(&ddl)?;
    if unique!=0||words.iter().any(|word|["CHECK","COLLATE","CONFLICT","STRICT","AUTOINCREMENT"].contains(&word.as_str())){return Err("CORE_AUX_CONSTRAINT_UNSUPPORTED: unknown uniqueness/check/collation/table semantics require an adapter".into());}
    let primary=columns.iter().filter(|c|c.2!=0).collect::<Vec<_>>();
    if primary.len()!=1||!primary[0].0.eq_ignore_ascii_case(key)||primary[0].2!=1{return Err("CORE_AUX_PRIMARY_KEY_UNSUPPORTED: domain key must remain the sole primary key".into());}
    Ok(())
}
fn copy_document_aux(source:&Connection,target:&Connection,table:&str,settings:&MigrationSettings)->Result<usize,String>{
    let mut keys=BTreeSet::new();let mut count=0;
    for row in convert::read_rows(source,table)?{
        let id=row.required_text("Id")?;let raw=RawFields::parse(&row.required_text("Data")?)?;
        let data_id:String=serde_json::from_str(raw.get("Id")?.ok_or("CORE_AUX_DOCUMENT_INVALID: missing Id")?).map_err(|_|"CORE_AUX_DOCUMENT_INVALID: Id must be a string".to_owned())?;
        if id.is_empty()||data_id!=id{return Err("CORE_AUX_DOCUMENT_INVALID: SQL and document identities differ".into());}
        let field=if table=="explorer_record"{"Path"}else{"ExePath"};
        let locator:String=serde_json::from_str(raw.get(field)?.ok_or("CORE_AUX_DOCUMENT_INVALID: missing locator")?).map_err(|_|"CORE_AUX_DOCUMENT_INVALID: locator must be a string".to_owned())?;
        // Empty strings are valid defaults on both C# entity types and on the
        // existing Rust scalar schema. Preserve them rather than dropping rows.
        raw.extensions(&["Id",field,"UpdateTime"])?;
        for(_,value)in &raw.0{require_unique_nested_json(value.get())?;}
        let time=parse_time(raw.get("UpdateTime")?.ok_or("CORE_AUX_DOCUMENT_INVALID: missing business UpdateTime")?,settings)?.ok_or("CORE_AUX_DOCUMENT_INVALID: null business UpdateTime")?;
        let time=DateTime::parse_from_rfc3339(&time).map_err(err)?.timestamp_millis();
        let key=if table=="explorer_record"{&locator}else{&id};
        if !keys.insert(key.as_str().to_owned()){return Err("CORE_AUX_DOCUMENT_KEY_COLLISION: multiple documents map to the same business key".into());}
        if table=="explorer_record"{target.execute("INSERT INTO explorer_record(path,update_time) VALUES(?1,?2)",params![locator,time]).map_err(err)?;}
        else{target.execute("INSERT INTO process_icon_meta(id,exe_path,update_time) VALUES(?1,?2,?3)",params![id,locator,time]).map_err(err)?;}
        count+=1;
    }Ok(count)
}
fn copy_auxiliary(source:&Connection,target:&Connection,format:SourceFormat,settings:&MigrationSettings)->Result<(BTreeMap<String,usize>,BTreeMap<String,AuxiliarySource>),String>{
    let mut result=BTreeMap::new();let mut sources=BTreeMap::new();
    for table in AUX_TABLES{
        let prefixed=format!("cliphost_{table}");
        let prefix_exists=table_exists(source,&prefixed)?;let bare_exists=table_exists(source,table)?;
        // Existing Rust Documents SQL routes every AUX domain to its prefixed
        // table. Empty active tables must not replay another generation's data.
        let document_adapter=format==SourceFormat::Documents&&!prefix_exists&&bare_exists;
        let from=if format==SourceFormat::Documents&&prefix_exists{prefixed.clone()}else{(*table).into()};
        let exists=table_exists(source,&from)?;
        let expected=aux_columns(table);
        let columns=if document_adapter{
            let original=column_info(source,&from)?;
            if !["explorer_record","process_icon_meta"].contains(table)||!["Id","Data","ObjectType","CreatedTime","UpdateTime"].iter().all(|name|original.iter().any(|column|column.0.eq_ignore_ascii_case(name))){return Err(format!("CORE_AUX_BARE_SCHEMA_UNSUPPORTED: {table} has no evidenced document adapter"));}
            validate_aux_schema(source,&from,&original,"Id")?;
            default_aux_columns(table)
        }else if exists{column_info(source,&from)?}else{default_aux_columns(table)};
        if !expected.iter().all(|name|columns.iter().any(|c|c.0.eq_ignore_ascii_case(name))){return Err(format!("CORE_AUX_SCHEMA_UNSUPPORTED: {from}"));}
        if exists&&!document_adapter{validate_aux_schema(source,&from,&columns,expected[0])?;}
        let defs=columns.iter().map(|c|{let primary=c.0.eq_ignore_ascii_case(expected[0]);let default=match &c.5{Some(value)=>{safe_default(value)?;format!(" DEFAULT {value}")},None=>String::new()};Ok(format!("{} {}{}{}{default}",quote(&c.0),if c.1.is_empty(){String::new()}else{quote(&c.1)},if primary{" PRIMARY KEY"}else{""},if c.4{" NOT NULL"}else{""}))}).collect::<Result<Vec<_>,String>>()?.join(",");
        target.execute_batch(&format!("CREATE TABLE {}({defs})",quote(table))).map_err(err)?;
        let count=if document_adapter{copy_document_aux(source,target,table,settings)?}else if exists{copy_rows(source,&from,target,table,&columns)?}else{0};result.insert((*table).into(),count);
        let mut archived_tables=Vec::new();if bare_exists{archived_tables.push(format!("legacy_source_{table}"));}if prefix_exists{archived_tables.push(prefixed);}
        let adapter=if document_adapter{if *table=="explorer_record"{"csharp-explorer-document-v1"}else{"csharp-process-icon-document-v1"}}else if !exists{"empty-domain-v1"}else if format==SourceFormat::Documents{"rust-prefixed-scalar-v1"}else{"rust-native-scalar-v1"};
        sources.insert((*table).into(),AuxiliarySource{source_table:exists.then_some(from),adapter:adapter.into(),archived_tables});
    }Ok((result,sources))
}
fn seal_table(c:&Connection,name:&str)->Result<(),String>{for(op,suffix)in[("INSERT","insert"),("UPDATE","update"),("DELETE","delete")]{let trigger=format!("sealed_{}_{}",&safe::hash_bytes(name.as_bytes())[..16],suffix);c.execute_batch(&format!("CREATE TRIGGER {} BEFORE {op} ON {} BEGIN SELECT RAISE(ABORT,'LEGACY_SOURCE_READ_ONLY'); END;",quote(&trigger),quote(name))).map_err(err)?;}Ok(())}
fn insert_item(c:&Connection,item:&CoreItem)->Result<(),String>{let i=&item.converted;c.execute("INSERT INTO items(scope,id,kind,title,icon_override,description,payload_version,payload_json,source_extensions_json,dedupe_key,source_process,created_at,updated_at,sub_index,is_pinned,revision,owner_parent_id,children_json,source_history_id) VALUES(?1,?2,?3,?4,?5,?6,1,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",params![i.scope,i.id,i.kind,i.title,i.icon_override,i.description,item.payload,item.extensions,i.dedupe_key,i.source_process,i.created_at,i.updated_at,i.sub_index,i.is_pinned as i64,item.revision,i.owner_parent_id,i.children.as_ref().map(serde_json::to_string).transpose().map_err(err)?,i.source_history_id]).map_err(err)?;Ok(())}
fn verify_items(c:&Connection,items:&[CoreItem],usage:&[(String,Option<String>,i64)])->Result<(),String>{
    let count:i64=c.query_row("SELECT count(*) FROM items",[],|r|r.get(0)).map_err(err)?;if count as usize!=items.len(){return Err("CORE_ITEM_COUNT_MISMATCH".into());}
    for item in items{let i=&item.converted;let row:(String,String,String,String)=c.query_row("SELECT payload_json,source_extensions_json,revision,created_at FROM items WHERE scope=?1 AND id=?2",params![i.scope,i.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(err)?;if row!=(item.payload.clone(),item.extensions.clone(),item.revision.clone(),i.created_at.clone()){return Err("CORE_ITEM_READBACK_MISMATCH".into());}}
    let mut s=c.prepare("SELECT id,last_use_time,usage_count FROM usage_info ORDER BY id").map_err(err)?;let actual=s.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,i64>(2)?))).map_err(err)?.collect::<Result<Vec<_>,_>>().map_err(err)?;if actual!=usage{return Err("CORE_USAGE_READBACK_MISMATCH".into());}Ok(())
}

#[cfg(test)]
#[path="migrate_tests.rs"]
mod tests;

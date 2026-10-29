#![cfg(windows)]
use super::*;

const DATE:&str="2026-10-17T03:12:34.123456700Z";
fn root()->tempfile::TempDir{
    let base=Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().join(".tmp/storage-core-migration-tests");
    fs::create_dir_all(&base).unwrap();tempfile::Builder::new().prefix("case-").tempdir_in(base).unwrap()
}
fn fixture(root:&Path)->MigrationOptions{
    let source=root.join("source");fs::create_dir_all(source.join("Configs")).unwrap();fs::create_dir_all(source.join("Image/nested")).unwrap();
    fs::write(source.join("Configs/FavorConfig.json"),b"{\"RootChildren\":[\"folder\"],\"FuturePreference\":true}").unwrap();
    fs::write(source.join("Image/nested/shared.png"),b"synthetic resource bytes").unwrap();
    let db=Connection::open(source.join("userdata.db")).unwrap();
    db.execute_batch("CREATE TABLE clip_item(Id TEXT PRIMARY KEY,Data TEXT,Extra BLOB); CREATE TABLE favor_item(Id TEXT PRIMARY KEY,Data TEXT); CREATE TABLE UsageInfo(Id TEXT PRIMARY KEY,Data TEXT); CREATE TABLE cliphost_kv(key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL); CREATE TABLE cliphost_lan_message(id TEXT PRIMARY KEY NOT NULL,conversation_id TEXT NOT NULL,payload TEXT NOT NULL,created_at INTEGER NOT NULL);").unwrap();
    let clip=format!(r#"{{"Id":"clip","Title":"","Icon":null,"CreateTime":"{DATE}","UpdateTime":"{DATE}","SourceProcessName":"fixture","FutureNumber":123456789012345678901234567890,"FutureDecimal":1.2300e+42,"Item":{{"ObjType":1,"Text":"original","FutureNested":{{"number":987654321098765432109876543210}}}}}}"#);
    db.execute("INSERT INTO clip_item VALUES('clip',?1,?2)",params![clip,vec![0u8,255,7]]).unwrap();
    for(id,item,children,parent)in[("folder",json!({"ObjType":100}),json!(["fav"]),Value::Null),("fav",json!({"ObjType":20,"Url":"https://example.test/","Title":"source title","Icon":"source icon"}),json!([]),json!("folder"))]{
        let doc=json!({"Id":id,"Title":null,"Icon":"","CreateTime":DATE,"UpdateTime":DATE,"SourceProcessName":"fixture","Item":item,"Children":children,"ParentId":parent});
        db.execute("INSERT INTO favor_item VALUES(?1,?2)",params![id,doc.to_string()]).unwrap();
    }
    for(id,time,count)in[("clip_item_clip",Value::Null,0),("future_orphan",Value::Null,9)]{
        db.execute("INSERT INTO UsageInfo VALUES(?1,?2)",params![id,json!({"Id":id,"LastUseTime":time,"UsageCount":count}).to_string()]).unwrap();
    }
    db.execute("INSERT INTO cliphost_kv VALUES('unknown-key',?1)",["{\"exact\":1.23000e+32}"]).unwrap();
    db.execute("INSERT INTO cliphost_lan_message VALUES('message','conversation',?1,17)",["{\"data\":\"opaque\"}"]).unwrap();
    drop(db);MigrationOptions{source_root:source,candidate_root:root.join("candidate")}
}
fn run(options:&MigrationOptions)->Result<CandidateReport,String>{build_candidate(options,&mut |_|Ok(()))}
fn candidate(options:&MigrationOptions)->Connection{safe::open_immutable_guarded(&options.candidate_root.join("userdata.db")).unwrap()}

#[test]
fn core_migration_preserves_raw_tokens_overrides_usage_auxiliary_and_full_tree(){
    let root=root();let options=fixture(root.path());let before=safe::seal_tree(&options.source_root).unwrap();
    let report=run(&options).unwrap();assert!(report.candidate_only&&!report.production_ready);assert_eq!((report.history_count,report.favorite_count,report.usage_count,report.orphan_usage_count),(1,2,2,1));
    assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    let db=candidate(&options);let(payload,extensions,title,icon,created):(String,String,Option<String>,Option<String>,String)=db.query_row("SELECT payload_json,source_extensions_json,title,icon_override,created_at FROM items WHERE scope='history' AND id='clip'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(payload,"{\"text\":\"original\"}");assert!(extensions.contains("123456789012345678901234567890"));assert!(extensions.contains("1.2300e+42"));assert!(extensions.contains("987654321098765432109876543210"));assert_eq!(title,Some(String::new()));assert_eq!(icon,None);assert_eq!(created,DATE);
    let link:String=db.query_row("SELECT payload_json FROM items WHERE id='fav'",[],|r|r.get(0)).unwrap();assert!(link.contains("source title"));assert!(link.contains("source icon"));
    let value:String=db.query_row("SELECT value FROM kv WHERE key='unknown-key'",[],|r|r.get(0)).unwrap();assert_eq!(value,"{\"exact\":1.23000e+32}");
    let orphan:(Option<String>,i64)=db.query_row("SELECT last_use_time,usage_count FROM usage_info WHERE id='future_orphan'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(orphan,(None,9));
    assert_eq!(fs::read(options.source_root.join("Configs/FavorConfig.json")).unwrap(),fs::read(options.candidate_root.join("Configs/FavorConfig.json")).unwrap());
    assert_eq!(fs::read(options.source_root.join("Image/nested/shared.png")).unwrap(),fs::read(options.candidate_root.join("Image/nested/shared.png")).unwrap());
    assert!(db.execute("DELETE FROM clip_item",[]).is_err());assert!(db.execute("DELETE FROM items",[]).is_err());assert!(db.execute("DELETE FROM kv",[]).is_err());
    assert!(crate::store::AppStore::open(&options.candidate_root.join("userdata.db")).is_err());
}

#[test]
fn core_migration_requires_complete_runtime_readability_before_publication(){
    for valid_source_title in [false,true]{
        let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
        // The old reader accepts a link with a record title without decoding
        // its shadowed source title. The new reader must also understand that
        // source payload before a candidate can be published successfully.
        let source_title=if valid_source_title{json!("source title")}else{json!(7)};
        let document=json!({"Id":"link","Title":"visible override","CreateTime":DATE,"UpdateTime":DATE,"Item":{"ObjType":20,"Url":"https://example.test/","Title":source_title}});
        db.execute("INSERT INTO clip_item(Id,Data) VALUES('link',?1)",[document.to_string()]).unwrap();drop(db);
        let before=safe::seal_tree(&options.source_root).unwrap();let result=run(&options);
        if valid_source_title{
            let report=result.unwrap();let db=candidate(&options);
            let runtime=crate::storage_core::Runtime::open(&db,&options.candidate_root.join("userdata.db")).unwrap();
            let link=runtime.get_clip(&db,"link").unwrap();assert_eq!(link.title.as_deref(),Some("visible override"));
            let payload:String=db.query_row("SELECT payload_json FROM items WHERE id='link'",[],|r|r.get(0)).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&payload).unwrap()["sourceTitle"],json!("source title"));
            assert_eq!(safe::hash_file(&options.candidate_root.join("userdata.db")).unwrap(),report.candidate_database_sha256);
            for suffix in ["-wal","-shm","-journal"]{assert!(!options.candidate_root.join(format!("userdata.db{suffix}")).exists());}
        }else{
            let error=result.unwrap_err();assert!(error.contains("CORE_CANDIDATE_RUNTIME_INVALID"),"{error}");assert!(error.contains("sourceTitle"),"{error}");
            assert!(!options.candidate_root.exists());
        }
        assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    }
}
#[test]
fn core_migration_cancel_each_visible_phase_never_publishes_or_changes_source(){
    for phase in ["preflight","snapshot","convert","items","copy_tree","ready_to_publish"]{
        let root=root();let options=fixture(root.path());let before=safe::seal_tree(&options.source_root).unwrap();
        let mut reached=false;let result=build_candidate(&options,&mut|p|if p.phase==phase{reached=true;Err("synthetic cancellation".into())}else{Ok(())});assert!(reached,"did not reach cancellation phase {phase}");assert!(result.is_err(),"{phase}");assert!(!options.candidate_root.exists());assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    }
}
#[test]
fn core_migration_detects_external_config_change_after_last_callback(){
    let root=root();let options=fixture(root.path());let db_before=fs::read(options.source_root.join("userdata.db")).unwrap();
    let result=build_candidate(&options,&mut|p|{if p.phase=="ready_to_publish"{fs::write(options.source_root.join("Configs/FavorConfig.json"),b"{\"RootChildren\":[]}").unwrap();}Ok(())});
    assert!(result.unwrap_err().contains("CORE_SOURCE_CHANGED"));assert!(!options.candidate_root.exists());assert_eq!(fs::read(options.source_root.join("userdata.db")).unwrap(),db_before);
}
#[test]
fn core_migration_refuses_existing_target_pending_journal_and_saved_undo(){
    let root=root();let options=fixture(root.path());fs::create_dir(&options.candidate_root).unwrap();assert!(run(&options).is_err());fs::remove_dir(&options.candidate_root).unwrap();
    fs::write(options.source_root.join("clip-host-favorite-drag-journal.json"),b"{}").unwrap();assert!(run(&options).unwrap_err().contains("CORE_PENDING_ROOT_JOURNAL"));fs::remove_file(options.source_root.join("clip-host-favorite-drag-journal.json")).unwrap();
    let db=Connection::open(options.source_root.join("userdata.db")).unwrap();db.execute("INSERT INTO cliphost_kv VALUES('favorite-drag.state.v1',?1)",["{\"receipts\":[],\"undo\":{\"id\":\"saved\"}} "]).unwrap();drop(db);
    let before=safe::seal_tree(&options.source_root).unwrap();assert!(run(&options).unwrap_err().contains("CORE_UNDO_REQUIRES_LOSSLESS_CONVERTER"));assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);assert!(!options.candidate_root.exists());
}
#[test]
fn core_migration_bad_json_invalid_graph_and_aux_authority_collision_fail_closed(){
    for condition in ["json","graph","aux"]{
        let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
        match condition{"json"=>{db.execute("UPDATE clip_item SET Data='broken'",[]).unwrap();},"graph"=>{db.execute("UPDATE favor_item SET Data=json_set(Data,'$.Children',json('[\"missing\"]')) WHERE Id='folder'",[]).unwrap();},_=>{db.execute_batch("CREATE TABLE text_action(key TEXT PRIMARY KEY,value TEXT)").unwrap();}}
        drop(db);let before=safe::seal_tree(&options.source_root).unwrap();let error=run(&options).unwrap_err();if condition=="aux"{assert!(error.contains("CORE_AUX_BARE_SCHEMA_UNSUPPORTED"),"{error}");}assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);assert!(!options.candidate_root.exists());
    }
}
#[test]
fn core_migration_active_writer_is_rejected_without_source_side_effects(){
    let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();let before=safe::seal_tree(&options.source_root).unwrap();assert!(run(&options).is_err());assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);assert!(!options.candidate_root.exists());drop(db);
}
#[test]
fn core_migration_image_keeps_hash_states_without_opening_its_missing_file(){
    let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
    let data=json!({"Id":"image","CreateTime":DATE,"UpdateTime":DATE,"Item":{"ObjType":3,"ImagePath":"missing.png","ImageHash":"","ImageMD5":"legacy-md5","PerceptualHash":null}});
    db.execute("INSERT INTO clip_item(Id,Data) VALUES('image',?1)",[data.to_string()]).unwrap();drop(db);
    run(&options).unwrap();let db=candidate(&options);let raw:String=db.query_row("SELECT payload_json FROM items WHERE id='image'",[],|r|r.get(0)).unwrap();let payload:Value=serde_json::from_str(&raw).unwrap();assert_eq!(payload["sourceLocator"],"missing.png");assert_eq!(payload["sourceHash"],"");assert_eq!(payload["sourceMd5"],"legacy-md5");assert!(payload.get("perceptualHash").unwrap().is_null());assert!(payload.get("sourceContentHash").is_some());
}
#[test]
fn core_migration_naive_usage_requires_explicit_policy_and_keeps_orphan(){
    let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();db.execute("UPDATE UsageInfo SET Data=?1 WHERE Id='future_orphan'",[json!({"Id":"future_orphan","LastUseTime":"2025-05-06T12:34:56.1234567","UsageCount":9}).to_string()]).unwrap();drop(db);
    assert!(run(&options).unwrap_err().contains("CORE_NAIVE_TIME_POLICY_REQUIRED"));let settings=MigrationSettings{source_local_offset_seconds:Some(-25200),..Default::default()};build_candidate_with_settings(&options,&settings,&mut |_|Ok(())).unwrap();let db=candidate(&options);let time:String=db.query_row("SELECT last_use_time FROM usage_info WHERE id='future_orphan'",[],|r|r.get(0)).unwrap();assert_eq!(time,"2025-05-06T19:34:56.123456700Z");
}

#[test]
fn core_migration_keeps_case_distinct_unknown_keys_and_rejects_known_aliases(){
    let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
    let raw=format!(r#"{{"Id":"case","CreateTime":"{DATE}","UpdateTime":"{DATE}","x":1.23000e+42,"X":123456789012345678901234567890,"Item":{{"ObjType":1,"Text":"case","future":{{"x":1,"X":2}},"Future":false}}}}"#);
    db.execute("INSERT INTO clip_item(Id,Data) VALUES('case',?1)",[raw]).unwrap();drop(db);
    run(&options).unwrap();let db=candidate(&options);let extensions:String=db.query_row("SELECT source_extensions_json FROM items WHERE id='case'",[],|r|r.get(0)).unwrap();
    assert!(extensions.contains(r#""x":1.23000e+42"#));assert!(extensions.contains(r#""X":123456789012345678901234567890"#));assert!(extensions.contains(r#""future":{"x":1,"X":2}"#));assert!(extensions.contains(r#""Future":false"#));drop(db);
    let root=self::root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
    db.execute("UPDATE clip_item SET Data=json_set(Data,'$.item',json('{\"ObjType\":1,\"Text\":\"ambiguous\"}'))",[]).unwrap();drop(db);
    let before=safe::seal_tree(&options.source_root).unwrap();assert!(run(&options).unwrap_err().contains("CORE_AMBIGUOUS_KNOWN_FIELD"));assert!(!options.candidate_root.exists());assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
}

#[test]
fn core_migration_record_year_one_is_preserved_but_usage_sentinel_is_null(){
    let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
    db.execute("UPDATE clip_item SET Data=json_set(Data,'$.CreateTime','0001-01-01T00:00:00.000000000Z','$.UpdateTime','0001-01-01T00:00:00.000000001Z')",[]).unwrap();
    db.execute("UPDATE UsageInfo SET Data=json_set(Data,'$.LastUseTime','0001-01-01T00:00:00') WHERE Id='future_orphan'",[]).unwrap();drop(db);
    run(&options).unwrap();let db=candidate(&options);let times:(String,String)=db.query_row("SELECT created_at,updated_at FROM items WHERE id='clip'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(times,("0001-01-01T00:00:00.000000000Z".into(),"0001-01-01T00:00:00.000000001Z".into()));
    let time:Option<String>=db.query_row("SELECT last_use_time FROM usage_info WHERE id='future_orphan'",[],|r|r.get(0)).unwrap();assert_eq!(time,None);
}

#[test]
fn core_migration_image_base_uses_legacy_declared_source_and_keeps_nested_extensions(){
    let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
    for(id,item)in[("relative",json!({"ObjType":3,"RSource":{"IsInImageDir":true,"RPath":"nested/shared.png","APath":"ignored","FuturePath":{"x":1,"X":2}}})),("absolute",json!({"ObjType":3,"RSource":{"IsInImageDir":false,"APath":"Z:/never-read/fixture.png"}}))]{
        db.execute("INSERT INTO clip_item(Id,Data) VALUES(?1,?2)",params![id,json!({"Id":id,"CreateTime":DATE,"UpdateTime":DATE,"Item":item}).to_string()]).unwrap();
    }drop(db);run(&options).unwrap();let db=candidate(&options);
    for(id,locator,base)in[("relative","nested/shared.png","image_root"),("absolute","Z:/never-read/fixture.png","absolute")]{
        let raw:String=db.query_row("SELECT payload_json FROM items WHERE id=?1",[id],|r|r.get(0)).unwrap();let value:Value=serde_json::from_str(&raw).unwrap();assert_eq!(value["sourceLocator"],locator);assert_eq!(value["locatorBase"],base);
    }
    let raw:String=db.query_row("SELECT source_extensions_json FROM items WHERE id='relative'",[],|r|r.get(0)).unwrap();let value:Value=serde_json::from_str(&raw).unwrap();assert_eq!(value["payload"]["RSource"]["FuturePath"],json!({"x":1,"X":2}));
}

fn save_synthetic_undo(options:&MigrationOptions,mode:&str)->String{
    let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
    let current=undo::legacy_favorites(&db,SourceFormat::Documents,&MigrationSettings::default()).unwrap();
    let revision=crate::host::storage_core_legacy_favorite_revision(&current,&["folder".into()]).unwrap();
    let mut before=current.clone();
    for row in &mut before{if row.id=="folder"{row.children.clear();}else{row.parent_id=None;row.sub_index=7;}}
    if mode=="content_change"{before.iter_mut().find(|r|r.id=="fav").unwrap().name="old different title".into();}
    let raw=format!(r#"{{"receipts":[{{"operation_id":"already-done","request_hash":"request","result":{{"createdIds":["original-created"],"revision":"legacy-revision","opaque":1.23000e+42}}}}],"undo":{{"id":"saved-undo","after_revision":{},"before":{},"created":[],"roots":["folder","fav"],"selected":["fav"],"FutureNumber":123456789012345678901234567890}},"FutureState":{{"x":1,"X":2}}}}"#,
        serde_json::to_string(if mode=="obsolete"{"stale-revision"}else{&revision}).unwrap(),serde_json::to_string(&before).unwrap());
    db.execute("INSERT INTO cliphost_kv VALUES('favorite-drag.state.v1',?1)",[&raw]).unwrap();raw
}

#[test]
fn core_migration_rebuilds_effective_structure_undo_and_keeps_receipt_tokens(){
    let root=root();let options=fixture(root.path());let original=save_synthetic_undo(&options,"valid");let before=safe::seal_tree(&options.source_root).unwrap();
    let report=run(&options).unwrap();assert_eq!(report.saved_undo,"converted_structure_only");assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    let db=candidate(&options);let state:String=db.query_row("SELECT value FROM kv WHERE key='favorite-drag.state.v1'",[],|r|r.get(0)).unwrap();
    assert!(state.contains("1.23000e+42"));assert!(state.contains("123456789012345678901234567890"));assert!(state.contains(r#""FutureState":{"x":1,"X":2}"#));
    let old=RawFields::parse(&original).unwrap();let new=RawFields::parse(&state).unwrap();assert_eq!(old.get("receipts").unwrap(),new.get("receipts").unwrap());
    let state:Value=serde_json::from_str(&state).unwrap();let revision=state["undo"]["after_revision"].as_str().unwrap();
    let runtime=crate::storage_core::Runtime{writer:register_writer(&db).unwrap(),root:options.candidate_root.clone(),historical_roots:report.source_aliases};
    let current=runtime.list_favors(&db).unwrap();assert_eq!(revision,crate::host::storage_core_favorite_revision(&current,&["folder".into()]).unwrap());
    let rebuilt:Vec<crate::model::FavorRecord>=serde_json::from_value(state["undo"]["before"].clone()).unwrap();
    let fav=rebuilt.iter().find(|r|r.id=="fav").unwrap();assert_eq!(fav.parent_id,None);assert_eq!(fav.sub_index,7);assert_eq!(fav.create_time.to_rfc3339_opts(chrono::SecondsFormat::Nanos,true),DATE);
    let current=current.iter().find(|r|r.id=="fav").unwrap();assert_eq!(fav.name,current.name);assert_eq!(fav.icon,current.icon);assert_eq!(fav.body_text,current.body_text);
}

#[test]
fn core_migration_preserves_obsolete_undo_and_refuses_effective_content_restore(){
    for mode in ["obsolete","content_change"]{
        let root=root();let options=fixture(root.path());let original=save_synthetic_undo(&options,mode);let before=safe::seal_tree(&options.source_root).unwrap();
        if mode=="obsolete"{let report=run(&options).unwrap();assert_eq!(report.saved_undo,"obsolete_preserved");let db=candidate(&options);let state:String=db.query_row("SELECT value FROM kv WHERE key='favorite-drag.state.v1'",[],|r|r.get(0)).unwrap();assert_eq!(state,original);}
        else{assert!(run(&options).unwrap_err().contains("non-structural"));assert!(!options.candidate_root.exists());}
        assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    }
}

#[test]
fn core_migration_refuses_unknown_saved_before_fields_without_losing_source_state(){
    let root=root();let options=fixture(root.path());save_synthetic_undo(&options,"valid");
    let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
    db.execute("UPDATE cliphost_kv SET value=json_set(value,'$.undo.before[0].FutureUndoSchema',17) WHERE key='favorite-drag.state.v1'",[]).unwrap();drop(db);
    let before=safe::seal_tree(&options.source_root).unwrap();let error=run(&options).unwrap_err();assert!(error.contains("unknown saved-before field"),"{error}");assert!(!options.candidate_root.exists());assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
}

#[test]
fn core_migration_native_column_evidence_is_typed_and_hash_conflicts_fail_closed(){
    for conflict in [false,true]{
        let root=root();let source=root.path().join("source");fs::create_dir(&source).unwrap();let db_path=source.join("userdata.db");
        let store=crate::store::AppStore::open(&db_path).unwrap();let mut clip=crate::model::ClipRecord::new_text("native","fixture");clip.create_time=DateTime::parse_from_rfc3339(DATE).unwrap().with_timezone(&Utc);clip.update_time=clip.create_time;store.insert_clip(&clip, true).unwrap();drop(store);
        let db=Connection::open(&db_path).unwrap();db.execute_batch("ALTER TABLE clip_item ADD COLUMN FutureBlob BLOB; ALTER TABLE clip_item ADD COLUMN FutureReal REAL;").unwrap();db.execute("UPDATE clip_item SET FutureBlob=?1,FutureReal=?2",params![vec![0u8,255,3],1.25f64]).unwrap();
        if conflict{db.execute("UPDATE clip_item SET content_hash='conflicting-index-value'",[]).unwrap();}drop(db);
        let options=MigrationOptions{source_root:source,candidate_root:root.path().join("candidate")};let before=safe::seal_tree(&options.source_root).unwrap();
        if conflict{assert!(run(&options).unwrap_err().contains("CORE_NATIVE_CONTENT_HASH_CONFLICT"));assert!(!options.candidate_root.exists());}
        else{run(&options).unwrap();let db=candidate(&options);let raw:String=db.query_row("SELECT source_extensions_json FROM items",[],|r|r.get(0)).unwrap();let value:Value=serde_json::from_str(&raw).unwrap();assert_eq!(value["nativeColumns"]["FutureBlob"],json!({"type":"blob","value":[0,255,3]}));assert_eq!(value["nativeColumns"]["FutureReal"]["type"],"real_bits");assert!(value["nativeColumns"]["FutureReal"]["value"].is_string());}
        assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    }
}

#[test]
fn core_migration_keeps_offline_wal_commit_and_all_source_sidecar_bytes(){
    let root=root();let mut options=fixture(root.path());let writer=Connection::open(options.source_root.join("userdata.db")).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").unwrap();
    let data=json!({"Id":"wal-commit","CreateTime":DATE,"UpdateTime":DATE,"Item":{"ObjType":1,"Text":"committed in WAL"}});
    writer.execute("INSERT INTO clip_item(Id,Data) VALUES('wal-commit',?1)",[data.to_string()]).unwrap();
    let offline=root.path().join("offline");fs::create_dir_all(offline.join("Configs")).unwrap();fs::create_dir_all(offline.join("Image/nested")).unwrap();
    for relative in ["userdata.db","userdata.db-wal","userdata.db-shm","Configs/FavorConfig.json","Image/nested/shared.png"]{fs::copy(options.source_root.join(relative),offline.join(relative)).unwrap();}
    drop(writer);options.source_root=offline;let before=safe::seal_tree(&options.source_root).unwrap();run(&options).unwrap();assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    let db=candidate(&options);let count:i64=db.query_row("SELECT count(*) FROM items WHERE id='wal-commit'",[],|r|r.get(0)).unwrap();assert_eq!(count,1);
}

#[test]
fn core_migration_accepts_normal_closed_wal_header_without_creating_sidecars(){
    let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; INSERT INTO cliphost_kv VALUES('wal-header','normal-close');").unwrap();drop(db);
    assert!(!options.source_root.join("userdata.db-wal").exists());assert!(!options.source_root.join("userdata.db-shm").exists());
    let bytes=fs::read(options.source_root.join("userdata.db")).unwrap();assert_eq!(&bytes[18..20],&[2,2]);let before=safe::seal_tree(&options.source_root).unwrap();run(&options).unwrap();assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    let db=candidate(&options);let value:String=db.query_row("SELECT value FROM kv WHERE key='wal-header'",[],|r|r.get(0)).unwrap();assert_eq!(value,"normal-close");
}

#[test]
fn core_migration_same_target_lock_rejects_nested_task_without_source_effects(){
    let root=root();let options=fixture(root.path());let before=safe::seal_tree(&options.source_root).unwrap();let mut checked=false;
    build_candidate(&options,&mut|event|{if event.phase=="ready_to_publish"{let error=run(&options).unwrap_err();assert!(error.contains("目录锁"),"{error}");checked=true;}Ok(())}).unwrap();
    assert!(checked);assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
}

#[test]
fn core_migration_forced_process_exit_never_publishes_and_retry_is_independent(){
    let root=root();let options=fixture(root.path());let before=safe::seal_tree(&options.source_root).unwrap();
    let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","storage_core::migrate::tests::core_migration_crash_child","--ignored","--nocapture"])
        .env("CLIPHOST_CORE_MIGRATION_CRASH_ROOT",root.path()).output().unwrap();
    assert_eq!(output.status.code(),Some(77),"{}",String::from_utf8_lossy(&output.stderr));assert!(!options.candidate_root.exists());assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    run(&options).unwrap();assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
}

#[test]
#[ignore="internal subprocess only; requires an explicit synthetic repo.tmp root"]
fn core_migration_crash_child(){
    let Some(root)=std::env::var_os("CLIPHOST_CORE_MIGRATION_CRASH_ROOT") else{return;};let root=PathBuf::from(root);
    let allowed=Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().join(".tmp/storage-core-migration-tests");
    let allowed=safe::checked_existing_root(&allowed).unwrap();let root=safe::checked_existing_root(&root).unwrap();assert!(root.starts_with(&allowed)&&root!=allowed);
    let options=MigrationOptions{source_root:root.join("source"),candidate_root:root.join("candidate")};
    let result=build_candidate(&options,&mut|event|{if event.phase=="ready_to_publish"{std::process::exit(77);}Ok(())});panic!("child failed to reach crash boundary: {result:?}");
}

#[test]
fn core_migration_and_runtime_no_hash_images_share_logical_identity_across_aliases_and_title_edits(){
    let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
    let old_alias=PathBuf::from("Z:/synthetic-never-probed/old-root");
    for(id,path)in[("image-relative","nested/shared.png"),("image-relative-case",r"NESTED\SHARED.PNG"),("image-old-absolute","Z:/synthetic-never-probed/old-root/Image/nested/shared.png")]{
        let data=json!({"Id":id,"Title":null,"CreateTime":DATE,"UpdateTime":DATE,"SourceProcessName":"fixture","Item":{"ObjType":3,"ImagePath":path,"ImageHash":"","PerceptualHash":""}});
        db.execute("INSERT INTO clip_item(Id,Data) VALUES(?1,?2)",params![id,data.to_string()]).unwrap();
    }drop(db);let before=safe::seal_tree(&options.source_root).unwrap();
    let settings=MigrationSettings{source_aliases:vec![old_alias],..Default::default()};build_candidate_with_settings(&options,&settings,&mut |_|Ok(())).unwrap();
    let db=candidate(&options);let mut statement=db.prepare("SELECT dedupe_key FROM items WHERE kind='image' ORDER BY id").unwrap();let keys=statement.query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap();assert_eq!(keys.len(),3);assert!(keys.iter().all(|key|key==&keys[0]),"{keys:?}");
    let logical_data_root=super::super::records::image_dedupe_key(r#"{"sourceLocator":"Image/NESTED/shared.png","locatorBase":"data_root","sourceContentHash":"","perceptualHash":""}"#,&options.candidate_root,&[]).unwrap();assert_eq!(logical_data_root,keys[0]);
    let raw_before:String=db.query_row("SELECT payload_json FROM items WHERE id='image-relative'",[],|r|r.get(0)).unwrap();drop(statement);drop(db);
    let store=crate::store::AppStore::open_isolated_core(&options.candidate_root.join("userdata.db")).unwrap();
    let mut item=store.get_clip("image-relative").unwrap();item.title=Some("edited title".into());store.save_clip(&item).unwrap();store.update_usage("clip_item_image-relative").unwrap();
    let mut duplicate=store.get_clip("image-relative").unwrap();duplicate.id="runtime-image-duplicate".into();duplicate.storage_revision=None;duplicate.title=Some("another title".into());store.insert_clip(&duplicate, true).unwrap();drop(store);
    let db=candidate(&options);for id in ["image-relative","runtime-image-duplicate"]{let key:String=db.query_row("SELECT dedupe_key FROM items WHERE id=?1",[id],|r|r.get(0)).unwrap();assert_eq!(key,keys[0]);}
    let raw_after:String=db.query_row("SELECT payload_json FROM items WHERE id='image-relative'",[],|r|r.get(0)).unwrap();assert_eq!(raw_before,raw_after);assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
}

#[test]
fn core_migration_auxiliary_preserves_not_null_and_constant_defaults(){
    let root=root();let options=fixture(root.path());let source=Connection::open(options.source_root.join("userdata.db")).unwrap();
    source.execute_batch("CREATE TABLE cliphost_collect_draft(id TEXT PRIMARY KEY NOT NULL,payload TEXT NOT NULL DEFAULT 'CHECK(text)',future TEXT NOT NULL DEFAULT 'future-value',optional TEXT DEFAULT NULL,count INTEGER NOT NULL DEFAULT (7)); INSERT INTO cliphost_collect_draft(id) VALUES('existing');").unwrap();
    let metadata=|db:&Connection,table:&str|{let mut q=db.prepare("SELECT name,\"notnull\",dflt_value FROM pragma_table_info(?1) ORDER BY cid").unwrap();let rows=q.query_map([table],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<String>>(2)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap();rows};
    let expected=metadata(&source,"cliphost_collect_draft");drop(source);let before=safe::seal_tree(&options.source_root).unwrap();run(&options).unwrap();assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    let db=Connection::open(options.candidate_root.join("userdata.db")).unwrap();assert_eq!(metadata(&db,"collect_draft"),expected);let writer=register_writer(&db).unwrap();let _lease=writer.enter(WriteScope::Auxiliary).unwrap();
    db.execute("INSERT INTO collect_draft(id) VALUES('new')",[]).unwrap();let values:(String,String,Option<String>,i64)=db.query_row("SELECT payload,future,optional,count FROM collect_draft WHERE id='new'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();assert_eq!(values,("CHECK(text)".into(),"future-value".into(),None,7));
    assert!(db.execute("INSERT INTO collect_draft(id,payload) VALUES('null',NULL)",[]).is_err());
}

#[test]
fn core_migration_auxiliary_rejects_each_unsupported_constraint_before_publication(){
    for(case,ddl,expected)in[
        ("unique","CREATE TABLE cliphost_text_action(id TEXT PRIMARY KEY NOT NULL,payload TEXT NOT NULL UNIQUE)","CORE_AUX_CONSTRAINT_UNSUPPORTED"),
        ("check","CREATE TABLE cliphost_text_action(id TEXT PRIMARY KEY NOT NULL,payload TEXT NOT NULL CHECK(length(payload)>0))","CORE_AUX_CONSTRAINT_UNSUPPORTED"),
        ("foreign","CREATE TABLE cliphost_text_action(id TEXT PRIMARY KEY NOT NULL,payload TEXT NOT NULL REFERENCES cliphost_kv(key))","CORE_AUX_EXECUTABLE_SCHEMA_UNSUPPORTED"),
        ("trigger","CREATE TABLE cliphost_text_action(id TEXT PRIMARY KEY NOT NULL,payload TEXT NOT NULL); CREATE TRIGGER synthetic_aux_guard BEFORE UPDATE ON cliphost_text_action BEGIN SELECT 1; END","CORE_AUX_EXECUTABLE_SCHEMA_UNSUPPORTED"),
        ("generated","CREATE TABLE cliphost_text_action(id TEXT PRIMARY KEY NOT NULL,payload TEXT NOT NULL,future TEXT GENERATED ALWAYS AS(payload) VIRTUAL)","CORE_UNSUPPORTED_GENERATED_COLUMN"),
        ("collation","CREATE TABLE cliphost_text_action(id TEXT COLLATE NOCASE PRIMARY KEY NOT NULL,payload TEXT NOT NULL)","CORE_AUX_CONSTRAINT_UNSUPPORTED"),
        ("default_function","CREATE TABLE cliphost_text_action(id TEXT PRIMARY KEY NOT NULL,payload TEXT NOT NULL DEFAULT (randomblob(16)))","CORE_AUX_DEFAULT_UNSUPPORTED"),
        ("wrong_primary","CREATE TABLE cliphost_text_action(id TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(id,payload))","CORE_AUX_PRIMARY_KEY_UNSUPPORTED"),
    ]{
        let root=root();let options=fixture(root.path());let db=Connection::open(options.source_root.join("userdata.db")).unwrap();db.execute_batch(ddl).unwrap();drop(db);let before=safe::seal_tree(&options.source_root).unwrap();
        let error=run(&options).unwrap_err();assert!(error.contains(expected),"{case}: {error}");assert!(!options.candidate_root.exists(),"{case}");assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before,"{case}");
    }
}

#[test]
fn core_migration_frozen_root_children_rejects_temporary_aba_configuration(){
    let root=root();let options=fixture(root.path());save_synthetic_undo(&options,"valid");
    let config=options.source_root.join("Configs/FavorConfig.json");let original=fs::read(&config).unwrap();let before=safe::seal_tree(&options.source_root).unwrap();let mut changed=false;let mut restored_inside_callback=false;
    let result=build_candidate(&options,&mut|event|{
        if event.phase=="convert"{fs::write(&config,b"{\"RootChildren\":[\"folder\",\"fav\"]}").unwrap();changed=true;}
        // Without the immediate frozen-byte check, this restores the first
        // inventory before final validation while undo used temporary roots.
        if event.phase=="items"&&changed&&!restored_inside_callback{fs::write(&config,&original).unwrap();restored_inside_callback=true;}
        Ok(())
    });
    fs::write(&config,&original).unwrap();assert!(changed);assert!(!restored_inside_callback,"changed RootChildren reached item insertion instead of failing at its read boundary");
    assert!(result.unwrap_err().contains("favorite configuration differs from the frozen snapshot"));assert!(!options.candidate_root.exists());assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
}

fn replace_folder_payload(options:&MigrationOptions,item:&str,duplicate_record_id:bool)->String{
    let extra_id=if duplicate_record_id{r#""Id":"folder","#}else{""};
    let document=format!(r#"{{{extra_id}"Id":"folder","Title":null,"CreateTime":"{DATE}","UpdateTime":"{DATE}","SourceProcessName":"fixture","ParentId":null,"Children":["fav"],"Item":{item}}}"#);
    let db=Connection::open(options.source_root.join("userdata.db")).unwrap();db.execute("UPDATE favor_item SET Data=?1 WHERE Id='folder'",[&document]).unwrap();document
}

#[test]
fn core_migration_adapts_equal_legacy_item_objtype_duplicates_and_preserves_raw_evidence(){
    let root=root();let options=fixture(root.path());
    let item=r#"{ "ObjType":100, "ObjType":100, "FutureNumber":1.23000e+42 }"#;
    let original=replace_folder_payload(&options,item,false);let before=safe::seal_tree(&options.source_root).unwrap();run(&options).unwrap();assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    let db=candidate(&options);let(kind,payload,extensions):(String,String,String)=db.query_row("SELECT kind,payload_json,source_extensions_json FROM items WHERE id='folder'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(kind,"folder");assert_eq!(payload,"{}");assert!(extensions.contains("1.23000e+42"));let evidence:Value=serde_json::from_str(&extensions).unwrap();assert_eq!(evidence["rawItemDocument"],item);
    let archived:String=db.query_row("SELECT Data FROM favor_item WHERE Id='folder'",[],|r|r.get(0)).unwrap();assert_eq!(archived,original);
    let untouched:String=db.query_row("SELECT source_extensions_json FROM items WHERE id='fav'",[],|r|r.get(0)).unwrap();let untouched:Value=serde_json::from_str(&untouched).unwrap();assert!(untouched.get("rawItemDocument").is_none());
}

#[test]
fn core_migration_legacy_discriminator_adapter_rejects_conflicting_or_other_duplicate_keys(){
    for(case,item,duplicate_record_id,expected)in[
        ("conflicting_type",r#"{"ObjType":1,"ObjType":100}"#,false,"CORE_DUPLICATE_ITEM_FIELD"),
        ("string_type",r#"{"ObjType":"100","ObjType":100}"#,false,"CORE_DUPLICATE_ITEM_FIELD"),
        ("other_payload_key",r#"{"ObjType":100,"Future":1,"Future":1}"#,false,"CORE_DUPLICATE_ITEM_FIELD"),
        ("nested_payload_key",r#"{"ObjType":100,"Future":{"x":1,"x":1}}"#,false,"Duplicate JSON key"),
        ("case_alias",r#"{"ObjType":100,"objtype":100}"#,false,"CORE_AMBIGUOUS_KNOWN_FIELD"),
        ("record_key",r#"{"ObjType":100,"ObjType":100}"#,true,"Duplicate JSON key"),
    ]{
        let root=root();let options=fixture(root.path());replace_folder_payload(&options,item,duplicate_record_id);let before=safe::seal_tree(&options.source_root).unwrap();
        let error=run(&options).unwrap_err();assert!(error.contains(expected),"{case}: {error}");assert!(!options.candidate_root.exists(),"{case}");assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before,"{case}");
    }
}

fn csharp_aux_fixture(options:&MigrationOptions)->(String,String){
    let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
    for table in ["explorer_record","process_icon_meta"]{db.execute_batch(&format!("CREATE TABLE {table}(Id NVARCHAR(255) PRIMARY KEY NOT NULL,UpdateTime DATETIME NOT NULL,Data TEXT,ObjectType NVARCHAR(100),CreatedTime DATETIME NOT NULL,FutureBlob BLOB);")).unwrap();}
    let explorer=format!(r#"{{"Id":"legacy-path-key","Path":"Z:/synthetic-no-read/folder","UpdateTime":"{DATE}","FutureNumber":1.23000e+42}}"#);
    let process=format!(r#"{{"Id":"fixture.exe","ExePath":"Z:/synthetic-no-read/legacy.exe","UpdateTime":"{DATE}","FutureNumber":123456789012345678901234567890}}"#);
    for(table,id,data)in[("explorer_record","legacy-path-key",&explorer),("process_icon_meta","fixture.exe",&process)]{db.execute(&format!("INSERT INTO {table} VALUES(?1,'1999-01-01 00:00:00',?2,'synthetic','1999-01-01 00:00:00',?3)"),params![id,data,vec![0u8,255,8]]).unwrap();}
    (explorer,process)
}

#[test]
fn core_migration_csharp_auxiliary_documents_use_business_time_and_keep_complete_source_evidence(){
    let root=root();let options=fixture(root.path());let(explorer,process)=csharp_aux_fixture(&options);
    let source=Connection::open(options.source_root.join("userdata.db")).unwrap();source.execute("INSERT INTO explorer_record(Id,UpdateTime,Data,CreatedTime) VALUES('empty-path','1999-01-01',?1,'1999-01-01')",[json!({"Id":"empty-path","Path":"","UpdateTime":DATE}).to_string()]).unwrap();drop(source);
    let before=safe::seal_tree(&options.source_root).unwrap();let report=run(&options).unwrap();assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    assert_eq!(report.auxiliary_tables["explorer_record"],2);assert_eq!(report.auxiliary_tables["process_icon_meta"],1);
    let expected=DateTime::parse_from_rfc3339(DATE).unwrap().timestamp_millis();let db=candidate(&options);
    let(path,time):(String,i64)=db.query_row("SELECT path,update_time FROM explorer_record WHERE path<>''",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(path,"Z:/synthetic-no-read/folder");assert_eq!(time,expected);
    let empty:i64=db.query_row("SELECT count(*) FROM explorer_record WHERE path=''",[],|r|r.get(0)).unwrap();assert_eq!(empty,1);
    let(id,path,time):(String,String,i64)=db.query_row("SELECT id,exe_path,update_time FROM process_icon_meta",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();assert_eq!(id,"fixture.exe");assert_eq!(path,"Z:/synthetic-no-read/legacy.exe");assert_eq!(time,expected);
    for(table,raw,adapter)in[("explorer_record",explorer,"csharp-explorer-document-v1"),("process_icon_meta",process,"csharp-process-icon-document-v1")]{
        let archived:String=db.query_row(&format!("SELECT Data FROM legacy_source_{table} WHERE FutureBlob IS NOT NULL"),[],|r|r.get(0)).unwrap();assert_eq!(archived,raw);let blob:Vec<u8>=db.query_row(&format!("SELECT FutureBlob FROM legacy_source_{table} WHERE FutureBlob IS NOT NULL"),[],|r|r.get(0)).unwrap();assert_eq!(blob,vec![0,255,8]);
        let source=&report.auxiliary_sources[table];assert_eq!(source.source_table.as_deref(),Some(table));assert_eq!(source.adapter,adapter);assert_eq!(source.archived_tables,vec![format!("legacy_source_{table}")]);
    }
}

#[test]
fn core_migration_existing_prefixed_auxiliary_is_authoritative_even_when_empty(){
    for nonempty in [false,true]{
        let root=root();let options=fixture(root.path());let(explorer,process)=csharp_aux_fixture(&options);let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
        db.execute_batch("CREATE TABLE cliphost_explorer_record(path TEXT PRIMARY KEY NOT NULL,update_time INTEGER NOT NULL); CREATE TABLE cliphost_process_icon_meta(id TEXT PRIMARY KEY NOT NULL,exe_path TEXT NOT NULL,update_time INTEGER NOT NULL);").unwrap();
        if nonempty{db.execute("INSERT INTO cliphost_explorer_record VALUES('Z:/synthetic-no-read/rust-authority',17)",[]).unwrap();db.execute("INSERT INTO cliphost_process_icon_meta VALUES('fixture.exe','Z:/synthetic-no-read/rust.exe',19)",[]).unwrap();}drop(db);
        let before=safe::seal_tree(&options.source_root).unwrap();let report=run(&options).unwrap();assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);let db=candidate(&options);
        for(table,raw)in[("explorer_record",&explorer),("process_icon_meta",&process)]{let count:i64=db.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get(0)).unwrap();assert_eq!(count,i64::from(nonempty));let preserved:String=db.query_row(&format!("SELECT Data FROM legacy_source_{table}"),[],|r|r.get(0)).unwrap();assert_eq!(&preserved,raw);let source=&report.auxiliary_sources[table];assert_eq!(source.source_table.as_deref(),Some(format!("cliphost_{table}").as_str()));assert_eq!(source.adapter,"rust-prefixed-scalar-v1");assert_eq!(source.archived_tables.len(),2);}
        if nonempty{let path:String=db.query_row("SELECT exe_path FROM process_icon_meta",[],|r|r.get(0)).unwrap();assert_eq!(path,"Z:/synthetic-no-read/rust.exe");}
    }
}

#[test]
fn core_migration_auxiliary_document_ambiguity_and_unknown_active_schema_fail_closed(){
    for condition in ["id_mismatch","business_key_collision","unknown_bare","unknown_prefixed"]{
        let root=root();let options=fixture(root.path());csharp_aux_fixture(&options);let db=Connection::open(options.source_root.join("userdata.db")).unwrap();
        let expected=match condition{
            "id_mismatch"=>{db.execute("UPDATE process_icon_meta SET Data=json_set(Data,'$.Id','different')",[]).unwrap();"CORE_AUX_DOCUMENT_INVALID"},
            "business_key_collision"=>{db.execute("INSERT INTO explorer_record(Id,UpdateTime,Data,CreatedTime) SELECT 'another-key',UpdateTime,json_set(Data,'$.Id','another-key'),CreatedTime FROM explorer_record",[]).unwrap();"CORE_AUX_DOCUMENT_KEY_COLLISION"},
            "unknown_bare"=>{db.execute_batch("DROP TABLE explorer_record; CREATE TABLE explorer_record(path TEXT PRIMARY KEY NOT NULL,update_time INTEGER NOT NULL);").unwrap();"CORE_AUX_BARE_SCHEMA_UNSUPPORTED"},
            _=>{db.execute_batch("CREATE TABLE cliphost_process_icon_meta(id TEXT PRIMARY KEY NOT NULL,payload TEXT);").unwrap();"CORE_AUX_SCHEMA_UNSUPPORTED"},
        };drop(db);let before=safe::seal_tree(&options.source_root).unwrap();let error=run(&options).unwrap_err();assert!(error.contains(expected),"{condition}: {error}");assert!(!options.candidate_root.exists());assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    }
}

#[test]
fn core_migration_auxiliary_preserves_nullable_text_primary_key_schema(){
    let root=root();let options=fixture(root.path());let source=Connection::open(options.source_root.join("userdata.db")).unwrap();
    source.execute_batch("CREATE TABLE cliphost_text_action(id TEXT PRIMARY KEY,payload TEXT DEFAULT 'default-payload'); INSERT INTO cliphost_text_action(id,payload) VALUES('existing','original');").unwrap();
    let expected:i64=source.query_row("SELECT \"notnull\" FROM pragma_table_info('cliphost_text_action') WHERE name='id'",[],|r|r.get(0)).unwrap();assert_eq!(expected,0);drop(source);let before=safe::seal_tree(&options.source_root).unwrap();run(&options).unwrap();assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before);
    let db=candidate(&options);let actual:i64=db.query_row("SELECT \"notnull\" FROM pragma_table_info('text_action') WHERE name='id'",[],|r|r.get(0)).unwrap();assert_eq!(actual,expected);
    let value:String=db.query_row("SELECT payload FROM text_action WHERE id='existing'",[],|r|r.get(0)).unwrap();assert_eq!(value,"original");
}

#[test]
fn core_migration_requires_complete_existing_lan_identity_group(){
    for mask in 0u8..8{
        let root=root();let options=fixture(root.path());let dir=options.source_root.join("Configs/LanShare");fs::create_dir_all(&dir).unwrap();
        let cert=b"synthetic certificate bytes";let key=b"synthetic private key bytes";
        let meta=serde_json::to_vec(&json!({"deviceId":"synthetic-device","fingerprint":safe::hash_bytes(cert)})).unwrap();
        for(bit,name,bytes)in[(1,"identity.json",meta.as_slice()),(2,"cert.der",cert.as_slice()),(4,"key.der",key.as_slice())]{if mask&bit!=0{fs::write(dir.join(name),bytes).unwrap();}}
        let before=safe::seal_tree(&options.source_root).unwrap();let result=run(&options);
        if mask==0||mask==7{
            result.unwrap();
            for name in ["identity.json","cert.der","key.der"]{let target=options.candidate_root.join("Configs/LanShare").join(name);if mask==0{assert!(!target.exists(),"first initialization must not happen inside migration");}else{assert_eq!(fs::read(target).unwrap(),fs::read(dir.join(name)).unwrap());}}
        }else{let error=result.unwrap_err();assert!(error.contains("CORE_LAN_IDENTITY_INCOMPLETE"),"mask={mask}: {error}");assert!(!options.candidate_root.exists(),"mask={mask}");}
        assert_eq!(safe::seal_tree(&options.source_root).unwrap(),before,"mask={mask}");
    }
}

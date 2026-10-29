//! Explicitly opened, isolated phase-one storage. Opening an old store never migrates it.
pub mod migrate;
pub(crate) mod records;
pub mod resources;
#[cfg(test)]
mod runtime_tests;

use rusqlite::{functions::FunctionFlags, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

pub const CORE_SCHEMA: &str = include_str!("../../../../docs/storage-v2-core.sql");
pub const AUX_TABLES: &[&str] = &[
    "text_action",
    "lan_peer",
    "lan_conversation",
    "lan_message",
    "collect_draft",
    "kv",
    "explorer_record",
    "process_icon_meta",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteScope {
    History,
    Favorite,
    Migration,
    Auxiliary,
}
impl WriteScope {
    fn code(self) -> u8 {
        match self {
            Self::History => 1,
            Self::Favorite => 2,
            Self::Migration => 3,
            Self::Auxiliary => 4,
        }
    }
    fn from_code(value: u8) -> Option<&'static str> {
        match value {
            1 => Some("history"),
            2 => Some("favorite"),
            3 => Some("migration"),
            4 => Some("auxiliary"),
            _ => None,
        }
    }
}
#[derive(Debug, Clone)]
pub struct WriterState {
    scope: Arc<AtomicU8>,
}
pub struct WriteLease {
    state: Arc<AtomicU8>,
}
impl WriterState {
    pub fn enter(&self, scope: WriteScope) -> Result<WriteLease, String> {
        self.scope
            .compare_exchange(0, scope.code(), Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| "Nested or concurrent core writer scope is not allowed.".to_owned())?;
        Ok(WriteLease {
            state: self.scope.clone(),
        })
    }
}
impl Drop for WriteLease {
    fn drop(&mut self) {
        self.state.store(0, Ordering::SeqCst);
    }
}
pub fn register_writer(connection: &Connection) -> Result<WriterState, String> {
    let state = WriterState {
        scope: Arc::new(AtomicU8::new(0)),
    };
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_INNOCUOUS;
    connection
        .create_scalar_function("cliphost_writer_generation", 0, flags, |_| Ok(2i64))
        .map_err(|e| e.to_string())?;
    let captured = state.scope.clone();
    connection
        .create_scalar_function("cliphost_write_scope", 0, flags, move |_| {
            Ok(WriteScope::from_code(captured.load(Ordering::SeqCst)).map(str::to_owned))
        })
        .map_err(|e| e.to_string())?;
    Ok(state)
}
pub fn schema_checksum() -> String {
    crate::model::sha256_hex(CORE_SCHEMA.as_bytes())
}

/// Called by the isolated migrator after creating/copying all eight domains.
pub fn install_aux_guards(connection: &Connection) -> Result<(), String> {
    for table in AUX_TABLES {
        for (operation, suffix) in [
            ("INSERT", "insert"),
            ("UPDATE", "update"),
            ("DELETE", "delete"),
        ] {
            connection
                .execute_batch(&aux_guard_sql(table, operation, suffix))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
fn aux_guard_sql(table: &str, operation: &str, suffix: &str) -> String {
    format!("CREATE TRIGGER core_{table}_{suffix}_guard BEFORE {operation} ON \"{table}\" BEGIN SELECT CASE WHEN cliphost_writer_generation() IS NOT 2 OR cliphost_write_scope() IS NULL OR cliphost_write_scope() NOT IN ('auxiliary','migration') THEN RAISE(ABORT,'AUX_WRITE_SCOPE_MISMATCH') END; END")
}

pub(crate) struct Runtime {
    pub writer: WriterState,
    pub root: PathBuf,
    pub historical_roots: Vec<PathBuf>,
}
impl Runtime {
    pub fn open(connection: &Connection, path: &Path) -> Result<Self, String> {
        let writer = register_writer(connection)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON; PRAGMA recursive_triggers=ON; PRAGMA trusted_schema=OFF;",
            )
            .map_err(|e| e.to_string())?;
        for pragma in ["foreign_keys", "recursive_triggers"] {
            if connection
                .query_row(&format!("PRAGMA {pragma}"), [], |r| r.get::<_, i64>(0))
                .map_err(|e| e.to_string())?
                != 1
            {
                return Err(format!("Required SQLite option {pragma} is disabled."));
            }
        }
        let info:(String,String,i64,String,i64,String)=connection.query_row("SELECT dataset_id,format_family,format_version,layout_id,min_writer_generation,migration_id FROM store_info WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).map_err(|e|e.to_string())?;
        if info.0.is_empty()
            || info.1 != "cliphost"
            || info.2 != 2
            || info.3 != "core"
            || info.4 != 2
            || info.5.is_empty()
        {
            return Err("Unsupported core layout/writer generation.".into());
        }
        let marker: String = connection
            .query_row(
                "SELECT migration_id FROM storage_core_candidate WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(|_| "A core database requires its explicit isolated-candidate marker.")?;
        if marker != info.5 {
            return Err("Candidate marker does not match the migration identity.".into());
        }
        let migrations: Vec<(i64, String)> = connection
            .prepare("SELECT version,checksum FROM schema_migrations ORDER BY version")
            .map_err(|e| e.to_string())?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        if migrations != vec![(1, schema_checksum())] {
            return Err("Unknown core schema migration/checksum.".into());
        }
        verify_core_schema(connection)?;
        for table in AUX_TABLES {
            let exists: i64 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            if exists != 1 {
                return Err(format!("Missing auxiliary domain {table}."));
            }
            let trigger_count: i64 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND tbl_name=?1",
                    [table],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            if trigger_count != 3 {
                return Err(format!("Unknown auxiliary trigger for {table}."));
            }
            for (operation, suffix) in [
                ("INSERT", "insert"),
                ("UPDATE", "update"),
                ("DELETE", "delete"),
            ] {
                let expected = format!("core_{table}_{suffix}_guard");
                let sql: Option<String> = connection
                    .query_row(
                        "SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1",
                        [expected],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if sql.as_deref() != Some(aux_guard_sql(table, operation, suffix).as_str()) {
                    return Err(format!("Missing auxiliary writer guard for {table}."));
                }
            }
        }
        let violations: i64 = connection
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .map_err(|e| e.to_string())?;
        if violations != 0 {
            return Err("Core foreign key violations.".into());
        }
        let roots: String = connection
            .query_row(
                "SELECT source_roots_json FROM storage_core_candidate WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(|_| "Candidate resource-root provenance is missing.")?;
        let historical_roots: Vec<PathBuf> = serde_json::from_str(&roots)
            .map_err(|_| "Invalid candidate resource-root provenance.")?;
        let runtime = Self {
            writer,
            root: path
                .parent()
                .ok_or("Database has no parent directory")?
                .to_owned(),
            historical_roots,
        };
        resources::ResourceResolver::new(&runtime.root, &runtime.historical_roots)?;
        runtime.validate_all(connection)?;
        Ok(runtime)
    }
}
fn verify_core_schema(connection: &Connection) -> Result<(), String> {
    let expected = Connection::open_in_memory().map_err(|e| e.to_string())?;
    expected
        .execute_batch(CORE_SCHEMA)
        .map_err(|e| e.to_string())?;
    for table in ["items", "usage_info", "store_info", "schema_migrations"] {
        let count = |c: &Connection| {
            c.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND tbl_name=?1",
                [table],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| e.to_string())
        };
        if count(connection)? != count(&expected)? {
            return Err(format!("Unknown core trigger for {table}."));
        }
    }
    let mut q=expected.prepare("SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'").map_err(|e|e.to_string())?;
    for object in q
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
    {
        let (kind, name, sql) = object.map_err(|e| e.to_string())?;
        let actual: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                rusqlite::params![kind, name],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if actual.as_deref() != Some(sql.as_str()) {
            return Err(format!("Core schema object differs: {name}."));
        }
    }
    Ok(())
}

/// A method-local atomic unit. Existing outer drag/undo transactions remain
/// owners of their commit; an error still rolls that whole transaction back.
pub(crate) fn write<T>(
    connection: &Connection,
    state: &WriterState,
    scope: WriteScope,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _lease = state.enter(scope)?;
    connection
        .execute_batch("SAVEPOINT cliphost_core_command")
        .map_err(|e| e.to_string())?;
    struct Pending<'a>(&'a Connection, bool);
    impl Drop for Pending<'_> {
        fn drop(&mut self) {
            if !self.1 {
                let _ = self.0.execute_batch(
                    "ROLLBACK TO cliphost_core_command; RELEASE cliphost_core_command",
                );
            }
        }
    }
    let mut pending = Pending(connection, false);
    let result = operation()?;
    connection
        .execute_batch("RELEASE cliphost_core_command")
        .map_err(|e| e.to_string())?;
    pending.1 = true;
    Ok(result)
}

//! Read only the original Explorer documents in the already selected database.
//! This intentionally bypasses auxiliary SQL rewriting; it never opens a
//! second database, resolves a user path, or modifies the source documents.

use super::{columns, has, invalid, StoreConnection};

pub(crate) struct LegacyExplorerDocument {
    pub id: String,
    pub data: Option<String>,
    pub updated: Option<String>,
}

impl StoreConnection {
    pub(crate) fn legacy_explorer_documents(
        &self,
    ) -> rusqlite::Result<Option<Vec<LegacyExplorerDocument>>> {
        if !self.is_document_store() {
            return Ok(None);
        }
        self.before_read()?;
        let names = columns(&self.inner, "explorer_record")?;
        if names.is_empty() {
            return Ok(None);
        }
        let is_table: bool = self.inner.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type='table' AND name='explorer_record' COLLATE NOCASE)",
            [], |row| row.get(0),
        )?;
        if !is_table || !has(&names, "Id") || !has(&names, "Data") {
            return Err(invalid("旧重开历史表格式无法识别，未导入或改写原表。"));
        }
        let updated = if has(&names, "UpdateTime") {
            "CAST(UpdateTime AS TEXT)"
        } else {
            "NULL"
        };
        let mut statement = self.inner.prepare(&format!(
            "SELECT Id,Data,{updated} FROM main.explorer_record ORDER BY Id"
        ))?;
        let rows = statement.query_map([], |row| {
            Ok(LegacyExplorerDocument {
                id: row.get(0)?,
                data: row.get(1)?,
                updated: row.get(2)?,
            })
        })?;
        Ok(Some(rows.collect::<rusqlite::Result<Vec<_>>>()?))
    }
}

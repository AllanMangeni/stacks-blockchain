//! Source/output DB open-options and read helpers used during squash.

use std::path::{Path, PathBuf};

use stacks_common::types::chainstate::StacksBlockId;
use stackslib::chainstate::stacks::index::marf::MARFOpenOpts;
use stackslib::util_lib::db::sqlite_open;

use crate::layout::epoch2_block_rel_path;

/// Open-opts for read-only scanning a MARF during squash (deferred hashing, no
/// cache). `external_blobs` is detected from the sidecar `.blobs`: present for
/// Clarity/Index and squashed sortitions, absent for an archival sortition.
pub fn read_marf_open_opts(db_path: &Path) -> MARFOpenOpts {
    let mut open_opts = MARFOpenOpts::default();
    open_opts.external_blobs = PathBuf::from(format!("{}.blobs", db_path.display())).exists();
    open_opts
}

/// Network identity read from a chainstate index DB's `db_config`.
pub struct DbConfig {
    pub mainnet: bool,
    pub chain_id: u32,
}

/// Read the network identity from an open index-DB connection's `db_config`.
pub fn read_db_config_from_conn(conn: &rusqlite::Connection) -> DbConfig {
    let (mainnet_i, chain_id): (i64, i64) = conn
        .query_row(
            "SELECT mainnet, chain_id FROM db_config LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or_else(|e| {
            eprintln!("Failed to read db_config: {e}");
            std::process::exit(1);
        });
    DbConfig {
        mainnet: mainnet_i != 0,
        chain_id: chain_id as u32,
    }
}

/// Read the network identity from the index DB at `index_db_path` (read-only).
pub fn read_db_config(index_db_path: &Path) -> DbConfig {
    // Read-only: never touch or create files on the source chainstate.
    let conn = sqlite_open(
        index_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        false,
    )
    .unwrap_or_else(|e| {
        eprintln!(
            "Failed to open index DB '{}' for db_config: {e}",
            index_db_path.display()
        );
        std::process::exit(1);
    });
    read_db_config_from_conn(&conn)
}

/// The expected relative paths of the canonical epoch-2.x block files, derived
/// from the (squashed) index DB's `block_headers`.
pub fn derive_expected_epoch2_block_rel_paths(
    conn: &rusqlite::Connection,
) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT index_block_hash, block_height FROM block_headers ORDER BY block_height")
        .map_err(|e| format!("prepare block_headers query: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, StacksBlockId>(0)?, row.get::<_, u64>(1)?))
        })
        .map_err(|e| format!("query block_headers: {e}"))?;

    let mut rel_paths = Vec::new();
    for row in rows {
        let (index_block_hash, block_height) =
            row.map_err(|e| format!("read block_headers row: {e}"))?;
        if block_height == 0 {
            continue;
        }
        rel_paths.push(epoch2_block_rel_path(&index_block_hash));
    }

    Ok(rel_paths)
}

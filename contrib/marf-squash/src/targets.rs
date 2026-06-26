// Copyright (C) 2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Resolution of the canonical Stacks + sortition boundaries that a squash must
//! anchor on.

use stacks_common::types::chainstate::{
    BlockHeaderHash, ConsensusHash, SortitionId, StacksBlockId,
};
use stackslib::burnchains::PoxConstants;
use stackslib::chainstate::burn::BlockSnapshot;
use stackslib::chainstate::burn::db::sortdb::{SortitionDB, SortitionHandleConn};
use stackslib::chainstate::nakamoto::NakamotoChainState;
use stackslib::chainstate::stacks::db::{StacksChainState, StacksHeaderInfo};

/// Canonical Stacks and sortition MARF boundaries that must be squashed together.
#[derive(Debug, Clone)]
pub struct CanonicalSquashTargets {
    /// Tenure-start Stacks block height that Clarity and Index squash to.
    pub stacks_height: u32,
    /// Tenure-start Stacks tip that Clarity and Index squash to.
    pub stacks_tip: StacksBlockId,
    /// Consensus hash of the tenure-start tip.
    pub stacks_tip_consensus_hash: ConsensusHash,
    /// Burn view consensus hash shared by the tenure-start and tenure-end headers.
    pub stacks_tip_burn_view_consensus_hash: ConsensusHash,
    /// Block hash of the tenure-start tip.
    pub stacks_tip_block_hash: BlockHeaderHash,
    /// Bitcoin height of the squash boundary (the tenure tip's `burn_view`).
    pub squash_bitcoin_height: u32,
    /// Sortition MARF height for `squash_bitcoin_height`.
    pub sortition_marf_height: u32,
    /// Source canonical burn tip - the fork selector for `MARF::squash_to_path`.
    pub sortition_canonical_tip: SortitionId,
    /// Squash boundary sortition (at `squash_bitcoin_height`) that the squashed output holds.
    pub sortition_boundary_tip: SortitionId,
    /// Sortition DB's first Bitcoin block height (used to derive MARF heights).
    pub first_bitcoin_height: u32,
}

fn find_tenure_start_ancestor_from_end(
    chainstate: &StacksChainState,
    tenure_ch: &ConsensusHash,
    end_header: &StacksHeaderInfo,
) -> Result<StacksHeaderInfo, String> {
    if end_header.consensus_hash != *tenure_ch {
        return Err(format!(
            "Tenure CH mismatch: tenure-start sortition has CH {tenure_ch}, but \
             tenure end header has CH {}.",
            end_header.consensus_hash
        ));
    }

    let mut cursor = end_header.clone();
    for _ in 0..=end_header.stacks_block_height {
        let cursor_id = cursor.index_block_hash();
        let parent_id =
            NakamotoChainState::get_nakamoto_parent_block_id(chainstate.db(), &cursor_id)
                .map_err(|e| format!("Failed to load parent of Nakamoto block {cursor_id}: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "Nakamoto block {cursor_id} at height {} has no parent_block_id row",
                        cursor.stacks_block_height
                    )
                })?;

        let Some(parent_header) = NakamotoChainState::get_block_header(chainstate.db(), &parent_id)
            .map_err(|e| format!("Failed to load parent header {parent_id}: {e}"))?
        else {
            return Err(format!(
                "Nakamoto block {cursor_id} at height {} points to missing parent {parent_id}",
                cursor.stacks_block_height
            ));
        };

        if parent_header.consensus_hash != *tenure_ch {
            return Ok(cursor);
        }

        if parent_header.stacks_block_height >= cursor.stacks_block_height {
            return Err(format!(
                "Nakamoto ancestry is not height-decreasing: parent {parent_id} height {} \
                 is not below child {cursor_id} height {}",
                parent_header.stacks_block_height, cursor.stacks_block_height
            ));
        }

        cursor = parent_header;
    }

    Err(format!(
        "Could not find tenure start ancestor for tenure {tenure_ch} before walking \
         {} parent links from {}",
        end_header.stacks_block_height,
        end_header.index_block_hash()
    ))
}

/// Build the error for a `--tenure-start-bitcoin-height` that did not start a
/// Nakamoto tenure, listing nearby tenure starts to help the caller pick one.
fn format_no_tenure_start_error(
    sortition_db: &SortitionDB,
    canonical_sortition_id: &SortitionId,
    start_height: u64,
) -> String {
    let ic = sortition_db.index_handle_at_tip();
    // Find nearby tenure starts for a helpful error message.
    let mut nearby = Vec::new();
    let search_radius = 10u64;
    let search_start = start_height.saturating_sub(search_radius);
    let search_end = start_height.saturating_add(search_radius);
    for h in search_start..=search_end {
        if h == start_height {
            continue;
        }
        if let Ok(Some(s)) = SortitionDB::get_ancestor_snapshot(&ic, h, canonical_sortition_id)
            && s.sortition
        {
            nearby.push(h);
        }
    }
    let nearby_str = if nearby.is_empty() {
        String::new()
    } else {
        format!(
            "\n  Nearby tenure starts: {}",
            nearby
                .iter()
                .map(|h| h.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "Bitcoin height {start_height} did not start a Nakamoto tenure \
         (sortition=false).{nearby_str}"
    )
}

/// STRICT BOUNDARY PREDICATE: assert the tenure starting at `start_height` is
/// exactly one burn block long.
///
/// The squashed boot path is simple iff sub-boundary MARF reads never fire. For
/// the sortition MARF, that holds when the tenure being squashed is exactly one
/// burn block long: in that case the next post-boundary block's
/// `parent_sortition_id` lands AT the boundary, not below. The constraint is:
/// the burn block immediately after `start_height` has its own sortition (i.e.
/// starts a NEW tenure), so the current tenure does not extend forward via flash
/// blocks.
///
/// The boundary is the first block in the tenure; intra-tenure descendants above
/// it are dropped from the artifact and re-synced from peers on boot. Sortition
/// remains anchored at the tenure's burn view, whose runtime canonical tip is the
/// highest canonical block in the tenure.
fn assert_single_burn_block_tenure(
    sortition_db: &SortitionDB,
    canonical_sortition_id: &SortitionId,
    start_height: u64,
) -> Result<(), String> {
    let ic = sortition_db.index_handle_at_tip();
    let next_height = start_height
        .checked_add(1)
        .ok_or_else(|| format!("Bitcoin height {start_height} + 1 overflows u64"))?;
    let next_snapshot =
        SortitionDB::get_ancestor_snapshot(&ic, next_height, canonical_sortition_id)
            .map_err(|e| {
                format!("Failed to get ancestor snapshot at Bitcoin height {next_height}: {e}")
            })?
            .ok_or_else(|| {
                format!(
                    "No canonical sortition at Bitcoin height {next_height}. The chain has \
                     not yet progressed past `tenure_start_bitcoin_height`; pick a boundary \
                     that is at least one burn block behind the canonical tip."
                )
            })?;
    if !next_snapshot.sortition {
        return Err(format!(
            "Bitcoin height {start_height} starts a Nakamoto tenure that extends \
             into burn block {next_height} via a flash block (sortition=false). \
             Refusing to squash: the tenure must be exactly one burn block long. \
             Bump --tenure-start-bitcoin-height to a tenure followed by its own \
             sortition."
        ));
    }
    Ok(())
}

/// Inputs to [`resolve_canonical_squash_targets`]: the source DB locations, the
/// network identity, the tenure-start Bitcoin height, and the PoX constants.
pub struct SquashTargetQuery<'a> {
    pub chainstate_root: &'a str,
    pub sortition_db_dir: &'a str,
    pub tenure_start_bitcoin_height: u32,
    pub mainnet: bool,
    pub chain_id: u32,
    pub pox_constants: PoxConstants,
}

/// The sortition read context shared by the boundary-resolution steps: the open
/// sortition DB, an index handle at its tip, and the canonical burn-chain tip
/// that ancestor lookups are anchored on.
struct SortitionReadCtx<'a> {
    sortition_db: &'a SortitionDB,
    ic: &'a SortitionHandleConn<'a>,
    canonical_tip: &'a BlockSnapshot,
}

/// Open the source chainstate and sortition DBs, and read the sortition DB's
/// `first_block_height` (as u32) and canonical burn-chain tip. Returns
/// `(chainstate, sortition_db, first_bitcoin_height, canonical_tip)`.
fn open_squash_dbs(
    query: &SquashTargetQuery,
) -> Result<(StacksChainState, SortitionDB, u32, BlockSnapshot), String> {
    let (chainstate, _) =
        StacksChainState::open(query.mainnet, query.chain_id, query.chainstate_root, None)
            .map_err(|e| {
                format!(
                    "Failed to open chainstate at '{}': {e}",
                    query.chainstate_root
                )
            })?;

    let sortition_db = SortitionDB::open(
        query.sortition_db_dir,
        false,
        query.pox_constants.clone(),
        None,
    )
    .map_err(|e| {
        format!(
            "Failed to open sortition DB at '{}': {e}",
            query.sortition_db_dir
        )
    })?;

    let first_bitcoin_height: u32 = sortition_db.first_block_height.try_into().map_err(|_| {
        format!(
            "Sortition first_block_height {} does not fit in u32",
            sortition_db.first_block_height
        )
    })?;

    let canonical_tip = SortitionDB::get_canonical_burn_chain_tip(sortition_db.conn())
        .map_err(|e| format!("Failed to get canonical burn tip: {e}"))?;

    Ok((
        chainstate,
        sortition_db,
        first_bitcoin_height,
        canonical_tip,
    ))
}

/// Resolve the tenure-start sortition snapshot at `start_height`, requiring it to
/// have started a Nakamoto tenure (`sortition=true`).
fn resolve_tenure_start_snapshot(
    ctx: &SortitionReadCtx,
    start_height: u64,
) -> Result<BlockSnapshot, String> {
    let start_snapshot =
        SortitionDB::get_ancestor_snapshot(ctx.ic, start_height, &ctx.canonical_tip.sortition_id)
            .map_err(|e| {
                format!("Failed to get ancestor snapshot at Bitcoin height {start_height}: {e}")
            })?
            .ok_or_else(|| format!("No canonical sortition at Bitcoin height {start_height}"))?;

    if !start_snapshot.sortition {
        return Err(format_no_tenure_start_error(
            ctx.sortition_db,
            &ctx.canonical_tip.sortition_id,
            start_height,
        ));
    }
    Ok(start_snapshot)
}

/// Find the highest known Nakamoto block header in the tenure starting at
/// `start_height`, requiring its consensus hash to match the tenure-start
/// sortition's.
fn resolve_tenure_end_header(
    chainstate: &StacksChainState,
    sortition_db: &SortitionDB,
    start_height: u64,
    tenure_ch: &ConsensusHash,
) -> Result<StacksHeaderInfo, String> {
    let end_header = NakamotoChainState::find_highest_known_block_header_in_tenure_by_block_height(
        chainstate,
        sortition_db,
        start_height,
    )
    .map_err(|e| format!("Failed to find tenure end at Bitcoin height {start_height}: {e}"))?
    .ok_or_else(|| {
        format!(
            "No Nakamoto blocks found at Bitcoin height {start_height}. \
             This may predate Nakamoto activation."
        )
    })?;

    if end_header.consensus_hash != *tenure_ch {
        return Err(format!(
            "Tenure CH mismatch: tenure-start sortition has CH {tenure_ch}, but \
             highest known block in tenure has CH {}. Likely a re-org between \
             the start sortition and the block headers table.",
            end_header.consensus_hash
        ));
    }
    Ok(end_header)
}

/// Extract and validate the tenure's burn_view: both the tenure-start and
/// tenure-end headers must carry a `burn_view`, and the two must agree.
/// `stacks_height` is the (already u32-checked) tenure-start height, used only in
/// error messages.
fn resolve_tenure_burn_view(
    start_header: &StacksHeaderInfo,
    end_header: &StacksHeaderInfo,
    stacks_height: u32,
) -> Result<ConsensusHash, String> {
    let stacks_tip = start_header.index_block_hash();
    let end_stacks_tip = end_header.index_block_hash();
    let header_burn_view = start_header.burn_view.clone().ok_or_else(|| {
        format!(
            "Nakamoto tenure start {stacks_tip} (height {stacks_height}) has no \
             burn_view set. Squash requires a Nakamoto block header with a \
             burn_view."
        )
    })?;
    let end_header_burn_view = end_header.burn_view.clone().ok_or_else(|| {
        format!(
            "Nakamoto tenure end {end_stacks_tip} (height {}) has no \
             burn_view set. Squash requires a Nakamoto block header with a \
             burn_view.",
            end_header.stacks_block_height
        )
    })?;
    if end_header_burn_view != header_burn_view {
        return Err(format!(
            "Tenure burn_view mismatch: tenure start {stacks_tip} has \
             burn_view {header_burn_view}, but tenure end {end_stacks_tip} \
             has burn_view {end_header_burn_view}. Refusing to split the \
             Stacks and sortition squash anchors across different burn views."
        ));
    }
    Ok(header_burn_view)
}

/// Load the burn_view's sortition snapshot, confirm it sits on the canonical
/// burn-chain fork, and resolve the squash Bitcoin height (bounded by both the
/// tenure-start height and the sortition DB's first block height). Returns
/// `(burn_view_snapshot, squash_bitcoin_height)`.
fn resolve_squash_burn_view_snapshot(
    ctx: &SortitionReadCtx,
    header_burn_view: &ConsensusHash,
    tenure_start_bitcoin_height: u32,
    first_bitcoin_height: u32,
) -> Result<(BlockSnapshot, u32), String> {
    let burn_view_snapshot =
        SortitionDB::get_block_snapshot_consensus(ctx.sortition_db.conn(), header_burn_view)
            .map_err(|e| format!("Failed to load snapshot for burn_view {header_burn_view}: {e}"))?
            .ok_or_else(|| {
                format!("No snapshot found for tenure start's burn_view {header_burn_view}")
            })?;

    let canonical_at_height = SortitionDB::get_ancestor_snapshot(
        ctx.ic,
        burn_view_snapshot.block_height,
        &ctx.canonical_tip.sortition_id,
    )
    .map_err(|e| {
        format!(
            "Failed to get canonical ancestor at Bitcoin height {}: {e}",
            burn_view_snapshot.block_height
        )
    })?
    .ok_or_else(|| {
        format!(
            "No canonical sortition at Bitcoin height {} (burn_view {header_burn_view})",
            burn_view_snapshot.block_height
        )
    })?;

    if canonical_at_height.sortition_id != burn_view_snapshot.sortition_id {
        return Err(format!(
            "Tenure start's burn_view {header_burn_view} points at sortition_id \
             {} (Bitcoin height {}), which is not on the canonical burn-chain \
             fork. Cowardly refusing to squash a tenure that landed on an \
             orphan burn fork.",
            burn_view_snapshot.sortition_id, burn_view_snapshot.block_height
        ));
    }

    let squash_bitcoin_height: u32 = burn_view_snapshot.block_height.try_into().map_err(|_| {
        format!(
            "Tenure burn-view Bitcoin height {} does not fit in u32",
            burn_view_snapshot.block_height
        )
    })?;
    if squash_bitcoin_height < tenure_start_bitcoin_height {
        return Err(format!(
            "Tenure burn-view Bitcoin height {squash_bitcoin_height} is earlier than \
             tenure-start Bitcoin height {tenure_start_bitcoin_height}. Refusing to squash."
        ));
    }
    if squash_bitcoin_height < first_bitcoin_height {
        return Err(format!(
            "Tenure burn-view Bitcoin height {squash_bitcoin_height} is below the \
             sortition DB's first_bitcoin_height {first_bitcoin_height}"
        ));
    }

    Ok((burn_view_snapshot, squash_bitcoin_height))
}

/// Confirm the sortition boundary snapshot reconstructs the tenure-end canonical
/// Stacks tip: the runtime tip's StacksBlockId, height, and burn_view must all
/// match the tenure end / tenure burn_view. Catches a boundary that would leave
/// the destination internally inconsistent.
fn validate_runtime_tip_reconstruction(
    sortition_db: &SortitionDB,
    burn_view_snapshot: &BlockSnapshot,
    end_header: &StacksHeaderInfo,
    header_burn_view: &ConsensusHash,
) -> Result<(), String> {
    let end_stacks_tip = end_header.index_block_hash();
    let sortition_boundary_id = &burn_view_snapshot.sortition_id;

    // The sortition boundary snapshot must resolve back to the tenure-end
    // canonical Stacks tip, even though Clarity/Index are anchored at the
    // tenure-start block.
    let runtime_tip = SortitionDB::get_canonical_nakamoto_tip_hash_and_height_and_burn_view(
        sortition_db.conn(),
        burn_view_snapshot,
    )
    .map_err(|e| {
        format!(
            "Failed to resolve runtime canonical Nakamoto tip from boundary \
             snapshot {sortition_boundary_id}: {e}"
        )
    })?
    .ok_or_else(|| {
        format!(
            "Runtime canonical Nakamoto tip resolution returned None from \
             boundary snapshot {sortition_boundary_id}"
        )
    })?;

    let (runtime_ch, runtime_burn_view, runtime_bhh, runtime_height) = runtime_tip;
    let runtime_block_id = StacksBlockId::new(&runtime_ch, &runtime_bhh);
    if runtime_block_id != end_stacks_tip {
        return Err(format!(
            "Runtime canonical tip reconstruction mismatch: boundary snapshot \
             {sortition_boundary_id} resolves to StacksBlockId {runtime_block_id} \
             at height {runtime_height}, but the tenure end header is \
             StacksBlockId {end_stacks_tip} at height {}. The squash boundary \
             would leave the destination internally inconsistent.",
            end_header.stacks_block_height
        ));
    }
    if runtime_height != end_header.stacks_block_height {
        return Err(format!(
            "Runtime canonical tip height mismatch: boundary snapshot resolves \
             to height {runtime_height} but tenure tip is at height \
             {}",
            end_header.stacks_block_height
        ));
    }
    // The block id can match even when the burn-view mapping is corrupt.
    if runtime_burn_view != *header_burn_view {
        return Err(format!(
            "Runtime canonical tip burn_view mismatch: boundary snapshot \
             resolves to burn_view {runtime_burn_view} but the tenure \
             burn_view is {header_burn_view}. The sortition DB's \
             `stacks_chain_tips_by_burn_view` row for sortition \
             {sortition_boundary_id} is internally inconsistent."
        ));
    }
    Ok(())
}

/// Resolve the canonical Stacks and sortition boundaries to squash to from the
/// tenure starting at `query.tenure_start_bitcoin_height`. Validates that the
/// tenure is a single burn block long, that its burn view is canonical and
/// consistent across the tenure-start and tenure-end headers, and that the
/// boundary reconstructs the tenure-end runtime tip.
pub fn resolve_canonical_squash_targets(
    query: SquashTargetQuery,
) -> Result<CanonicalSquashTargets, String> {
    let tenure_start_bitcoin_height = query.tenure_start_bitcoin_height;
    let start_height = u64::from(tenure_start_bitcoin_height);

    let (chainstate, sortition_db, first_bitcoin_height, canonical_tip) = open_squash_dbs(&query)?;

    let ic = sortition_db.index_handle_at_tip();
    let ctx = SortitionReadCtx {
        sortition_db: &sortition_db,
        ic: &ic,
        canonical_tip: &canonical_tip,
    };
    let start_snapshot = resolve_tenure_start_snapshot(&ctx, start_height)?;

    let tenure_ch = start_snapshot.consensus_hash.clone();

    // The squash boundary must be exactly one burn block long (see the predicate
    // doc); otherwise the squashed boot path would need sub-boundary MARF reads.
    assert_single_burn_block_tenure(&sortition_db, &canonical_tip.sortition_id, start_height)?;

    let end_header =
        resolve_tenure_end_header(&chainstate, &sortition_db, start_height, &tenure_ch)?;

    let start_header = find_tenure_start_ancestor_from_end(&chainstate, &tenure_ch, &end_header)?;

    if start_header.stacks_block_height > end_header.stacks_block_height {
        return Err(format!(
            "Tenure block ordering mismatch: tenure start height {} is greater \
             than tenure end height {} at Bitcoin height {start_height}.",
            start_header.stacks_block_height, end_header.stacks_block_height
        ));
    }

    let stacks_height: u32 = start_header.stacks_block_height.try_into().map_err(|_| {
        format!(
            "Tenure start Stacks height {} does not fit in u32",
            start_header.stacks_block_height
        )
    })?;
    let stacks_tip = start_header.index_block_hash();
    let stacks_tip_consensus_hash = start_header.consensus_hash.clone();
    let stacks_tip_block_hash = start_header.anchored_header.block_hash();

    let header_burn_view = resolve_tenure_burn_view(&start_header, &end_header, stacks_height)?;

    let (burn_view_snapshot, squash_bitcoin_height) = resolve_squash_burn_view_snapshot(
        &ctx,
        &header_burn_view,
        tenure_start_bitcoin_height,
        first_bitcoin_height,
    )?;

    let sortition_marf_height = squash_bitcoin_height - first_bitcoin_height;

    let sortition_canonical_tip = canonical_tip.sortition_id.clone();
    let sortition_boundary_tip = burn_view_snapshot.sortition_id.clone();

    validate_runtime_tip_reconstruction(
        &sortition_db,
        &burn_view_snapshot,
        &end_header,
        &header_burn_view,
    )?;

    Ok(CanonicalSquashTargets {
        stacks_height,
        stacks_tip,
        stacks_tip_consensus_hash,
        stacks_tip_burn_view_consensus_hash: header_burn_view,
        stacks_tip_block_hash,
        squash_bitcoin_height,
        sortition_marf_height,
        sortition_canonical_tip,
        sortition_boundary_tip,
        first_bitcoin_height,
    })
}

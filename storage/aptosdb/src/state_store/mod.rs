// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//! This file defines state store APIs that are related account state Merkle tree.

use crate::{
    common::NUM_STATE_SHARDS,
    ledger_db::LedgerDb,
    metrics::{OTHER_TIMERS_SECONDS, STATE_ITEMS, TOTAL_STATE_BYTES},
    pruner::{PrunerManager, StateKvPrunerManager, StateMerklePrunerManager},
    schema::{
        db_metadata::{DbMetadataKey, DbMetadataSchema, DbMetadataValue},
        stale_node_index::StaleNodeIndexSchema,
        stale_node_index_cross_epoch::StaleNodeIndexCrossEpochSchema,
        stale_state_value_index::StaleStateValueIndexSchema,
        state_value::StateValueSchema,
        state_value_index::StateValueIndexSchema,
        version_data::VersionDataSchema,
    },
    state_kv_db::StateKvDb,
    state_merkle_db::StateMerkleDb,
    state_restore::{
        StateSnapshotProgress, StateSnapshotRestore, StateSnapshotRestoreMode, StateValueWriter,
    },
    state_store::buffered_state::BufferedState,
    utils::{
        iterators::PrefixedStateValueIterator,
        new_sharded_kv_schema_batch,
        truncation_helper::{truncate_ledger_db, truncate_state_kv_db},
        ShardedStateKvSchemaBatch,
    },
};
use anyhow::Context;
use aptos_crypto::{
    hash::{CryptoHash, SPARSE_MERKLE_PLACEHOLDER_HASH},
    HashValue,
};
use aptos_executor::components::in_memory_state_calculator_v2::InMemoryStateCalculatorV2;
use aptos_experimental_runtimes::thread_manager::THREAD_MANAGER;
use aptos_infallible::Mutex;
use aptos_jellyfish_merkle::iterator::JellyfishMerkleIterator;
use aptos_logger::info;
use aptos_schemadb::{schema::SeekKeyCodec, ReadOptions, SchemaBatch};
use aptos_scratchpad::{SmtAncestors, SparseMerkleTree};
use aptos_storage_interface::{
    async_proof_fetcher::AsyncProofFetcher,
    cached_state_view::{CachedStateView, ShardedStateCache},
    db_ensure as ensure,
    state_delta::StateDelta,
    AptosDbError, DbReader, Result, StateSnapshotReceiver,
};
use aptos_types::{
    proof::{definition::LeafCount, SparseMerkleProofExt, SparseMerkleRangeProof},
    state_store::{
        create_empty_sharded_state_updates,
        state_key::{prefix::StateKeyPrefix, StateKey},
        state_storage_usage::StateStorageUsage,
        state_value::{StaleStateValueIndex, StateValue, StateValueChunkWithProof},
        ShardedStateUpdates, StateViewId,
    },
    transaction::Version,
    write_set::{TransactionWrite, WriteSet},
};
use claims::{assert_ge, assert_le};
use rayon::prelude::*;
use std::{collections::HashSet, ops::Deref, sync::Arc};

pub(crate) mod buffered_state;
mod state_merkle_batch_committer;
mod state_snapshot_committer;

#[cfg(test)]
mod state_store_test;

type StateValueBatch = crate::state_restore::StateValueBatch<StateKey, Option<StateValue>>;

// We assume TARGET_SNAPSHOT_INTERVAL_IN_VERSION > block size.
const MAX_WRITE_SETS_AFTER_SNAPSHOT: LeafCount = buffered_state::TARGET_SNAPSHOT_INTERVAL_IN_VERSION
    * (buffered_state::ASYNC_COMMIT_CHANNEL_BUFFER_SIZE + 2 + 1/*  Rendezvous channel */)
    * 2;

pub const MAX_COMMIT_PROGRESS_DIFFERENCE: u64 = 100000;

pub(crate) struct StateDb {
    pub ledger_db: Arc<LedgerDb>,
    pub state_merkle_db: Arc<StateMerkleDb>,
    pub state_kv_db: Arc<StateKvDb>,
    pub state_merkle_pruner: StateMerklePrunerManager<StaleNodeIndexSchema>,
    pub epoch_snapshot_pruner: StateMerklePrunerManager<StaleNodeIndexCrossEpochSchema>,
    pub state_kv_pruner: StateKvPrunerManager,
    pub skip_usage: bool,
}

pub(crate) struct StateStore {
    pub state_db: Arc<StateDb>,
    // The `base` of buffered_state is the latest snapshot in state_merkle_db while `current`
    // is the latest state sparse merkle tree that is replayed from that snapshot until the latest
    // write set stored in ledger_db.
    buffered_state: Mutex<BufferedState>,
    buffered_state_target_items: usize,
    smt_ancestors: Mutex<SmtAncestors<StateValue>>,
}

impl Deref for StateStore {
    type Target = StateDb;

    fn deref(&self) -> &Self::Target {
        self.state_db.deref()
    }
}

// "using an Arc<dyn DbReader> as an Arc<dyn StateReader>" is not allowed in stable Rust. Actually we
// want another trait, `StateReader`, which is a subset of `DbReader` here but Rust does not support trait
// upcasting coercion for now. Should change it to a different trait once upcasting is stabilized.
// ref: https://github.com/rust-lang/rust/issues/65991
impl DbReader for StateDb {
    /// Returns the latest state snapshot strictly before `next_version` if any.
    fn get_state_snapshot_before(
        &self,
        next_version: Version,
    ) -> Result<Option<(Version, HashValue)>> {
        self.state_merkle_db
            .get_state_snapshot_version_before(next_version)?
            .map(|ver| Ok((ver, self.state_merkle_db.get_root_hash(ver)?)))
            .transpose()
    }

    /// Get the latest state value of the given key up to the given version. Only used for testing for now
    /// but should replace the `get_value_with_proof_by_version` call for VM execution if just fetch the
    /// value without proof.
    fn get_state_value_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<Option<StateValue>> {
        Ok(self
            .get_state_value_with_version_by_version(state_key, version)?
            .map(|(_, value)| value))
    }

    /// Gets the latest state value and its corresponding version when it's of the given key up
    /// to the given version.
    fn get_state_value_with_version_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<Option<(Version, StateValue)>> {
        let mut read_opts = ReadOptions::default();
        // We want `None` if the state_key changes in iteration.
        read_opts.set_prefix_same_as_start(true);
        // We don't need to look beyond the prune window. And it's a potential performance issue
        // if we do -- beyond the edge of the pruning window we will see only deletions.
        read_opts.set_iterate_upper_bound(SeekKeyCodec::<StateValueSchema>::encode_seek_key(&(
            state_key.clone(),
            self.state_kv_pruner.get_min_readable_version(),
        ))?);
        let mut iter = self
            .state_kv_db
            .db_shard(state_key.get_shard_id())
            .iter::<StateValueSchema>(read_opts)?;
        iter.seek(&(state_key.clone(), version))?;
        Ok(iter
            .next()
            .transpose()?
            .and_then(|((_, version), value_opt)| value_opt.map(|value| (version, value))))
    }

    /// Returns the proof of the given state key and version.
    fn get_state_proof_by_version_ext(
        &self,
        state_key: &StateKey,
        version: Version,
        root_depth: usize,
    ) -> Result<SparseMerkleProofExt> {
        let (_, proof) = self
            .state_merkle_db
            .get_with_proof_ext(state_key, version, root_depth)?;
        Ok(proof)
    }

    /// Get the state value with proof given the state key and version
    fn get_state_value_with_proof_by_version_ext(
        &self,
        state_key: &StateKey,
        version: Version,
        root_depth: usize,
    ) -> Result<(Option<StateValue>, SparseMerkleProofExt)> {
        let (leaf_data, proof) = self
            .state_merkle_db
            .get_with_proof_ext(state_key, version, root_depth)?;
        Ok((
            match leaf_data {
                Some((_, (key, version))) => Some(self.expect_value_by_version(&key, version)?),
                None => None,
            },
            proof,
        ))
    }

    fn get_state_storage_usage(&self, version: Option<Version>) -> Result<StateStorageUsage> {
        version.map_or(Ok(StateStorageUsage::zero()), |version| {
            Ok(match self.ledger_db.metadata_db().get_usage(version) {
                Ok(data) => data,
                _ => {
                    ensure!(self.skip_usage, "VersionData at {version} is missing.");
                    StateStorageUsage::new_untracked()
                },
            })
        })
    }
}

impl DbReader for StateStore {
    fn get_buffered_state_base(&self) -> Result<SparseMerkleTree<StateValue>> {
        Ok(self.smt_ancestors.lock().get_youngest())
    }

    /// Returns the latest state snapshot strictly before `next_version` if any.
    fn get_state_snapshot_before(
        &self,
        next_version: Version,
    ) -> Result<Option<(Version, HashValue)>> {
        self.deref().get_state_snapshot_before(next_version)
    }

    /// Get the latest state value of the given key up to the given version. Only used for testing for now
    /// but should replace the `get_value_with_proof_by_version` call for VM execution if just fetch the
    /// value without proof.
    fn get_state_value_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<Option<StateValue>> {
        self.deref().get_state_value_by_version(state_key, version)
    }

    /// Gets the latest state value and the its corresponding version when its of the given key up
    /// to the given version.
    fn get_state_value_with_version_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<Option<(Version, StateValue)>> {
        self.deref()
            .get_state_value_with_version_by_version(state_key, version)
    }

    /// Returns the proof of the given state key and version.
    fn get_state_proof_by_version_ext(
        &self,
        state_key: &StateKey,
        version: Version,
        root_depth: usize,
    ) -> Result<SparseMerkleProofExt> {
        self.deref()
            .get_state_proof_by_version_ext(state_key, version, root_depth)
    }

    /// Get the state value with proof extension given the state key and version
    fn get_state_value_with_proof_by_version_ext(
        &self,
        state_key: &StateKey,
        version: Version,
        root_depth: usize,
    ) -> Result<(Option<StateValue>, SparseMerkleProofExt)> {
        self.deref()
            .get_state_value_with_proof_by_version_ext(state_key, version, root_depth)
    }
}

impl StateDb {
    fn expect_value_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<StateValue> {
        self.get_state_value_by_version(state_key, version)
            .and_then(|opt| {
                opt.ok_or_else(|| {
                    AptosDbError::NotFound(format!(
                        "State Value is missing for key {:?} by version {}",
                        state_key, version
                    ))
                })
            })
    }
}

impl StateStore {
    pub fn new(
        ledger_db: Arc<LedgerDb>,
        state_merkle_db: Arc<StateMerkleDb>,
        state_kv_db: Arc<StateKvDb>,
        state_merkle_pruner: StateMerklePrunerManager<StaleNodeIndexSchema>,
        epoch_snapshot_pruner: StateMerklePrunerManager<StaleNodeIndexCrossEpochSchema>,
        state_kv_pruner: StateKvPrunerManager,
        buffered_state_target_items: usize,
        hack_for_tests: bool,
        empty_buffered_state_for_restore: bool,
        skip_usage: bool,
    ) -> Self {
        if !hack_for_tests && !empty_buffered_state_for_restore {
            Self::sync_commit_progress(
                Arc::clone(&ledger_db),
                Arc::clone(&state_kv_db),
                /*crash_if_difference_is_too_large=*/ true,
            );
        }
        let state_db = Arc::new(StateDb {
            ledger_db,
            state_merkle_db,
            state_kv_db,
            state_merkle_pruner,
            epoch_snapshot_pruner,
            state_kv_pruner,
            skip_usage,
        });
        let (buffered_state, smt_ancestors) = if empty_buffered_state_for_restore {
            BufferedState::new(
                &state_db,
                StateDelta::new_empty(),
                buffered_state_target_items,
            )
        } else {
            Self::create_buffered_state_from_latest_snapshot(
                &state_db,
                buffered_state_target_items,
                hack_for_tests,
                /*check_max_versions_after_snapshot=*/ true,
            )
            .expect("buffered state creation failed.")
        };

        Self {
            state_db,
            buffered_state: Mutex::new(buffered_state),
            buffered_state_target_items,
            smt_ancestors: Mutex::new(smt_ancestors),
        }
    }

    // We commit the overall commit progress at the last, and use it as the source of truth of the
    // commit progress.
    pub fn sync_commit_progress(
        ledger_db: Arc<LedgerDb>,
        state_kv_db: Arc<StateKvDb>,
        crash_if_difference_is_too_large: bool,
    ) {
        let ledger_metadata_db = ledger_db.metadata_db();
        if let Ok(overall_commit_progress) = ledger_metadata_db.get_latest_version() {
            info!(
                overall_commit_progress = overall_commit_progress,
                "Start syncing databases..."
            );
            let ledger_commit_progress = ledger_metadata_db
                .get_ledger_commit_progress()
                .expect("Failed to read ledger commit progress.");
            assert_ge!(ledger_commit_progress, overall_commit_progress);

            let state_kv_commit_progress = state_kv_db
                .metadata_db()
                .get::<DbMetadataSchema>(&DbMetadataKey::StateKvCommitProgress)
                .expect("Failed to read state K/V commit progress.")
                .expect("State K/V commit progress cannot be None.")
                .expect_version();
            assert_ge!(state_kv_commit_progress, overall_commit_progress);

            // LedgerCommitProgress was not guaranteed to commit after all ledger changes finish,
            // have to attempt truncating every column family.
            info!(
                ledger_commit_progress = ledger_commit_progress,
                "Attempt ledger truncation...",
            );
            let difference = ledger_commit_progress - overall_commit_progress;
            if crash_if_difference_is_too_large {
                assert_le!(difference, MAX_COMMIT_PROGRESS_DIFFERENCE);
            }
            truncate_ledger_db(ledger_db, overall_commit_progress)
                .expect("Failed to truncate ledger db.");

            // State K/V commit progress isn't (can't be) written atomically with the data,
            // because there are shards, so we have to attempt truncation anyway.
            info!(
                state_kv_commit_progress = state_kv_commit_progress,
                "Start state KV truncation..."
            );
            let difference = state_kv_commit_progress - overall_commit_progress;
            if crash_if_difference_is_too_large {
                assert_le!(difference, MAX_COMMIT_PROGRESS_DIFFERENCE);
            }
            truncate_state_kv_db(
                &state_kv_db,
                state_kv_commit_progress,
                overall_commit_progress,
                std::cmp::max(difference as usize, 1), /* batch_size */
            )
            .expect("Failed to truncate state K/V db.");
        } else {
            info!("No overall commit progress was found!");
        }
    }

    #[cfg(feature = "db-debugger")]
    pub fn catch_up_state_merkle_db(
        ledger_db: Arc<LedgerDb>,
        state_merkle_db: Arc<StateMerkleDb>,
        state_kv_db: Arc<StateKvDb>,
    ) -> Result<Option<Version>> {
        use aptos_config::config::NO_OP_STORAGE_PRUNER_CONFIG;

        let state_merkle_pruner = StateMerklePrunerManager::new(
            Arc::clone(&state_merkle_db),
            NO_OP_STORAGE_PRUNER_CONFIG.state_merkle_pruner_config,
        );
        let epoch_snapshot_pruner = StateMerklePrunerManager::new(
            Arc::clone(&state_merkle_db),
            NO_OP_STORAGE_PRUNER_CONFIG.state_merkle_pruner_config,
        );
        let state_kv_pruner = StateKvPrunerManager::new(
            Arc::clone(&state_kv_db),
            NO_OP_STORAGE_PRUNER_CONFIG.ledger_pruner_config,
        );
        let state_db = Arc::new(StateDb {
            ledger_db,
            state_merkle_db,
            state_kv_db,
            state_merkle_pruner,
            epoch_snapshot_pruner,
            state_kv_pruner,
            skip_usage: false,
        });
        let (buffered_state, _) = Self::create_buffered_state_from_latest_snapshot(
            &state_db, 0, /*hack_for_tests=*/ false,
            /*check_max_versions_after_snapshot=*/ false,
        )?;
        Ok(buffered_state.current_state().base_version)
    }

    fn create_buffered_state_from_latest_snapshot(
        state_db: &Arc<StateDb>,
        buffered_state_target_items: usize,
        hack_for_tests: bool,
        check_max_versions_after_snapshot: bool,
    ) -> Result<(BufferedState, SmtAncestors<StateValue>)> {
        let num_transactions = state_db
            .ledger_db
            .metadata_db()
            .get_latest_version()
            .map_or(0, |v| v + 1);

        let latest_snapshot_version = state_db
            .state_merkle_db
            .get_state_snapshot_version_before(Version::MAX)
            .expect("Failed to query latest node on initialization.");

        info!(
            num_transactions = num_transactions,
            latest_snapshot_version = latest_snapshot_version,
            "Initializing BufferedState."
        );
        let latest_snapshot_root_hash = if let Some(version) = latest_snapshot_version {
            state_db
                .state_merkle_db
                .get_root_hash(version)
                .expect("Failed to query latest checkpoint root hash on initialization.")
        } else {
            *SPARSE_MERKLE_PLACEHOLDER_HASH
        };
        let usage = state_db.get_state_storage_usage(latest_snapshot_version)?;
        let (mut buffered_state, smt_ancestors) = BufferedState::new(
            state_db,
            StateDelta::new_at_checkpoint(
                latest_snapshot_root_hash,
                usage,
                latest_snapshot_version,
            ),
            buffered_state_target_items,
        );

        // In some backup-restore tests we hope to open the db without consistency check.
        if hack_for_tests {
            return Ok((buffered_state, smt_ancestors));
        }

        // Make sure the committed transactions is ahead of the latest snapshot.
        let snapshot_next_version = latest_snapshot_version.map_or(0, |v| v + 1);

        // For non-restore cases, always snapshot_next_version <= num_transactions.
        if snapshot_next_version > num_transactions {
            info!(
                snapshot_next_version = snapshot_next_version,
                num_transactions = num_transactions,
                "snapshot is after latest transaction version. It should only happen in restore mode",
            );
        }

        // Replaying the committed write sets after the latest snapshot.
        if snapshot_next_version < num_transactions {
            if check_max_versions_after_snapshot {
                ensure!(
                    num_transactions - snapshot_next_version <= MAX_WRITE_SETS_AFTER_SNAPSHOT,
                    "Too many versions after state snapshot. snapshot_next_version: {}, num_transactions: {}",
                    snapshot_next_version,
                    num_transactions,
                );
            }
            let snapshot = state_db.get_state_snapshot_before(num_transactions)?;
            let speculative_state = buffered_state
                .current_state()
                .current
                .freeze(&buffered_state.current_state().base);
            let latest_snapshot_state_view = CachedStateView::new_impl(
                StateViewId::Miscellaneous,
                snapshot,
                speculative_state,
                Arc::new(AsyncProofFetcher::new(state_db.clone())),
            );
            let write_sets = state_db
                .ledger_db
                .write_set_db()
                .get_write_sets(snapshot_next_version, num_transactions)?;
            let txn_info_iter = state_db
                .ledger_db
                .transaction_info_db()
                .get_transaction_info_iter(snapshot_next_version, write_sets.len())?;
            let last_checkpoint_index = txn_info_iter
                .into_iter()
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .enumerate()
                .filter(|(_idx, txn_info)| txn_info.is_state_checkpoint())
                .last()
                .map(|(idx, _)| idx);
            latest_snapshot_state_view.prime_cache_by_write_set(&write_sets)?;

            let (updates_until_last_checkpoint, state_after_last_checkpoint) =
                InMemoryStateCalculatorV2::calculate_for_write_sets_after_snapshot(
                    buffered_state.current_state(),
                    latest_snapshot_state_view.into_state_cache(),
                    last_checkpoint_index,
                    &write_sets,
                )?;

            // synchronously commit the snapshot at the last checkpoint here if not committed to disk yet.
            buffered_state.update(
                updates_until_last_checkpoint,
                state_after_last_checkpoint,
                true, /* sync_commit */
            )?;
        }

        info!(
            latest_snapshot_version = buffered_state.current_state().base_version,
            latest_snapshot_root_hash = buffered_state.current_state().base.root_hash(),
            latest_in_memory_version = buffered_state.current_state().current_version,
            latest_in_memory_root_hash = buffered_state.current_state().current.root_hash(),
            "StateStore initialization finished.",
        );
        Ok((buffered_state, smt_ancestors))
    }

    pub fn reset(&self) {
        let (buffered_state, smt_ancestors) = Self::create_buffered_state_from_latest_snapshot(
            &self.state_db,
            self.buffered_state_target_items,
            false,
            true,
        )
        .expect("buffered state creation failed.");
        *self.buffered_state.lock() = buffered_state;
        *self.smt_ancestors.lock() = smt_ancestors;
    }

    pub fn buffered_state(&self) -> &Mutex<BufferedState> {
        &self.buffered_state
    }

    /// Returns the key, value pairs for a particular state key prefix at at desired version. This
    /// API can be used to get all resources of an account by passing the account address as the
    /// key prefix.
    pub fn get_prefixed_state_value_iterator(
        &self,
        key_prefix: &StateKeyPrefix,
        first_key_opt: Option<&StateKey>,
        desired_version: Version,
    ) -> Result<PrefixedStateValueIterator> {
        PrefixedStateValueIterator::new(
            &self.state_kv_db,
            key_prefix.clone(),
            first_key_opt.cloned(),
            desired_version,
            self.state_kv_db.enabled_sharding(),
        )
    }

    /// Gets the proof that proves a range of accounts.
    pub fn get_value_range_proof(
        &self,
        rightmost_key: HashValue,
        version: Version,
    ) -> Result<SparseMerkleRangeProof> {
        self.state_merkle_db.get_range_proof(rightmost_key, version)
    }

    /// Put the write sets on top of current state
    pub fn put_write_sets(
        &self,
        write_sets: Vec<WriteSet>,
        first_version: Version,
        batch: &SchemaBatch,
        sharded_state_kv_batches: &ShardedStateKvSchemaBatch,
        state_kv_metadata_batch: &SchemaBatch,
        put_state_value_indices: bool,
    ) -> Result<()> {
        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["put_writesets"])
            .start_timer();

        // convert value state sets to hash map reference
        let value_state_sets_raw: Vec<ShardedStateUpdates> = write_sets
            .iter()
            .map(|ws| {
                let mut sharded_state_updates = create_empty_sharded_state_updates();
                ws.iter().for_each(|(key, value)| {
                    sharded_state_updates[key.get_shard_id() as usize]
                        .insert(key.clone(), value.as_state_value());
                });
                sharded_state_updates
            })
            .collect::<Vec<_>>();

        let value_state_sets = value_state_sets_raw.iter().collect::<Vec<_>>();

        self.put_stats_and_indices(
            value_state_sets.as_slice(),
            first_version,
            StateStorageUsage::new_untracked(),
            None,
            batch,
            sharded_state_kv_batches,
            /*skip_usage=*/ false,
            None,
        )?;

        self.put_state_values(
            value_state_sets.to_vec(),
            first_version,
            sharded_state_kv_batches,
            state_kv_metadata_batch,
            put_state_value_indices,
        )?;

        Ok(())
    }

    /// Put the `value_state_sets` into its own CF.
    pub fn put_value_sets(
        &self,
        value_state_sets: Vec<&ShardedStateUpdates>,
        first_version: Version,
        expected_usage: StateStorageUsage,
        sharded_state_cache: Option<&ShardedStateCache>,
        ledger_batch: &SchemaBatch,
        sharded_state_kv_batches: &ShardedStateKvSchemaBatch,
        state_kv_metadata_batch: &SchemaBatch,
        put_state_value_indices: bool,
        skip_usage: bool,
        last_checkpoint_index: Option<usize>,
    ) -> Result<()> {
        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["put_value_sets"])
            .start_timer();

        self.put_stats_and_indices(
            &value_state_sets,
            first_version,
            expected_usage,
            sharded_state_cache,
            ledger_batch,
            sharded_state_kv_batches,
            skip_usage,
            last_checkpoint_index,
        )?;

        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["add_state_kv_batch"])
            .start_timer();

        self.put_state_values(
            value_state_sets,
            first_version,
            sharded_state_kv_batches,
            state_kv_metadata_batch,
            put_state_value_indices,
        )
    }

    pub fn put_state_values(
        &self,
        value_state_sets: Vec<&ShardedStateUpdates>,
        first_version: Version,
        sharded_state_kv_batches: &ShardedStateKvSchemaBatch,
        state_kv_metadata_batch: &SchemaBatch,
        put_state_value_indices: bool,
    ) -> Result<()> {
        sharded_state_kv_batches
            .par_iter()
            .enumerate()
            .try_for_each(|(shard_id, batch)| {
                value_state_sets
                    .par_iter()
                    .enumerate()
                    .flat_map_iter(|(i, shards)| {
                        let version = first_version + i as Version;
                        let kvs = &shards[shard_id];
                        kvs.iter().map(move |(k, v)| {
                            batch.put::<StateValueSchema>(&(k.clone(), version), v)
                        })
                    })
                    .collect::<Result<_>>()
            })?;

        // Eventually this index will move to indexer side. For now we temporarily write this into
        // metadata db to unblock the sharded DB migration.
        // TODO(grao): Remove when we are ready.
        if put_state_value_indices {
            value_state_sets
                .par_iter()
                .enumerate()
                .try_for_each(|(i, updates)| {
                    let version = first_version + i as Version;
                    updates.iter().flatten().try_for_each(|(k, _)| {
                        state_kv_metadata_batch
                            .put::<StateValueIndexSchema>(&(k.clone(), version), &())
                    })
                })?;
        }

        Ok(())
    }

    pub fn get_usage(&self, version: Option<Version>) -> Result<StateStorageUsage> {
        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["get_usage"])
            .start_timer();
        self.state_db.get_state_storage_usage(version)
    }

    /// Put storage usage stats and State key and value indices into the batch.
    /// The state KV indices will be generated as follows:
    /// 1. A deletion at current version is always coupled with stale index for the tombstone with
    /// `stale_since_version` equal to the version, to ensure tombstone is cleared from db after
    /// pruner processes the current version.
    /// 2. An update at current version will first try to find the corresponding old value, if it
    /// exists, a stale index of that old value will be added. Otherwise, it's a no-op. Because
    /// non-existence means either the key never shows up or it got deleted. Neither case needs
    /// extra stale index as 1 cover the latter case.
    pub fn put_stats_and_indices(
        &self,
        value_state_sets: &[&ShardedStateUpdates],
        first_version: Version,
        expected_usage: StateStorageUsage,
        // If not None, it must contains all keys in the value_state_sets.
        // TODO(grao): Restructure this function.
        sharded_state_cache: Option<&ShardedStateCache>,
        batch: &SchemaBatch,
        sharded_state_kv_batches: &ShardedStateKvSchemaBatch,
        skip_usage: bool,
        last_checkpoint_index: Option<usize>,
    ) -> Result<()> {
        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["put_stats_and_indices"])
            .start_timer();

        let num_versions = value_state_sets.len();

        let base_version = first_version.checked_sub(1);
        let mut usage = self.get_usage(base_version)?;
        let base_version_usage = usage;

        let mut state_cache_with_version = &ShardedStateCache::default();
        if let Some(base_version) = base_version {
            let _timer = OTHER_TIMERS_SECONDS
                .with_label_values(&["put_stats_and_indices__total_get"])
                .start_timer();
            if let Some(sharded_state_cache) = sharded_state_cache {
                // For some entries the base value version is None, here is to fiil those in.
                // See `ShardedStateCache`.
                self.prepare_version_in_cache(base_version, sharded_state_cache)?;
                state_cache_with_version = sharded_state_cache;
            } else {
                let key_set = value_state_sets
                    .iter()
                    .flat_map(|sharded_states| sharded_states.iter().flatten())
                    .map(|(key, _)| key)
                    .collect::<HashSet<_>>();
                THREAD_MANAGER.get_high_pri_io_pool().scope(|s| {
                    for key in key_set {
                        let cache = state_cache_with_version.shard(key.get_shard_id());
                        s.spawn(move |_| {
                            let _timer = OTHER_TIMERS_SECONDS
                                .with_label_values(&["put_stats_and_indices__get_state_value"])
                                .start_timer();
                            let version_and_value = self
                                .state_db
                                .get_state_value_with_version_by_version(key, base_version)
                                .expect("Must succeed.");
                            if let Some((version, value)) = version_and_value {
                                cache.insert(key.clone(), (Some(version), Some(value)));
                            } else {
                                cache.insert(key.clone(), (Some(base_version), None));
                            }
                        });
                    }
                });
            }
        }

        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["put_stats_and_indices__calculate_total_size"])
            .start_timer();
        // calculate total state size in bytes
        let usage_deltas: Vec<Vec<_>> = state_cache_with_version
            .par_iter()
            .enumerate()
            .map(|(shard_id, cache)| {
                let _timer = OTHER_TIMERS_SECONDS
                    .with_label_values(&[&format!(
                        "put_stats_and_indices__calculate_total_size__shard_{shard_id}"
                    )])
                    .start_timer();
                let mut usage_delta = Vec::with_capacity(num_versions);
                for (idx, kvs) in value_state_sets.iter().enumerate() {
                    let version = first_version + idx as Version;
                    let mut items_delta = 0;
                    let mut bytes_delta = 0;

                    for (key, value) in kvs[shard_id].iter() {
                        if let Some(value) = value {
                            items_delta += 1;
                            bytes_delta += (key.size() + value.size()) as i64;
                        } else {
                            // Update the stale index of the tombstone at current version to
                            // current version.
                            sharded_state_kv_batches[shard_id]
                                .put::<StaleStateValueIndexSchema>(
                                    &StaleStateValueIndex {
                                        stale_since_version: version,
                                        version,
                                        state_key: key.clone(),
                                    },
                                    &(),
                                )
                                .unwrap();
                        }

                        let old_version_and_value_opt = if let Some((old_version, old_value_opt)) =
                            cache.insert(key.clone(), (Some(version), value.clone()))
                        {
                            old_value_opt.map(|value| (old_version, value))
                        } else {
                            None
                        };

                        if let Some((old_version, old_value)) = old_version_and_value_opt {
                            let old_version = old_version
                                .context("Must have old version in cache.")
                                .unwrap();
                            items_delta -= 1;
                            bytes_delta -= (key.size() + old_value.size()) as i64;
                            // stale index of the old value at its version.
                            sharded_state_kv_batches[shard_id]
                                .put::<StaleStateValueIndexSchema>(
                                    &StaleStateValueIndex {
                                        stale_since_version: version,
                                        version: old_version,
                                        state_key: key.clone(),
                                    },
                                    &(),
                                )
                                .unwrap();
                        }
                    }
                    usage_delta.push((items_delta, bytes_delta));
                }

                usage_delta
            })
            .collect();

        for i in 0..num_versions {
            let mut items_delta = 0;
            let mut bytes_delta = 0;
            for usage_delta in usage_deltas.iter() {
                items_delta += usage_delta[i].0;
                bytes_delta += usage_delta[i].1;
            }
            usage = StateStorageUsage::new(
                (usage.items() as i64 + items_delta) as usize,
                (usage.bytes() as i64 + bytes_delta) as usize,
            );
            let should_write_index_for_version =
                (i == num_versions - 1) || Some(i) == last_checkpoint_index;
            if !skip_usage || should_write_index_for_version {
                let version = first_version + i as u64;
                if should_write_index_for_version {
                    info!("Write usage at version {version}, {usage:?}, skip_usage: {skip_usage}.");
                }
                batch
                    .put::<VersionDataSchema>(&version, &usage.into())
                    .unwrap();
            }
        }

        if !expected_usage.is_untracked() {
            ensure!(
                expected_usage == usage,
                "Calculated state db usage at version {} not expected. expected: {:?}, calculated: {:?}, base version: {:?}, base version usage: {:?}",
                first_version + value_state_sets.len() as u64 - 1,
                expected_usage,
                usage,
                base_version,
                base_version_usage,
            );
        }

        STATE_ITEMS.set(usage.items() as i64);
        TOTAL_STATE_BYTES.set(usage.bytes() as i64);

        Ok(())
    }

    pub(crate) fn shard_state_value_batch(
        &self,
        metadata_batch: &SchemaBatch,
        sharded_batch: &ShardedStateKvSchemaBatch,
        values: &StateValueBatch,
    ) -> Result<()> {
        values.iter().for_each(|((key, version), value)| {
            let shard_id = key.get_shard_id() as usize;
            assert!(
                shard_id < NUM_STATE_SHARDS,
                "Invalid shard id: {}",
                shard_id
            );
            sharded_batch[shard_id]
                .put::<StateValueSchema>(&(key.clone(), *version), value)
                .expect("Inserting into sharded schema batch should never fail");

            if self.state_kv_db.enabled_sharding() {
                metadata_batch
                    .put::<StateValueIndexSchema>(&(key.clone(), *version), &())
                    .expect("Inserting into state value index schema batch should never fail");
            }
        });
        Ok(())
    }

    /// Merklize the results generated by `value_state_sets` to `batch` and return the result root
    /// hashes for each write set.
    #[cfg(test)]
    pub fn merklize_value_set(
        &self,
        value_set: Vec<(HashValue, Option<&(HashValue, StateKey)>)>,
        version: Version,
        base_version: Option<Version>,
    ) -> Result<HashValue> {
        let (top_levels_batch, sharded_batch, root_hash) =
            self.state_merkle_db.merklize_value_set(
                value_set,
                version,
                base_version,
                /*previous_epoch_ending_version=*/ None,
            )?;
        self.state_merkle_db
            .commit(version, top_levels_batch, sharded_batch)?;
        Ok(root_hash)
    }

    pub fn get_root_hash(&self, version: Version) -> Result<HashValue> {
        self.state_merkle_db.get_root_hash(version)
    }

    pub fn get_value_count(&self, version: Version) -> Result<usize> {
        self.state_merkle_db.get_leaf_count(version)
    }

    pub fn get_state_key_and_value_iter(
        self: &Arc<Self>,
        version: Version,
        start_hashed_key: HashValue,
    ) -> Result<impl Iterator<Item = Result<(StateKey, StateValue)>> + Send + Sync> {
        let store = Arc::clone(self);
        Ok(JellyfishMerkleIterator::new(
            Arc::clone(&self.state_merkle_db),
            version,
            start_hashed_key,
        )?
        .map(|it| it.map_err(Into::into))
        .map(move |res| match res {
            Ok((_hashed_key, (key, version))) => {
                Ok((key.clone(), store.expect_value_by_version(&key, version)?))
            },
            Err(err) => Err(err),
        }))
    }

    pub fn get_value_chunk_with_proof(
        self: &Arc<Self>,
        version: Version,
        first_index: usize,
        chunk_size: usize,
    ) -> Result<StateValueChunkWithProof> {
        let result_iter = JellyfishMerkleIterator::new_by_index(
            Arc::clone(&self.state_merkle_db),
            version,
            first_index,
        )?
        .take(chunk_size)
        .map(|it| it.map_err(Into::into));
        let state_key_values: Vec<(StateKey, StateValue)> = result_iter
            .into_iter()
            .map(|res| {
                res.and_then(|(_, (key, version))| {
                    Ok((key.clone(), self.expect_value_by_version(&key, version)?))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !state_key_values.is_empty(),
            "State chunk starting at {}",
            first_index,
        );
        let last_index = (state_key_values.len() - 1 + first_index) as u64;
        let first_key = state_key_values.first().expect("checked to exist").0.hash();
        let last_key = state_key_values.last().expect("checked to exist").0.hash();
        let proof = self.get_value_range_proof(last_key, version)?;
        let root_hash = self.get_root_hash(version)?;

        Ok(StateValueChunkWithProof {
            first_index: first_index as u64,
            last_index,
            first_key,
            last_key,
            raw_values: state_key_values,
            proof,
            root_hash,
        })
    }

    // state sync doesn't query for the progress, but keeps its record by itself.
    // TODO: change to async comment once it does like https://github.com/aptos-labs/aptos-core/blob/159b00f3d53e4327523052c1b99dd9889bf13b03/storage/backup/backup-cli/src/backup_types/state_snapshot/restore.rs#L147 or overlap at least two chunks.
    pub fn get_snapshot_receiver(
        self: &Arc<Self>,
        version: Version,
        expected_root_hash: HashValue,
    ) -> Result<Box<dyn StateSnapshotReceiver<StateKey, StateValue>>> {
        Ok(Box::new(StateSnapshotRestore::new(
            &self.state_merkle_db,
            self,
            version,
            expected_root_hash,
            false, /* async_commit */
            StateSnapshotRestoreMode::Default,
        )?))
    }

    #[cfg(test)]
    pub fn get_all_jmt_nodes_referenced(
        &self,
        version: Version,
    ) -> Result<Vec<aptos_jellyfish_merkle::node_type::NodeKey>> {
        aptos_jellyfish_merkle::JellyfishMerkleTree::new(self.state_merkle_db.as_ref())
            .get_all_nodes_referenced(version)
            .map_err(Into::into)
    }

    #[cfg(test)]
    pub fn get_all_jmt_nodes(&self) -> Result<Vec<aptos_jellyfish_merkle::node_type::NodeKey>> {
        let mut iter = self
            .state_db
            .state_merkle_db
            .metadata_db()
            .iter::<crate::schema::jellyfish_merkle_node::JellyfishMerkleNodeSchema>(
            Default::default(),
        )?;
        iter.seek_to_first();

        let all_rows = iter.collect::<Result<Vec<_>>>()?;

        let mut keys: Vec<aptos_jellyfish_merkle::node_type::NodeKey> =
            all_rows.into_iter().map(|(k, _v)| k).collect();
        if self.state_merkle_db.sharding_enabled() {
            for i in 0..NUM_STATE_SHARDS as u8 {
                let mut iter = self
                    .state_merkle_db
                    .db_shard(i)
                    .iter::<crate::schema::jellyfish_merkle_node::JellyfishMerkleNodeSchema>(
                    Default::default(),
                )?;
                iter.seek_to_first();

                let all_rows = iter.collect::<Result<Vec<_>>>()?;
                keys.extend(all_rows.into_iter().map(|(k, _v)| k).collect::<Vec<_>>());
            }
        }
        Ok(keys)
    }

    fn prepare_version_in_cache(
        &self,
        base_version: Version,
        sharded_state_cache: &ShardedStateCache,
    ) -> Result<()> {
        THREAD_MANAGER.get_high_pri_io_pool().scope(|s| {
            sharded_state_cache.par_iter().for_each(|shard| {
                shard.iter_mut().for_each(|mut entry| {
                    match entry.value() {
                        (None, Some(_)) => s.spawn(move |_| {
                            let _timer = OTHER_TIMERS_SECONDS
                                .with_label_values(&["put_stats_and_indices__get_state_value"])
                                .start_timer();
                            let version_and_value = self
                                .state_db
                                .get_state_value_with_version_by_version(entry.key(), base_version)
                                .expect("Must succeed.");
                            if let Some((version, _)) = version_and_value {
                                entry.0 = Some(version);
                            } else {
                                unreachable!();
                            }
                        }),
                        _ => {
                            // I just want a counter.
                            let _timer = OTHER_TIMERS_SECONDS
                                .with_label_values(&["put_stats_and_indices__skip"])
                                .start_timer();
                        },
                    };
                })
            });
        });

        Ok(())
    }
}

impl StateValueWriter<StateKey, StateValue> for StateStore {
    fn write_kv_batch(
        &self,
        version: Version,
        node_batch: &StateValueBatch,
        progress: StateSnapshotProgress,
    ) -> Result<()> {
        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["state_value_writer_write_chunk"])
            .start_timer();
        let batch = SchemaBatch::new();
        let sharded_schema_batch = new_sharded_kv_schema_batch();

        batch.put::<DbMetadataSchema>(
            &DbMetadataKey::StateSnapshotRestoreProgress(version),
            &DbMetadataValue::StateSnapshotProgress(progress),
        )?;

        self.shard_state_value_batch(&batch, &sharded_schema_batch, node_batch)?;

        self.state_kv_db
            .commit(version, batch, sharded_schema_batch)
    }

    fn write_usage(&self, version: Version, usage: StateStorageUsage) -> Result<()> {
        self.ledger_db.metadata_db().put_usage(version, usage)
    }

    fn get_progress(&self, version: Version) -> Result<Option<StateSnapshotProgress>> {
        Ok(self
            .state_kv_db
            .metadata_db()
            .get::<DbMetadataSchema>(&DbMetadataKey::StateSnapshotRestoreProgress(version))?
            .map(|v| v.expect_state_snapshot_progress()))
    }
}

// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    async_proof_fetcher::AsyncProofFetcher, metrics::TIMER, state_view::DbStateView, DbReader,
};
use aptos_crypto::{hash::CryptoHash, HashValue};
use aptos_experimental_runtimes::thread_manager::THREAD_MANAGER;
use aptos_scratchpad::{FrozenSparseMerkleTree, SparseMerkleTree, StateStoreStatus};
use aptos_types::{
    state_store::{
        errors::StateviewError, state_key::StateKey, state_storage_usage::StateStorageUsage,
        state_value::StateValue, StateViewId, TStateView,
    },
    transaction::Version,
    write_set::WriteSet,
};
use core::fmt;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use std::{
    collections::{HashMap, HashSet},
    fmt::{Debug, Formatter},
    sync::Arc,
};
use aptos_logger::info;

static IO_POOL: Lazy<rayon::ThreadPool> = Lazy::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(32)
        .thread_name(|index| format!("kv_reader_{}", index))
        .build()
        .unwrap()
});

type Result<T, E = StateviewError> = std::result::Result<T, E>;
type StateCacheShard = DashMap<StateKey, (Option<Version>, Option<StateValue>)>;

// Sharded by StateKey.get_shard_id(). The version in the value indicates there is an entry on that
// version for the given StateKey, and the version is the maximum one which <= the base version. It
// will be None if the value is None, or we found the value on the speculative tree (in that case
// we don't know the maximum version).
#[derive(Debug, Default)]
pub struct ShardedStateCache {
    shards: [StateCacheShard; 16],
}

impl ShardedStateCache {
    pub fn combine(&mut self, rhs: Self) {
        use rayon::prelude::*;
        THREAD_MANAGER.get_exe_cpu_pool().install(|| {
            self.shards
                .par_iter_mut()
                .zip_eq(rhs.shards.into_par_iter())
                .for_each(|(l, r)| {
                    for (k, (ver, val)) in r.into_iter() {
                        l.entry(k).or_insert((ver, val));
                    }
                })
        });
    }

    pub fn shard(&self, shard_id: u8) -> &StateCacheShard {
        &self.shards[shard_id as usize]
    }

    pub fn flatten(self) -> DashMap<StateKey, Option<StateValue>> {
        // TODO(grao): Rethink the strategy for state sync, and optimize this.
        self.shards
            .into_iter()
            .flatten()
            .map(|(key, (_ver_opt, val_opt))| (key, val_opt))
            .collect()
    }

    pub fn par_iter(&self) -> impl IndexedParallelIterator<Item = &StateCacheShard> {
        self.shards.par_iter()
    }

    pub fn iter(&self) -> impl Iterator<Item = &StateCacheShard> {
        self.shards.iter()
    }
}

/// `CachedStateView` is like a snapshot of the global state comprised of state view at two
/// levels, persistent storage and memory.
pub struct CachedStateView {
    /// For logging and debugging purpose, identifies what this view is for.
    id: StateViewId,

    /// A readable snapshot in the persistent storage.
    snapshot: Option<(Version, HashValue)>,

    /// The in-memory state on top of the snapshot.
    speculative_state: FrozenSparseMerkleTree<StateValue>,

    /// The cache of verified account states from `reader` and `speculative_state_view`,
    /// represented by a hashmap with an account address as key and a pair of an ordered
    /// account state map and an an optional account state proof as value. When the VM queries an
    /// `access_path`, this cache will first check whether `reader_cache` is hit. If hit, it
    /// will return the corresponding value of that `access_path`; otherwise, the account state
    /// will be loaded into the cache from scratchpad or persistent storage in order as a
    /// deserialized ordered map and then be returned. If the VM queries this account again,
    /// the cached data can be read directly without bothering storage layer. The proofs in
    /// cache are needed by ScratchPad after VM execution to construct an in-memory sparse Merkle
    /// tree.
    /// ```text
    ///                      +----------------------------+
    ///                      | In-memory SparseMerkleTree <------+
    ///                      +-------------^--------------+      |
    ///                                    |                     |
    ///                                write sets                |
    ///                                    |          cached account state map
    ///                            +-------+-------+           proof
    ///                            |      V M      |             |
    ///                            +-------^-------+             |
    ///                                    |                     |
    ///                      value of `account_address/path`     |
    ///                                    |                     |
    ///        +---------------------------+---------------------+-------+
    ///        | +-------------------------+---------------------+-----+ |
    ///        | |           state_cache,     state_key_to_proof_cache   | |
    ///        | +---------------^---------------------------^---------+ |
    ///        |                 |                           |           |
    ///        |     state store values only        state blob proof     |
    ///        |                 |                           |           |
    ///        |                 |                           |           |
    ///        | +---------------+--------------+ +----------+---------+ |
    ///        | |      speculative_state       | |       reader       | |
    ///        | +------------------------------+ +--------------------+ |
    ///        +---------------------------------------------------------+
    /// ```
    /// Cache of state key to state value, which is used in case of fine grained storage object.
    /// Eventually this should replace the `account_to_state_cache` as we deprecate account state blob
    /// completely and migrate to fine grained storage. A value of None in this cache reflects that
    /// the corresponding key has been deleted. This is a temporary hack until we support deletion
    /// in JMT node.
    sharded_state_cache: ShardedStateCache,

    /// db
    reader: Arc<dyn DbReader>,
}

impl Debug for CachedStateView {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.id)
    }
}

impl CachedStateView {
    /// Constructs a [`CachedStateView`] with persistent state view in the DB and the in-memory
    /// speculative state represented by `speculative_state`. The persistent state view is the
    /// latest one preceding `next_version`
    pub fn new(
        id: StateViewId,
        reader: Arc<dyn DbReader>,
        next_version: Version,
        speculative_state: SparseMerkleTree<StateValue>,
        _proof_fetcher: Arc<AsyncProofFetcher>,
    ) -> Result<Self> {
        // n.b. Freeze the state before getting the state snapshot, otherwise it's possible that
        // after we got the snapshot, in-mem trees newer than it gets dropped before being frozen,
        // due to a commit happening from another thread.
        let base_smt = reader.get_buffered_state_base()?;
        let speculative_state = speculative_state.freeze(&base_smt);
        let snapshot = reader
            .get_state_snapshot_before(next_version)
            .map_err(Into::<StateviewError>::into)?;
        info!(
            snapshot = snapshot,
            "alden alden alden."
        );

        Ok(Self::new_impl(id, snapshot, speculative_state, reader))
    }

    pub fn new_impl(
        id: StateViewId,
        snapshot: Option<(Version, HashValue)>,
        speculative_state: FrozenSparseMerkleTree<StateValue>,
        reader: Arc<dyn DbReader>,
    ) -> Self {
        Self {
            id,
            snapshot,
            speculative_state,
            sharded_state_cache: ShardedStateCache::default(),
            reader,
        }
    }

    pub fn prime_cache_by_write_set<'a, T: IntoIterator<Item = &'a WriteSet> + Send>(
        &self,
        write_sets: T,
    ) -> Result<()> {
        IO_POOL.scope(|s| {
            write_sets
                .into_iter()
                .flat_map(|write_set| write_set.iter())
                .map(|(key, _)| key)
                .collect::<HashSet<_>>()
                .into_iter()
                .for_each(|key| {
                    s.spawn(move |_| {
                        self.get_state_value_bytes(key).expect("Must succeed.");
                    })
                });
        });
        Ok(())
    }

    pub fn into_state_cache(self) -> StateCache {
        // FIXME(aldenhu): make faster
        let proofs = self
            .sharded_state_cache
            .par_iter()
            .map(|shard| {
                shard.iter().map(|dashmap_ref| {
                    let (key, (_val_ver_opt, val_opt)) = dashmap_ref.pair();
                    (key.hash(), val_opt.as_ref().map(CryptoHash::hash))
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flatten()
            .collect();

        StateCache {
            frozen_base: self.speculative_state,
            sharded_state_cache: self.sharded_state_cache,
            proofs,
        }
    }

    fn get_version_and_state_value_internal(
        &self,
        state_key: &StateKey,
    ) -> Result<(Option<Version>, Option<StateValue>)> {
        // Do most of the work outside the write lock.
        let key_hash = state_key.hash();
        match self.speculative_state.get(key_hash) {
            StateStoreStatus::ExistsInScratchPad(value) => Ok((None, Some(value))),
            StateStoreStatus::DoesNotExist => Ok((None, None)),
            // Tree is known, but we only know the hash of the value, need to request the actual
            // StateValue.
            StateStoreStatus::UnknownValue => {
                self.fetch_value_and_maybe_proof_in_snapshot(state_key, None)
            },
            StateStoreStatus::UnknownSubtreeRoot { .. } => unreachable!(),
        }
    }

    fn fetch_value_and_maybe_proof_in_snapshot(
        &self,
        state_key: &StateKey,
        _fetch_proof: Option<(HashValue, usize)>,
    ) -> Result<(Option<Version>, Option<StateValue>)> {
        let version_and_value_opt = match self.snapshot {
            None => None,
            Some((version, _root_hash)) => {
                let _timer = TIMER
                    .with_label_values(&["async_proof_fetcher_fetch"])
                    .start_timer();
                self.reader
                    .get_state_value_with_version_by_version(state_key, version)?
            },
        };
        Ok(match version_and_value_opt {
            None => (None, None),
            Some((version, value)) => (Some(version), Some(value)),
        })
    }
}

pub struct StateCache {
    pub frozen_base: FrozenSparseMerkleTree<StateValue>,
    pub sharded_state_cache: ShardedStateCache,
    pub proofs: HashMap<HashValue, Option<HashValue>>,
}

impl TStateView for CachedStateView {
    type Key = StateKey;

    fn id(&self) -> StateViewId {
        self.id
    }

    fn get_state_value(&self, state_key: &StateKey) -> Result<Option<StateValue>> {
        let _timer = TIMER.with_label_values(&["get_state_value"]).start_timer();
        // First check if the cache has the state value.
        if let Some(version_and_value_opt) = self
            .sharded_state_cache
            .shard(state_key.get_shard_id())
            .get(state_key)
        {
            // This can return None, which means the value has been deleted from the DB.
            let value_opt = &version_and_value_opt.1;
            return Ok(value_opt.clone());
        }
        let version_and_state_value_option =
            self.get_version_and_state_value_internal(state_key)?;
        // Update the cache if still empty
        let new_version_and_value = self
            .sharded_state_cache
            .shard(state_key.get_shard_id())
            .entry(state_key.clone())
            .or_insert(version_and_state_value_option);
        let value_opt = &new_version_and_value.1;
        Ok(value_opt.clone())
    }

    fn get_usage(&self) -> Result<StateStorageUsage> {
        Ok(self.speculative_state.usage())
    }
}

pub struct CachedDbStateView {
    db_state_view: DbStateView,
    state_cache: RwLock<HashMap<StateKey, Option<StateValue>>>,
}

impl From<DbStateView> for CachedDbStateView {
    fn from(db_state_view: DbStateView) -> Self {
        Self {
            db_state_view,
            state_cache: RwLock::new(HashMap::new()),
        }
    }
}

impl TStateView for CachedDbStateView {
    type Key = StateKey;

    fn id(&self) -> StateViewId {
        self.db_state_view.id()
    }

    fn get_state_value(&self, state_key: &StateKey) -> Result<Option<StateValue>> {
        // First check if the cache has the state value.
        if let Some(val_opt) = self.state_cache.read().get(state_key) {
            // This can return None, which means the value has been deleted from the DB.
            return Ok(val_opt.clone());
        }
        let state_value_option = self.db_state_view.get_state_value(state_key)?;
        // Update the cache if still empty
        let mut cache = self.state_cache.write();
        let new_value = cache
            .entry(state_key.clone())
            .or_insert_with(|| state_value_option);
        Ok(new_value.clone())
    }

    fn get_usage(&self) -> Result<StateStorageUsage> {
        self.db_state_view.get_usage()
    }
}

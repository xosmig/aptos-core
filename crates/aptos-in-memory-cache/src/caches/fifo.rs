// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{Cache, IncrementableCacheKey, OrderedCache};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::{fmt::Debug, hash::Hash, sync::Arc};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Debug, Clone, Copy)]
struct CacheMetadata<K> {
    eviction_trigger_size_in_bytes: u64,
    target_size_in_bytes: u64,
    total_size_in_bytes: u64,
    last_key: Option<K>,
    first_key: Option<K>,
}

/// A simple in-memory cache with a deterministic FIFO eviction policy.
pub struct FIFOCache<K, V>
where
    K: Hash + Eq + PartialEq + IncrementableCacheKey<V> + Send + Sync + Clone + 'static,
    V: Send + Sync + Clone + 'static,
{
    /// Cache maps the cache key to the deserialized Transaction.
    items: Arc<DashMap<K, V>>,
    insert_notify: Arc<Notify>,
    cache_metadata: Arc<RwLock<CacheMetadata<K>>>,
    _cancellation_token_drop_guard: tokio_util::sync::DropGuard,
}

impl<K, V> FIFOCache<K, V>
where
    K: Hash + Eq + PartialEq + IncrementableCacheKey<V> + Send + Sync + Clone + Debug + 'static,
    V: Send + Sync + Clone + 'static,
{
    pub fn new(target_size_in_bytes: u64, eviction_trigger_size_in_bytes: u64) -> Self {
        let cancellation_token = CancellationToken::new();
        let items = Arc::new(DashMap::new());
        let insert_notify = Arc::new(Notify::new());
        let cache_metadata = Arc::new(RwLock::new(CacheMetadata {
            eviction_trigger_size_in_bytes,
            target_size_in_bytes,
            total_size_in_bytes: 0,
            last_key: None,
            first_key: None,
        }));

        Self::spawn_cleanup_task(
            items.clone(),
            insert_notify.clone(),
            cache_metadata.clone(),
            cancellation_token.clone(),
        );

        Self {
            items,
            insert_notify,
            cache_metadata,
            _cancellation_token_drop_guard: cancellation_token.drop_guard(),
        }
    }

    fn evict(items: Arc<DashMap<K, V>>, cache_metadata: Arc<RwLock<CacheMetadata<K>>>) {
        // Skip if eviction is not needed.
        let should_evict = {
            let current_cache_metadata = cache_metadata.read();
            current_cache_metadata
                .total_size_in_bytes
                .saturating_sub(current_cache_metadata.eviction_trigger_size_in_bytes)
                > 0
        };
        if !should_evict {
            return;
        }

        // Evict items from the cache.
        let mut current_cache_metadata = cache_metadata.write();
        let mut actual_bytes_removed = 0;
        let mut bytes_to_remove = current_cache_metadata
            .total_size_in_bytes
            .saturating_sub(current_cache_metadata.target_size_in_bytes);
        while bytes_to_remove > 0 {
            if let Some(key_to_remove) = current_cache_metadata.first_key.clone() {
                let (_k, v) = items
                    .remove(&key_to_remove)
                    .expect("Failed to remove the key");
                let size_of_v = std::mem::size_of_val(&v) as u64;
                bytes_to_remove = bytes_to_remove.saturating_sub(size_of_v);
                actual_bytes_removed += size_of_v;
                current_cache_metadata.first_key = Some(key_to_remove.next(&v));
            } else {
                break;
            }
        }

        current_cache_metadata.total_size_in_bytes -= actual_bytes_removed;
    }

    /// Perform cache eviction on a separate task.
    fn spawn_cleanup_task(
        items: Arc<DashMap<K, V>>,
        insert_notify: Arc<Notify>,
        cache_metadata: Arc<RwLock<CacheMetadata<K>>>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = insert_notify.notified() => {
                        Self::evict(items.clone(), cache_metadata.clone());
                    },
                    _ = cancellation_token.cancelled() => {
                        info!("In-memory cache cleanup task is cancelled.");
                        return;
                    }
                }
            }
        });
    }
}

impl<K, V> Cache<K, V> for FIFOCache<K, V>
where
    K: Hash + Eq + PartialEq + IncrementableCacheKey<V> + Send + Sync + Clone,
    V: Send + Sync + Clone,
{
    fn get(&self, key: &K) -> Option<V> {
        self.items.get(key).map(|v| v.value().clone())
    }

    fn insert(&self, key: K, value: V) {
        // If cache is empty, set the first to the new key.
        if self.items.is_empty() {
            let mut cache_metadata = self.cache_metadata.write();
            cache_metadata.first_key = Some(key.clone());
        }

        // TODO: Implement pre-caching for out of order writes
        // Check if the inserted key is in order
        let last_kv = {
            self.cache_metadata
                .read()
                .last_key
                .clone()
                .and_then(|k| self.get(&k).and_then(|v| Some((k, v))))
        };
        if let Some((k, v)) = last_kv {
            if k.next(&v) != key {
                // Panic if the key is not in order
                panic!("Key is not in order");
            }
        }

        let mut cache_metadata = self.cache_metadata.write();
        cache_metadata.last_key = Some(key.clone());
        cache_metadata.total_size_in_bytes += std::mem::size_of_val(&value) as u64;
        self.items.insert(key, value);
        self.insert_notify.notify_waiters();
    }

    fn total_size(&self) -> u64 {
        let cache_metadata = self.cache_metadata.read();
        cache_metadata.total_size_in_bytes
    }
}

impl<K, V> OrderedCache<K, V> for FIFOCache<K, V>
where
    K: Hash + Eq + PartialEq + IncrementableCacheKey<V> + Send + Sync + Clone,
    V: Send + Sync + Clone,
{
    fn first_key(&self) -> Option<K> {
        let cache_metadata = self.cache_metadata.read();
        cache_metadata.first_key.clone()
    }

    fn last_key(&self) -> Option<K> {
        let cache_metadata = self.cache_metadata.read();
        cache_metadata.last_key.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{caches::fifo::FIFOCache, Cache};
    use std::{sync::Arc, time::Duration};

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    struct TestKey(u64);

    impl IncrementableCacheKey<u64> for TestKey {
        fn next(&self, _: &u64) -> Self {
            TestKey(self.0 + 1)
        }
    }

    #[tokio::test]
    async fn test_insert_four_values() {
        let cache = FIFOCache::new(100, 200);
        let cache = Arc::new(cache);
        tokio::time::sleep(Duration::from_nanos(1)).await;

        let key1 = TestKey(1);
        let key2 = TestKey(2);
        let key3 = TestKey(3);
        let key4 = TestKey(4);

        let value1 = 1;
        let value2 = 2;
        let value3 = 3;
        let value4 = 4;

        cache.insert(key1.clone(), value1);
        cache.insert(key2.clone(), value2);
        cache.insert(key3.clone(), value3);
        cache.insert(key4.clone(), value4);
        tokio::time::sleep(Duration::from_nanos(1)).await;

        assert_eq!(cache.get(&key1), Some(value1));
        assert_eq!(cache.get(&key2), Some(value2));
        assert_eq!(cache.get(&key3), Some(value3));
        assert_eq!(cache.get(&key4), Some(value4));
    }

    #[tokio::test]
    async fn test_add_ten_values_with_eviction() {
        let cache = FIFOCache::new(40, 64);
        let cache = Arc::new(cache);

        // Insert 8 values, size is 8*8=64 bytes
        for i in 0..8 {
            cache.insert(TestKey(i), i);
        }

        for i in 0..8 {
            assert_eq!(cache.get(&TestKey(i)), Some(i));
        }

        // Insert 9th value, size is 8*9=72>64 bytes, eviction threshold reached
        // Evicts until target size size is reached
        // Sleep for 1 second to ensure eviction task finishes
        tokio::time::sleep(Duration::from_nanos(1)).await;
        cache.insert(TestKey(8), 8);
        tokio::time::sleep(Duration::from_nanos(1)).await;
        // New size is 8*5=40 bytes
        // Keys evicted: 0, 1, 2, 3
        assert_eq!(cache.total_size(), 40);
        assert_eq!(cache.get(&TestKey(0)), None);
        assert_eq!(cache.get(&TestKey(1)), None);
        assert_eq!(cache.get(&TestKey(2)), None);
        assert_eq!(cache.get(&TestKey(3)), None);

        // Insert 10th value, size is 8*6=48 bytes
        cache.insert(TestKey(9), 9);
        assert_eq!(cache.total_size(), 48);
        assert_eq!(cache.get(&TestKey(9)), Some(9));
    }
}

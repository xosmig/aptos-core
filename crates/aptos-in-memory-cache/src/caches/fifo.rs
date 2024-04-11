// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{Cache, OrderedCache};
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
    K: Hash + Eq + PartialEq + Send + Sync + Clone + 'static,
    V: Send + Sync + Clone + 'static,
{
    /// Cache maps the cache key to the deserialized Transaction.
    items: Arc<DashMap<K, V>>,
    insert_notify: Arc<Notify>,
    cache_metadata: Arc<RwLock<CacheMetadata<K>>>,
    _cancellation_token_drop_guard: tokio_util::sync::DropGuard,
    next_key_function:
        Arc<dyn Fn(&K, &dyn Fn(&K) -> Option<V>) -> Option<K> + Send + Sync + 'static>,
}

impl<K, V> FIFOCache<K, V>
where
    K: Hash + Eq + PartialEq + Send + Sync + Clone + Debug + 'static,
    V: Send + Sync + Clone + 'static,
{
    pub fn new(
        target_size_in_bytes: u64,
        eviction_trigger_size_in_bytes: u64,
        next_key_function: impl Fn(&K, &dyn Fn(&K) -> Option<V>) -> Option<K> + Send + Sync + 'static,
    ) -> Self {
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
        let next_key_function = Arc::new(next_key_function);

        let cache = Self {
            items,
            insert_notify,
            cache_metadata,
            _cancellation_token_drop_guard: cancellation_token.clone().drop_guard(),
            next_key_function,
        };

        cache.spawn_cleanup_task(cancellation_token);
        cache
    }

    fn evict(
        items: Arc<DashMap<K, V>>,
        cache_metadata: Arc<RwLock<CacheMetadata<K>>>,
        next_key_function: Arc<
            dyn Fn(&K, &dyn Fn(&K) -> Option<V>) -> Option<K> + Send + Sync + 'static,
        >,
    ) {
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
        let getter = |k: &K| -> Option<V> { items.get(k).map(|r| r.value().clone()) };
        while bytes_to_remove > 0 {
            if let Some(key_to_remove) = current_cache_metadata.first_key.clone() {
                let (_k, v) = items
                    .remove(&key_to_remove)
                    .expect("Failed to remove the key");
                let size_of_v = std::mem::size_of_val(&v) as u64;
                bytes_to_remove = bytes_to_remove.saturating_sub(size_of_v);
                actual_bytes_removed += size_of_v;
                current_cache_metadata.first_key =
                    Some((next_key_function)(&key_to_remove, &getter).unwrap());
            } else {
                break;
            }
        }

        current_cache_metadata.total_size_in_bytes -= actual_bytes_removed;
    }

    /// Perform cache eviction on a separate task.
    fn spawn_cleanup_task(&self, cancellation_token: tokio_util::sync::CancellationToken) {
        let insert_notify = self.insert_notify.clone();
        let items = self.items.clone();
        let cache_metadata = self.cache_metadata.clone();
        let next_key_function = self.next_key_function.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = insert_notify.notified() => {
                        Self::evict(items.clone(), cache_metadata.clone(), next_key_function.clone());
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
    K: Hash + Eq + PartialEq + Send + Sync + Clone,
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
        let last_key = { self.cache_metadata.read().last_key.clone() };
        if let Some(k) = last_key {
            let getter = |k: &K| -> Option<V> { self.items.get(k).map(|r| r.value().clone()) };
            if (self.next_key_function)(&k, &getter).unwrap() != key {
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
    K: Hash + Eq + PartialEq + Send + Sync + Clone,
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
    use crate::{caches::fifo::FIFOCache, Cache};
    use std::{sync::Arc, time::Duration};

    #[tokio::test]
    async fn test_insert_four_values() {
        let cache = FIFOCache::<u64, u64>::new(100, 200, |key, _| Some(key + 1));
        let cache = Arc::new(cache);
        tokio::time::sleep(Duration::from_nanos(1)).await;

        let key1 = 1;
        let key2 = 2;
        let key3 = 3;
        let key4 = 4;

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
        let cache = FIFOCache::<u64, u64>::new(40, 64, |key, _| Some(key + 1));
        let cache = Arc::new(cache);

        // Insert 8 values, size is 8*8=64 bytes
        for i in 0..8 {
            cache.insert(i, i);
            assert_eq!(cache.total_size(), ((i + 1) * 8) as u64);
        }

        for i in 0..8 {
            assert_eq!(cache.get(&i), Some(i));
        }

        // Insert 9th value, size is 8*9=72>64 bytes, eviction threshold reached
        // Evicts until target size size is reached
        // Sleep for 1 second to ensure eviction task finishes
        tokio::time::sleep(Duration::from_nanos(1)).await;
        cache.insert(8, 8);
        tokio::time::sleep(Duration::from_nanos(1)).await;
        // New size is 8*5=40 bytes
        // Keys evicted: 0, 1, 2, 3
        assert_eq!(cache.total_size(), 40);
        assert_eq!(cache.get(&0), None);
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&3), None);

        // Insert 10th value, size is 8*6=48 bytes
        cache.insert(9, 9);
        assert_eq!(cache.total_size(), 48);
        assert_eq!(cache.get(&9), Some(9));
    }
}

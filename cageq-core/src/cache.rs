//! A small bounded FIFO cache — the main fit's and the mobile export's, which used to keep a
//! copy each. FIFO, as `sidecar_dsp.py`'s caches are (a Python dict's insertion order evicts the
//! oldest entry for free; `order` tracks it here, since `HashMap` doesn't).

use std::collections::{HashMap, VecDeque};

/// Entries each cache keeps (`sidecar_dsp.py`'s `_FIT_CACHE_MAX` / `_EXPORT_FIT_CACHE`: 32).
const CACHE_MAX: usize = 32;

pub(crate) struct BoundedCache<K, V> {
    entries: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K: Clone + Eq + std::hash::Hash, V: Clone> BoundedCache<K, V> {
    pub(crate) fn new() -> Self {
        BoundedCache { entries: HashMap::new(), order: VecDeque::new() }
    }

    pub(crate) fn get(&self, key: &K) -> Option<V> {
        self.entries.get(key).cloned()
    }

    pub(crate) fn insert(&mut self, key: K, value: V) {
        if !self.entries.contains_key(&key) {
            if self.order.len() >= CACHE_MAX
                && let Some(oldest) = self.order.pop_front()
            {
                self.entries.remove(&oldest);
            }
            self.order.push_back(key.clone());
        }
        self.entries.insert(key, value);
    }
}

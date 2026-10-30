//! The imports of painted surfaces, kept across frames (Linux).
//!
//! Free of crate dependencies, so `tests/surface_cache.rs` compiles it on its own.

use std::collections::HashMap;
use std::hash::Hash;

/// How many frames in a row an entry may go unused before it is dropped.
pub(crate) const EVICT_AFTER_FRAMES: u64 = 8;

/// Values made once per key and reused on later frames, each dropped once it has not been used for
/// [`EVICT_AFTER_FRAMES`] frames.
pub(crate) struct SurfaceCache<K, V> {
    entries: HashMap<K, Entry<V>>,
    frame: u64,
}

struct Entry<V> {
    value: V,
    last_used: u64,
}

impl<K: Hash + Eq, V> SurfaceCache<K, V> {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            frame: 0,
        }
    }

    /// Starts a frame, dropping the entries that went unused for the last [`EVICT_AFTER_FRAMES`] frames.
    pub(crate) fn begin_frame(&mut self) {
        self.frame += 1;
        let frame = self.frame;
        self.entries
            .retain(|_, entry| frame - entry.last_used <= EVICT_AFTER_FRAMES);
    }

    /// The value for `key`, made with `make` if there is none, and marked as used in this frame. A value `make`
    /// fails to make is not kept, so the next frame tries again.
    pub(crate) fn get_or_try_insert_with<E>(
        &mut self,
        key: K,
        make: impl FnOnce() -> Result<V, E>,
    ) -> Result<&V, E> {
        let frame = self.frame;
        let entry = match self.entries.entry(key) {
            std::collections::hash_map::Entry::Occupied(occupied) => occupied.into_mut(),
            std::collections::hash_map::Entry::Vacant(vacant) => vacant.insert(Entry {
                value: make()?,
                last_used: frame,
            }),
        };
        entry.last_used = frame;
        Ok(&entry.value)
    }

    /// How many values are kept.
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

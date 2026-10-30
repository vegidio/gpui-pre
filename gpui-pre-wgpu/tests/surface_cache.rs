//! The cache of painted surfaces' imports, compiled on its own: the crate's unit tests need Zed's repository to build.
#![cfg(target_os = "linux")]

#[path = "../src/surface_cache.rs"]
mod surface_cache;

use std::cell::Cell;
use surface_cache::{EVICT_AFTER_FRAMES, SurfaceCache};

#[test]
fn makes_each_value_once_and_reuses_it() {
    let mut cache = SurfaceCache::new();
    let made = Cell::new(0);
    let make = |value: &'static str| {
        made.set(made.get() + 1);
        Ok::<_, ()>(value)
    };
    for _ in 0..3 {
        cache.begin_frame();
        assert_eq!(cache.get_or_try_insert_with(1, || make("one")), Ok(&"one"));
        assert_eq!(cache.get_or_try_insert_with(2, || make("two")), Ok(&"two"));
    }
    assert_eq!(made.get(), 2, "one make per key");
    assert_eq!(cache.len(), 2);
}

#[test]
fn drops_values_unused_for_the_eviction_window() {
    let mut cache = SurfaceCache::new();
    cache.begin_frame();
    cache
        .get_or_try_insert_with("old", || Ok::<_, ()>(()))
        .unwrap();
    for _ in 0..EVICT_AFTER_FRAMES {
        cache.begin_frame();
        cache
            .get_or_try_insert_with("kept", || Ok::<_, ()>(()))
            .unwrap();
    }
    assert_eq!(
        cache.len(),
        2,
        "kept through {EVICT_AFTER_FRAMES} unused frames"
    );
    cache.begin_frame();
    assert_eq!(cache.len(), 1, "dropped once unused for more");

    let mut remade = false;
    cache
        .get_or_try_insert_with("old", || {
            remade = true;
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(remade, "an evicted value is made again");
}

#[test]
fn a_value_that_fails_is_not_kept() {
    let mut cache = SurfaceCache::<u32, u32>::new();
    cache.begin_frame();
    assert_eq!(cache.get_or_try_insert_with(1, || Err("no")), Err("no"));
    assert_eq!(cache.len(), 0);
    cache.begin_frame();
    assert_eq!(
        cache.get_or_try_insert_with(1, || Ok::<_, &str>(7)),
        Ok(&7),
        "the next frame tries again"
    );
}

#[test]
fn a_new_cache_is_empty() {
    // The cache is part of the device's resources, which a device loss drops; the new device starts with a new one.
    let cache = SurfaceCache::<u32, u32>::new();
    assert_eq!(cache.len(), 0);
}

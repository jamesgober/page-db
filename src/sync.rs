//! Concurrency primitives, switched between `std` and `loom`.
//!
//! The buffer pool's correctness under contention is verified with `loom`,
//! which needs its own instrumented `Arc`, `Mutex`, `RwLock`, and atomics to
//! explore thread interleavings. Building with `--cfg loom` swaps the real
//! primitives for loom's; every other build uses the standard library. Nothing
//! else in the crate needs to know which is in play.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(not(loom))]
pub(crate) use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock a mutex, recovering the guard if a previous holder panicked.
///
/// The pool's critical sections do not panic, so poisoning cannot actually
/// happen — this is the lint-clean way to take the guard without `unwrap`,
/// and it keeps a hypothetical poisoned lock usable rather than cascading the
/// panic to every later caller.
#[inline]
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Take a read lock, recovering from poisoning (see [`lock`]).
#[inline]
pub(crate) fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Take a write lock, recovering from poisoning (see [`lock`]).
#[inline]
pub(crate) fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

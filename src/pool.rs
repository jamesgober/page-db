//! [`BufferPool`]: a bounded in-memory cache of pages over a [`PageStore`].
//!
//! The pool keeps a fixed number of frames resident, each holding one page. A
//! caller asks for a page by id and gets back a [`PageGuard`] that pins the
//! frame — a pinned frame is never evicted — and dropping the guard unpins it.
//! Writing through the guard marks the frame dirty; a dirty frame is always
//! flushed to the store before its frame is reused. Those two rules — never
//! evict a pinned page, never lose a dirty page without flushing — are the
//! invariants the property tests and the loom model checks hold the pool to.
//!
//! Eviction is the clock (second-chance) algorithm: each frame carries a
//! reference bit set when it is touched; the clock hand sweeps frames, clearing
//! set bits and skipping pinned frames, and reuses the first unset, unpinned
//! frame it finds. If every frame is pinned, admission fails with
//! [`PageError::BufferPoolExhausted`] rather than evicting something it must
//! not.
//!
//! For v0.3.0 the pool serializes its bookkeeping — and the miss-path store I/O
//! — under a single mutex. That keeps the pin/evict/dirty logic small enough to
//! verify exhaustively with loom; sharding the table to remove the single-lock
//! bottleneck is a later, measured change behind the same API.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use crate::error::{PageError, PageResult};
use crate::file::PageFile;
use crate::page::{Page, PageId, PageSize};
use crate::store::PageStore;
use crate::sync::{self, Arc, AtomicBool, AtomicU64, AtomicUsize, Mutex, Ordering, RwLock};
use crate::sync::{RwLockReadGuard, RwLockWriteGuard};

/// Sentinel id for a frame that holds no page yet.
const NO_PAGE: u64 = u64::MAX;

/// One cache frame: a reusable page buffer plus its residency bookkeeping.
struct FrameInner {
    /// The page bytes. Reused in place across the pages that occupy this frame.
    page: RwLock<Page>,
    /// The id of the page currently resident, or [`NO_PAGE`].
    id: AtomicU64,
    /// Outstanding pins. A frame with `pin > 0` is never evicted.
    pin: AtomicUsize,
    /// Set when the resident page has unflushed modifications.
    dirty: AtomicBool,
    /// The clock reference bit: set on access, cleared by the clock sweep.
    referenced: AtomicBool,
}

impl FrameInner {
    fn new(page: Page) -> Self {
        Self {
            page: RwLock::new(page),
            id: AtomicU64::new(NO_PAGE),
            pin: AtomicUsize::new(0),
            dirty: AtomicBool::new(false),
            referenced: AtomicBool::new(false),
        }
    }

    #[inline]
    fn resident_id(&self) -> PageId {
        PageId::new(self.id.load(Ordering::Acquire))
    }
}

/// The pool's mutable bookkeeping, guarded by one mutex.
struct Core {
    /// Resident page id to frame index.
    map: HashMap<PageId, usize>,
    /// Frame indices never yet filled, available without eviction.
    free: Vec<usize>,
    /// The clock hand.
    hand: usize,
}

/// A bounded cache of pages over a [`PageStore`].
///
/// `BufferPool<S>` is generic over its backing store; the default is
/// [`PageFile`], so `BufferPool` without a type parameter is a pool over a file
/// of pages. The handle is `Send + Sync` and every method takes `&self`, so it
/// is shared across threads behind an `Arc` with no outer lock.
///
/// # Examples
///
/// ```
/// use page_db::{BufferPool, PageId, Lsn, DEFAULT_PAGE_SIZE};
///
/// # let dir = tempfile::tempdir().unwrap();
/// # let path = dir.path().join("data.pages");
/// // A pool of 128 frames over a 4 KiB-page file.
/// let pool = BufferPool::open(&path, DEFAULT_PAGE_SIZE, 128)?;
///
/// // Create page 0, write to it (which marks it dirty), then release the pin.
/// {
///     let guard = pool.new_page(PageId::new(0))?;
///     let mut page = guard.write();
///     page.set_lsn(Lsn::new(1));
///     page.payload_mut()[..5].copy_from_slice(b"hello");
/// }
///
/// // Flush dirty pages to the file and make them durable.
/// pool.flush_all()?;
/// pool.sync()?;
///
/// // Fetch it back — served from cache if resident, else read from the file.
/// let guard = pool.fetch(PageId::new(0))?;
/// assert_eq!(&guard.read().payload()[..5], b"hello");
/// # Ok::<(), page_db::PageError>(())
/// ```
pub struct BufferPool<S = PageFile> {
    store: S,
    frames: Vec<Arc<FrameInner>>,
    core: Mutex<Core>,
    capacity: usize,
}

impl BufferPool<PageFile> {
    /// Open a page file and wrap it in a pool of `capacity` frames.
    ///
    /// A convenience over [`PageFile::open`] followed by [`BufferPool::new`].
    ///
    /// # Errors
    ///
    /// Returns [`PageError::Io`] if the file cannot be opened.
    pub fn open<P: AsRef<Path>>(path: P, page_size: PageSize, capacity: usize) -> PageResult<Self> {
        let file = PageFile::open(path, page_size)?;
        Ok(Self::new(file, capacity))
    }
}

impl<S: PageStore> BufferPool<S> {
    /// Build a pool of `capacity` frames over `store`.
    ///
    /// `capacity` is the number of pages held resident; it is clamped up to at
    /// least one. The frame buffers are allocated once, here — the pool does no
    /// per-request allocation on the hot path.
    #[must_use]
    pub fn new(store: S, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let mut frames = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            frames.push(Arc::new(FrameInner::new(store.allocate_page())));
        }
        let free = (0..capacity).collect();
        Self {
            store,
            frames,
            core: Mutex::new(Core {
                map: HashMap::with_capacity(capacity),
                free,
                hand: 0,
            }),
            capacity,
        }
    }

    /// The number of frames in the pool.
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of pages currently resident.
    #[must_use]
    pub fn resident_len(&self) -> usize {
        sync::lock(&self.core).map.len()
    }

    /// Whether page `id` is currently held in the pool.
    #[must_use]
    pub fn is_resident(&self, id: PageId) -> bool {
        sync::lock(&self.core).map.contains_key(&id)
    }

    /// Fetch the page at `id`, pinning it and returning a guard.
    ///
    /// Served from cache if resident; otherwise a frame is found (a free one, or
    /// an evicted victim, flushing it first if dirty) and the page is read from
    /// the store into it. The returned [`PageGuard`] holds a pin for its
    /// lifetime.
    ///
    /// # Errors
    ///
    /// - [`PageError::BufferPoolExhausted`] if every frame is pinned.
    /// - Whatever the store's read returns (for a file:
    ///   [`PageError::ShortRead`] past end-of-file, or an integrity error).
    /// - [`PageError::Io`] if flushing an evicted dirty victim fails.
    pub fn fetch(&self, id: PageId) -> PageResult<PageGuard> {
        let mut core = sync::lock(&self.core);

        if let Some(&slot) = core.map.get(&id) {
            let frame = self.frames[slot].clone();
            let _ = frame.pin.fetch_add(1, Ordering::AcqRel);
            frame.referenced.store(true, Ordering::Release);
            return Ok(PageGuard { frame });
        }

        let slot = self.take_slot(&mut core)?;
        {
            let frame = &self.frames[slot];
            let mut page = sync::write(&frame.page);
            if let Err(err) = self.store.read_into(id, &mut page) {
                drop(page);
                core.free.push(slot);
                return Err(err);
            }
        }
        Ok(self.install(&mut core, slot, id, false))
    }

    /// Introduce a fresh, zeroed page at `id`, pinning it and returning a guard.
    ///
    /// The page is created in memory and marked dirty, so it is written to the
    /// store on the next flush; no read is performed. If `id` is already
    /// resident it is reset to a blank page. The caller chooses the id — the
    /// free-list allocator that picks ids is a later release.
    ///
    /// # Errors
    ///
    /// - [`PageError::BufferPoolExhausted`] if every frame is pinned.
    /// - [`PageError::Io`] if flushing an evicted dirty victim fails.
    pub fn new_page(&self, id: PageId) -> PageResult<PageGuard> {
        let mut core = sync::lock(&self.core);

        if let Some(&slot) = core.map.get(&id) {
            let frame = self.frames[slot].clone();
            sync::write(&frame.page).reset();
            let _ = frame.pin.fetch_add(1, Ordering::AcqRel);
            frame.dirty.store(true, Ordering::Release);
            frame.referenced.store(true, Ordering::Release);
            return Ok(PageGuard { frame });
        }

        let slot = self.take_slot(&mut core)?;
        sync::write(&self.frames[slot].page).reset();
        Ok(self.install(&mut core, slot, id, true))
    }

    /// Flush page `id` to the store if it is resident and dirty.
    ///
    /// This places the bytes in the store; call [`sync`](BufferPool::sync) to
    /// make them durable. Do not call this while holding a write guard to the
    /// same page on the same thread — flushing takes the frame's lock.
    ///
    /// # Errors
    ///
    /// Whatever the store's write returns.
    pub fn flush(&self, id: PageId) -> PageResult<()> {
        let core = sync::lock(&self.core);
        if let Some(&slot) = core.map.get(&id) {
            self.flush_slot(slot, id)?;
        }
        Ok(())
    }

    /// Flush every dirty resident page to the store.
    ///
    /// # Errors
    ///
    /// Whatever the store's write returns. On error, some pages may already have
    /// been flushed; the operation is safe to retry.
    pub fn flush_all(&self) -> PageResult<()> {
        let core = sync::lock(&self.core);
        for (&id, &slot) in core.map.iter() {
            self.flush_slot(slot, id)?;
        }
        Ok(())
    }

    /// Flush all dirty pages, then make the store durable.
    ///
    /// Equivalent to [`flush_all`](BufferPool::flush_all) followed by
    /// [`sync`](BufferPool::sync) — the common checkpoint sequence.
    ///
    /// # Errors
    ///
    /// Whatever flushing or the store's sync returns.
    pub fn checkpoint(&self) -> PageResult<()> {
        self.flush_all()?;
        self.sync()
    }

    /// Make the store durable (the pages already written to it).
    ///
    /// This does not flush dirty cached pages first; use
    /// [`flush_all`](BufferPool::flush_all) or
    /// [`checkpoint`](BufferPool::checkpoint) for that.
    ///
    /// # Errors
    ///
    /// Whatever the store's sync returns.
    pub fn sync(&self) -> PageResult<()> {
        self.store.sync()
    }

    /// Install a freshly loaded or created page into `slot`, returning a pinned
    /// guard. Caller holds `core` and has already populated the frame's buffer.
    fn install(&self, core: &mut Core, slot: usize, id: PageId, dirty: bool) -> PageGuard {
        let frame = &self.frames[slot];
        frame.id.store(id.get(), Ordering::Release);
        frame.dirty.store(dirty, Ordering::Release);
        frame.referenced.store(true, Ordering::Release);
        frame.pin.store(1, Ordering::Release);
        let _ = core.map.insert(id, slot);
        PageGuard {
            frame: self.frames[slot].clone(),
        }
    }

    /// Flush the page in `slot` (resident id `id`) if dirty.
    fn flush_slot(&self, slot: usize, id: PageId) -> PageResult<()> {
        let frame = &self.frames[slot];
        if frame.dirty.load(Ordering::Acquire) {
            let mut page = sync::write(&frame.page);
            self.store.write_page(id, &mut page)?;
            frame.dirty.store(false, Ordering::Release);
        }
        Ok(())
    }

    /// Obtain a frame slot to fill: a free one, or an evicted victim (flushed
    /// first if dirty). Caller holds `core`.
    fn take_slot(&self, core: &mut Core) -> PageResult<usize> {
        if let Some(slot) = core.free.pop() {
            return Ok(slot);
        }
        let slot = match self.find_victim(core) {
            Some(slot) => slot,
            None => {
                return Err(PageError::BufferPoolExhausted {
                    capacity: self.capacity,
                });
            }
        };
        let victim_id = self.frames[slot].resident_id();
        self.flush_slot(slot, victim_id)?;
        let _ = core.map.remove(&victim_id);
        Ok(slot)
    }

    /// The clock sweep: return an unpinned frame to reuse, or `None` if all
    /// frames are pinned. Caller holds `core`.
    fn find_victim(&self, core: &mut Core) -> Option<usize> {
        let n = self.capacity;
        // Two full passes: the first clears reference bits, the second selects.
        // If every frame stays pinned across both, the pool is exhausted.
        let mut steps = 0;
        while steps < 2 * n {
            let slot = core.hand;
            core.hand = (core.hand + 1) % n;
            steps += 1;

            let frame = &self.frames[slot];
            if frame.pin.load(Ordering::Acquire) > 0 {
                continue;
            }
            if frame.referenced.swap(false, Ordering::AcqRel) {
                continue;
            }
            return Some(slot);
        }
        None
    }
}

/// A pin on a cached page.
///
/// While a `PageGuard` is alive the page stays resident and unevictable. Read
/// the page with [`read`](PageGuard::read) and write it with
/// [`write`](PageGuard::write); taking a write guard marks the page dirty.
/// Dropping the `PageGuard` releases the pin.
pub struct PageGuard {
    frame: Arc<FrameInner>,
}

impl PageGuard {
    /// The id of the pinned page.
    #[inline]
    #[must_use]
    pub fn id(&self) -> PageId {
        self.frame.resident_id()
    }

    /// Whether the page has unflushed modifications.
    #[inline]
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.frame.dirty.load(Ordering::Acquire)
    }

    /// Borrow the page for reading. Multiple readers of the same page proceed
    /// concurrently.
    #[inline]
    #[must_use]
    pub fn read(&self) -> PageRef<'_> {
        PageRef {
            guard: sync::read(&self.frame.page),
        }
    }

    /// Borrow the page for writing, marking it dirty.
    ///
    /// The page is recorded dirty as soon as the write guard is taken, so it
    /// will be flushed even if the actual mutation is conditional.
    #[inline]
    #[must_use]
    pub fn write(&self) -> PageMut<'_> {
        self.frame.dirty.store(true, Ordering::Release);
        PageMut {
            guard: sync::write(&self.frame.page),
        }
    }
}

impl Drop for PageGuard {
    fn drop(&mut self) {
        let _ = self.frame.pin.fetch_sub(1, Ordering::AcqRel);
    }
}

impl std::fmt::Debug for PageGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageGuard")
            .field("id", &self.id())
            .field("dirty", &self.is_dirty())
            .finish()
    }
}

/// A shared read borrow of a pinned page. Dereferences to [`Page`].
pub struct PageRef<'a> {
    guard: RwLockReadGuard<'a, Page>,
}

impl Deref for PageRef<'_> {
    type Target = Page;
    #[inline]
    fn deref(&self) -> &Page {
        &self.guard
    }
}

/// An exclusive write borrow of a pinned page. Dereferences to [`Page`].
pub struct PageMut<'a> {
    guard: RwLockWriteGuard<'a, Page>,
}

impl Deref for PageMut<'_> {
    type Target = Page;
    #[inline]
    fn deref(&self) -> &Page {
        &self.guard
    }
}

impl DerefMut for PageMut<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Page {
        &mut self.guard
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::*;
    use crate::page::Lsn;
    use crate::test_store::MemStore;

    const PS: usize = 4096;

    fn pool(capacity: usize) -> BufferPool<MemStore> {
        BufferPool::new(MemStore::new(PS), capacity)
    }

    #[test]
    fn test_new_page_then_fetch_serves_from_cache() {
        let pool = pool(8);
        {
            let guard = pool.new_page(PageId::new(0)).unwrap();
            guard.write().payload_mut()[0] = 0x7A;
        }
        assert!(pool.is_resident(PageId::new(0)));
        let guard = pool.fetch(PageId::new(0)).unwrap();
        assert_eq!(guard.read().payload()[0], 0x7A);
    }

    #[test]
    fn test_capacity_is_clamped_up_to_one() {
        assert_eq!(pool(0).capacity(), 1);
    }

    #[test]
    fn test_pinned_page_is_never_evicted() {
        let pool = pool(1);
        let _held = pool.new_page(PageId::new(0)).unwrap();
        // The only frame is pinned, so admitting another page must fail rather
        // than evict the pinned one.
        assert!(matches!(
            pool.new_page(PageId::new(1)),
            Err(PageError::BufferPoolExhausted { capacity: 1 })
        ));
        assert!(pool.is_resident(PageId::new(0)));
    }

    #[test]
    fn test_dirty_page_is_flushed_before_eviction() {
        let pool = pool(1);
        {
            let guard = pool.new_page(PageId::new(0)).unwrap();
            guard.write().set_lsn(Lsn::new(9));
        } // page 0 is dirty and unpinned

        // Force eviction of page 0 by reusing the only frame.
        {
            let _ = pool.new_page(PageId::new(1)).unwrap();
        }
        // Page 0 must have been written to the store on its way out.
        assert!(pool.store_contains(0));
        // And it reads back with the data it held.
        let guard = pool.fetch(PageId::new(0)).unwrap();
        assert_eq!(guard.read().lsn(), Lsn::new(9));
    }

    #[test]
    fn test_clock_keeps_the_recently_used_page() {
        let pool = pool(2);
        let _ = pool.new_page(PageId::new(0)).unwrap();
        let _ = pool.new_page(PageId::new(1)).unwrap();
        pool.flush_all().unwrap();

        // Touch 0 so its reference bit is set, then admit 2: the clock should
        // evict 1 (untouched), not 0.
        let _ = pool.fetch(PageId::new(0)).unwrap();
        let _ = pool.new_page(PageId::new(2)).unwrap();

        assert!(pool.is_resident(PageId::new(0)));
        assert!(!pool.is_resident(PageId::new(1)));
        assert!(pool.is_resident(PageId::new(2)));
    }

    #[test]
    fn test_flush_clears_dirty() {
        let pool = pool(4);
        {
            let guard = pool.new_page(PageId::new(0)).unwrap();
            assert!(guard.is_dirty());
        }
        pool.flush(PageId::new(0)).unwrap();
        let guard = pool.fetch(PageId::new(0)).unwrap();
        assert!(!guard.is_dirty());
    }

    #[test]
    fn test_fetch_missing_unwritten_page_errors() {
        let pool = pool(4);
        assert!(matches!(
            pool.fetch(PageId::new(99)),
            Err(PageError::ShortRead { .. })
        ));
        // A failed miss must not leak the frame it borrowed.
        assert_eq!(pool.resident_len(), 0);
        assert_eq!(pool.capacity(), 4);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Through any sequence of fetches and dirtying writes against a pool
        /// smaller than the working set, every page always reads back the last
        /// value written to it — nothing is lost to eviction, nothing is stale.
        #[test]
        fn pool_never_loses_data(
            ops in proptest::collection::vec((0u8..6, any::<u8>(), any::<bool>()), 1..200),
        ) {
            const N: u64 = 6;
            let pool = pool(2);                       // 2 frames, 6 pages
            let mut expected: HashMap<u64, u8> = HashMap::new();

            // Seed every page so all fetches resolve.
            for id in 0..N {
                let guard = pool.new_page(PageId::new(id)).unwrap();
                guard.write().payload_mut()[0] = 0;
                let _ = expected.insert(id, 0);
                drop(guard);
            }
            pool.flush_all().unwrap();

            for (id, marker, write) in ops {
                let id = id as u64 % N;
                let guard = pool.fetch(PageId::new(id)).unwrap();
                // Read must match the model.
                prop_assert_eq!(guard.read().payload()[0], expected[&id]);
                if write {
                    guard.write().payload_mut()[0] = marker;
                    let _ = expected.insert(id, marker);
                }
            }

            // After a checkpoint, a cold reread of every page still matches.
            pool.flush_all().unwrap();
            for id in 0..N {
                let guard = pool.fetch(PageId::new(id)).unwrap();
                prop_assert_eq!(guard.read().payload()[0], expected[&id]);
            }
        }
    }

    // Helper hook used by the eviction test, kept here so the store stays
    // private to the crate.
    impl BufferPool<MemStore> {
        fn store_contains(&self, id: u64) -> bool {
            self.store.contains(id)
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use crate::sync::Arc;
    use crate::test_store::MemStore;

    /// A pinned page is never evicted: while one thread holds a pin on the only
    /// frame, another thread's attempt to admit a different page fails rather
    /// than evicting the pinned one, under every interleaving.
    #[test]
    fn loom_pinned_page_never_evicted() {
        loom::model(|| {
            let pool = Arc::new(BufferPool::new(MemStore::new(4096), 1));
            let held = pool.new_page(PageId::new(0)).unwrap();

            let p = Arc::clone(&pool);
            let other = loom::thread::spawn(move || p.new_page(PageId::new(1)).is_err());

            // The pinned page stays resident no matter how the threads interleave.
            assert!(pool.is_resident(PageId::new(0)));
            let admit_failed = other.join().unwrap();
            assert!(admit_failed);
            assert_eq!(held.id(), PageId::new(0));
            drop(held);
        });
    }

    /// A dirty page is never lost: when an unpinned dirty page is evicted to
    /// make room, it is flushed to the store first, under every interleaving.
    #[test]
    fn loom_dirty_page_flushed_on_eviction() {
        loom::model(|| {
            let store_pages = {
                let pool = Arc::new(BufferPool::new(MemStore::new(4096), 1));
                {
                    let guard = pool.new_page(PageId::new(0)).unwrap();
                    guard.write().payload_mut()[0] = 0x5A;
                }

                let p = Arc::clone(&pool);
                let t = loom::thread::spawn(move || {
                    // Admitting page 1 reuses the only frame, evicting page 0,
                    // which is dirty and so must be flushed first.
                    let _ = p.new_page(PageId::new(1)).unwrap();
                });
                t.join().unwrap();
                pool.store_contains_loom(0)
            };
            assert!(store_pages, "evicted dirty page 0 was not flushed");
        });
    }

    impl BufferPool<MemStore> {
        fn store_contains_loom(&self, id: u64) -> bool {
            self.store.contains(id)
        }
    }
}

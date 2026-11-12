//! [`BufferPool`]: a bounded in-memory cache of pages over a [`PageStore`].
//!
//! The pool keeps a fixed number of frames resident, each holding one page. A
//! caller asks for a page by id and gets back a [`PageGuard`] that pins the
//! frame (a pinned frame is never evicted), and dropping the guard unpins it.
//! Writing through the guard marks the frame dirty; a dirty frame is always
//! flushed to the store before its frame is reused. Those two rules (never
//! evict a pinned page, never lose a dirty page without flushing) are the
//! invariants the property tests and the loom model checks hold the pool to.
//!
//! Eviction is the clock (second-chance) algorithm: each frame carries a
//! reference bit set when it is touched; the clock hand sweeps frames, clearing
//! set bits and skipping pinned frames, and reuses the first unset, unpinned
//! frame it finds. If every frame is pinned, admission fails with
//! [`PageError::BufferPoolExhausted`] rather than evicting something it must
//! not.
//!
//! ## Write-ahead rule
//!
//! By default the pool writes a dirty page whenever it is flushed or evicted,
//! whatever its LSN. A database that keeps a write-ahead log installs a
//! [`LogWatermark`] with [`BufferPool::with_log_watermark`]: the pool then
//! never writes a page whose LSN is beyond the log's durable point. Eviction
//! passes over such pages (forcing the log only when nothing else can be
//! evicted), and a flush forces the log first or leaves the page dirty.
//!
//! ## Locking
//!
//! The pool's bookkeeping (the id-to-frame map, the free frames, the clock
//! hand) sits under one mutex, and each frame's page under its own
//! reader-writer lock. The dirty bit and a frame's resident id change only while
//! that frame's page lock is held, so a flush and a write are ordered by the
//! lock and a write can never land on a frame the flush has just marked clean.
//!
//! The lock order is: the pool mutex may be held while taking the page lock of
//! an **unpinned** frame, and a thread holding a page lock (through a
//! [`PageRef`] or [`PageMut`]) may take the pool mutex, as latch coupling down
//! a tree does when it fetches a child under the parent's latch. Nothing waits
//! on the page lock of a pinned frame while holding the pool mutex: flushes
//! find their targets under the mutex, release it, and only then lock each
//! page. A miss still reads from the store, and evicts a dirty victim, under the
//! mutex; moving that I/O out is a later change behind the same API.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use crate::error::{PageError, PageResult, sync_poisoned};
use crate::file::PageFile;
use crate::page::{Lsn, Page, PageId, PageSize};
use crate::store::PageStore;
use crate::sync::{self, Arc, AtomicBool, AtomicU64, AtomicUsize, Mutex, Ordering, RwLock};
use crate::sync::{RwLockReadGuard, RwLockWriteGuard};

/// Sentinel id for a frame that holds no page yet.
const NO_PAGE: u64 = u64::MAX;

/// The durable point of a write-ahead log, consulted before the pool writes a
/// page.
///
/// The write-ahead rule says a page may reach disk only once the log records up
/// to its LSN are durable. A pool with a watermark
/// ([`BufferPool::with_log_watermark`]) reads each dirty page's
/// [`lsn`](Page::lsn) before writing it, and writes it only if the LSN is at or
/// below [`durable_lsn`](LogWatermark::durable_lsn). Pages that have never been
/// logged carry [`Lsn::ZERO`] and are always writable.
///
/// Any `Fn() -> Lsn + Send + Sync` closure is a watermark that cannot force the
/// log. Implement the trait directly to supply [`force`](LogWatermark::force)
/// as well, so a flush can make the log durable instead of skipping the page.
///
/// The pool may call both methods while it holds its own internal locks, so an
/// implementation must not call back into the pool.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicU64, Ordering};
/// use page_db::{BufferPool, Lsn, PageId, DEFAULT_PAGE_SIZE};
///
/// # let dir = tempfile::tempdir().unwrap();
/// # let path = dir.path().join("data.pages");
/// // The log's durable end, advanced by whatever syncs the log.
/// let durable = Arc::new(AtomicU64::new(0));
/// let seen = Arc::clone(&durable);
/// let pool = BufferPool::open(&path, DEFAULT_PAGE_SIZE, 64)?
///     .with_log_watermark(move || Lsn::new(seen.load(Ordering::Acquire)));
///
/// {
///     let guard = pool.new_page(PageId::new(0))?;
///     guard.write().set_lsn(Lsn::new(10));
/// }
/// pool.flush_all()?;                      // LSN 10 is not durable: not written
/// assert!(pool.fetch(PageId::new(0))?.is_dirty());
///
/// durable.store(10, Ordering::Release);   // the log reached LSN 10
/// pool.flush_all()?;                      // now it is written
/// assert!(!pool.fetch(PageId::new(0))?.is_dirty());
/// # Ok::<(), page_db::PageError>(())
/// ```
pub trait LogWatermark: Send + Sync {
    /// The highest LSN whose log record is durable.
    fn durable_lsn(&self) -> Lsn;

    /// Make the log durable through at least `lsn` if possible, and return the
    /// durable point afterwards.
    ///
    /// The default cannot force the log and returns
    /// [`durable_lsn`](LogWatermark::durable_lsn) unchanged, so a page beyond it
    /// is skipped rather than written.
    ///
    /// # Errors
    ///
    /// Whatever forcing the log returns; the pool passes it to its caller.
    fn force(&self, lsn: Lsn) -> PageResult<Lsn> {
        let _ = lsn;
        Ok(self.durable_lsn())
    }
}

impl<F> LogWatermark for F
where
    F: Fn() -> Lsn + Send + Sync,
{
    #[inline]
    fn durable_lsn(&self) -> Lsn {
        self()
    }
}

/// One cache frame: a reusable page buffer plus its residency bookkeeping.
struct FrameInner {
    /// The page bytes. Reused in place across the pages that occupy this frame.
    page: RwLock<Page>,
    /// The id of the page currently resident, or [`NO_PAGE`]. Changed only
    /// while `page` is write-locked.
    id: AtomicU64,
    /// Outstanding pins. A frame with `pin > 0` is never evicted.
    pin: AtomicUsize,
    /// Set when the resident page has unflushed modifications. Changed only
    /// while `page` is write-locked.
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
    /// Frame indices holding no page, available without eviction.
    free: Vec<usize>,
    /// The clock hand.
    hand: usize,
}

/// The outcome of one clock sweep.
struct Sweep {
    /// The frame to reuse, if one was found.
    victim: Option<usize>,
    /// The highest LSN among frames passed over only because the log is not
    /// yet durable that far.
    blocked: Option<Lsn>,
}

/// A bounded cache of pages over a [`PageStore`].
///
/// `BufferPool<S>` is generic over its backing store; the default is
/// [`PageFile`], so `BufferPool` without a type parameter is a pool over a file
/// of pages. The handle is `Send + Sync` and every method takes `&self`, so it
/// is shared across threads behind an `Arc` with no outer lock.
///
/// Dropping a pool discards its dirty frames without writing them. Call
/// [`checkpoint`](BufferPool::checkpoint) first to keep them.
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
/// // Fetch it back, served from cache if resident, else read from the file.
/// let guard = pool.fetch(PageId::new(0))?;
/// assert_eq!(&guard.read().payload()[..5], b"hello");
/// # Ok::<(), page_db::PageError>(())
/// ```
pub struct BufferPool<S = PageFile> {
    store: S,
    frames: Vec<Arc<FrameInner>>,
    core: Mutex<Core>,
    capacity: usize,
    watermark: Option<Box<dyn LogWatermark>>,
    sync_failed: AtomicBool,
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
    /// least one. The frame buffers are allocated once, here; the pool does no
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
            watermark: None,
            sync_failed: AtomicBool::new(false),
        }
    }

    /// Enforce the write-ahead rule against `watermark`.
    ///
    /// From then on the pool never writes a dirty page whose LSN is beyond
    /// [`LogWatermark::durable_lsn`]:
    ///
    /// - eviction passes over such pages and picks another victim; only when
    ///   every unpinned frame is held back this way does it call
    ///   [`LogWatermark::force`] and sweep again, failing with
    ///   [`PageError::BufferPoolExhausted`] if the log still is not durable far
    ///   enough;
    /// - [`flush`](BufferPool::flush), [`flush_all`](BufferPool::flush_all), and
    ///   [`checkpoint`](BufferPool::checkpoint) call `force` for such a page and
    ///   write it if the log is then durable through its LSN, or leave it dirty
    ///   and unwritten if not.
    ///
    /// Without a watermark (the default) the pool writes any dirty page it
    /// flushes or evicts, whatever its LSN, as 1.0 did.
    #[must_use]
    pub fn with_log_watermark<W: LogWatermark + 'static>(mut self, watermark: W) -> Self {
        self.watermark = Some(Box::new(watermark));
        self
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
    /// - [`PageError::BufferPoolExhausted`] if every frame is pinned (or, with a
    ///   [`LogWatermark`], every unpinned frame holds a page the log is not yet
    ///   durable for).
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
        let frame = &self.frames[slot];
        let mut page = sync::write(&frame.page);
        if let Err(err) = self.store.read_into(id, &mut page) {
            frame.id.store(NO_PAGE, Ordering::Release);
            frame.dirty.store(false, Ordering::Release);
            drop(page);
            core.free.push(slot);
            return Err(err);
        }
        let guard = self.install(&mut core, slot, id, false);
        drop(page);
        Ok(guard)
    }

    /// Introduce a fresh, zeroed page at `id`, pinning it and returning a guard.
    ///
    /// The page is created in memory and marked dirty, so it is written to the
    /// store on the next flush; no read is performed. If `id` is already
    /// resident it is reset to a blank page, which another holder of a guard on
    /// it sees on its next read. The caller chooses the id; a
    /// [`PageAllocator`](crate::PageAllocator) can pick it.
    ///
    /// # Errors
    ///
    /// - [`PageError::BufferPoolExhausted`] if every frame is pinned.
    /// - [`PageError::Io`] if flushing an evicted dirty victim fails.
    pub fn new_page(&self, id: PageId) -> PageResult<PageGuard> {
        let mut core = sync::lock(&self.core);

        if let Some(&slot) = core.map.get(&id) {
            let frame = self.frames[slot].clone();
            let _ = frame.pin.fetch_add(1, Ordering::AcqRel);
            frame.referenced.store(true, Ordering::Release);
            // The frame is pinned now, so it stays put; reset it after releasing
            // the pool mutex, since another thread may hold its page lock.
            drop(core);
            {
                let mut page = sync::write(&frame.page);
                page.reset();
                frame.dirty.store(true, Ordering::Release);
            }
            return Ok(PageGuard { frame });
        }

        let slot = self.take_slot(&mut core)?;
        let frame = &self.frames[slot];
        let mut page = sync::write(&frame.page);
        page.reset();
        let guard = self.install(&mut core, slot, id, true);
        drop(page);
        Ok(guard)
    }

    /// Drop page `id` from the pool without writing it, returning whether it
    /// was resident.
    ///
    /// Use this when a page is freed: a [`PageAllocator`](crate::PageAllocator)
    /// writes its free-list link into the freed page directly through the
    /// store, so a dirty frame left in the pool for that page would overwrite
    /// the link at the next flush or eviction. Discard the page before the
    /// allocator's next [`sync`](crate::PageAllocator::sync). Any unflushed
    /// changes to it are lost, which is the point.
    ///
    /// # Errors
    ///
    /// [`PageError::PagePinned`] if a [`PageGuard`] for the page is alive; the
    /// page stays resident.
    pub fn discard(&self, id: PageId) -> PageResult<bool> {
        let mut core = sync::lock(&self.core);
        let Some(&slot) = core.map.get(&id) else {
            return Ok(false);
        };
        let frame = &self.frames[slot];
        if frame.pin.load(Ordering::Acquire) > 0 {
            return Err(PageError::PagePinned { page_id: id.get() });
        }
        {
            // Unpinned, so only a flush can hold the lock; wait it out.
            let _page = sync::write(&frame.page);
            frame.dirty.store(false, Ordering::Release);
            frame.referenced.store(false, Ordering::Release);
            frame.id.store(NO_PAGE, Ordering::Release);
        }
        let _ = core.map.remove(&id);
        core.free.push(slot);
        Ok(true)
    }

    /// Flush page `id` to the store if it is resident and dirty.
    ///
    /// This places the bytes in the store; call [`sync`](BufferPool::sync) to
    /// make them durable. With a [`LogWatermark`], a page whose LSN the log is
    /// not durable for (after [`LogWatermark::force`]) is left dirty and not
    /// written. Do not call this while holding a [`PageRef`] or [`PageMut`] to
    /// the same page on the same thread: flushing takes the page's lock.
    ///
    /// # Errors
    ///
    /// Whatever the store's write, or forcing the log, returns.
    pub fn flush(&self, id: PageId) -> PageResult<()> {
        let frame = {
            let core = sync::lock(&self.core);
            match core.map.get(&id) {
                Some(&slot) => self.frames[slot].clone(),
                None => return Ok(()),
            }
        };
        let mut durable = None;
        self.flush_frame(&frame, id, &mut durable)
    }

    /// Flush every dirty resident page to the store, in page-id order.
    ///
    /// The pages to flush are chosen under the pool's mutex, which is then
    /// released before any page is locked or written, so other threads keep
    /// fetching while the flush runs. With a [`LogWatermark`], pages the log is
    /// not durable for are left dirty and not written (see
    /// [`flush`](BufferPool::flush)). Do not call this while holding a
    /// [`PageRef`] or [`PageMut`] on the same thread.
    ///
    /// # Errors
    ///
    /// Whatever the store's write, or forcing the log, returns. On error, some
    /// pages may already have been flushed; a page whose write failed stays
    /// dirty, so retrying the flush writes it again.
    pub fn flush_all(&self) -> PageResult<()> {
        let mut targets: Vec<(PageId, Arc<FrameInner>)> = {
            let core = sync::lock(&self.core);
            core.map
                .iter()
                .filter(|&(_, &slot)| self.frames[slot].dirty.load(Ordering::Acquire))
                .map(|(&id, &slot)| (id, self.frames[slot].clone()))
                .collect()
        };
        targets.sort_unstable_by_key(|&(id, _)| id);
        let mut durable = None;
        for (id, frame) in &targets {
            self.flush_frame(frame, *id, &mut durable)?;
        }
        Ok(())
    }

    /// Flush all dirty pages, then make the store durable.
    ///
    /// Equivalent to [`flush_all`](BufferPool::flush_all) followed by
    /// [`sync`](BufferPool::sync), the common checkpoint sequence. If the sync
    /// fails, do not retry the checkpoint on this pool: see
    /// [`sync`](BufferPool::sync).
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
    /// A failed sync is sticky. The pages it covered were already marked clean
    /// when they were written, and the OS may have dropped the ones it could not
    /// persist, so a retry could succeed without them. After the first failure
    /// every later `sync` (and so every `checkpoint`) on this pool returns an
    /// error; reopen the store and recover from the log.
    ///
    /// # Errors
    ///
    /// Whatever the store's sync returns, or [`PageError::Io`] if an earlier
    /// sync on this pool failed.
    pub fn sync(&self) -> PageResult<()> {
        if self.sync_failed.load(Ordering::Acquire) {
            return Err(sync_poisoned());
        }
        if let Err(err) = self.store.sync() {
            self.sync_failed.store(true, Ordering::Release);
            return Err(err);
        }
        Ok(())
    }

    /// Install a freshly loaded or created page into `slot`, returning a pinned
    /// guard. Caller holds `core` and the frame's page write lock, and has
    /// already populated the frame's buffer.
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

    /// Flush `frame` if it still holds page `id` and is dirty. Called without
    /// the pool mutex. `durable` caches the log watermark across one flush call.
    fn flush_frame(
        &self,
        frame: &FrameInner,
        id: PageId,
        durable: &mut Option<Lsn>,
    ) -> PageResult<()> {
        let mut page = sync::write(&frame.page);
        // The id and the dirty bit change only under this lock, so this check
        // and the write below see one consistent state of the frame.
        if frame.id.load(Ordering::Acquire) != id.get() || !frame.dirty.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(watermark) = &self.watermark {
            let lsn = page.lsn();
            let mut point = match *durable {
                Some(point) => point,
                None => watermark.durable_lsn(),
            };
            if lsn > point {
                point = watermark.force(lsn)?;
            }
            *durable = Some(point);
            if lsn > point {
                return Ok(());
            }
        }
        self.store.write_page(id, &mut page)?;
        frame.dirty.store(false, Ordering::Release);
        Ok(())
    }

    /// Obtain a frame slot to fill: a free one, or an evicted victim (flushed
    /// first if dirty). Caller holds `core`.
    fn take_slot(&self, core: &mut Core) -> PageResult<usize> {
        if let Some(slot) = core.free.pop() {
            return Ok(slot);
        }
        let exhausted = PageError::BufferPoolExhausted {
            capacity: self.capacity,
        };
        let durable = self.watermark.as_ref().map(|w| w.durable_lsn());
        let sweep = self.find_victim(core, durable);
        let slot = match (sweep.victim, sweep.blocked, &self.watermark) {
            (Some(slot), _, _) => slot,
            (None, Some(lsn), Some(watermark)) => {
                // Every evictable frame waits on the log: force it and retry.
                let point = watermark.force(lsn)?;
                match self.find_victim(core, Some(point)).victim {
                    Some(slot) => slot,
                    None => return Err(exhausted),
                }
            }
            _ => return Err(exhausted),
        };

        let frame = &self.frames[slot];
        let victim_id = frame.resident_id();
        {
            // The victim is unpinned, so only a flush can hold its lock, and a
            // flush never waits on the pool mutex: this cannot deadlock.
            let mut page = sync::write(&frame.page);
            if frame.dirty.load(Ordering::Acquire) {
                self.store.write_page(victim_id, &mut page)?;
                frame.dirty.store(false, Ordering::Release);
            }
        }
        let _ = core.map.remove(&victim_id);
        Ok(slot)
    }

    /// The clock sweep: find an unpinned frame to reuse. With a durable point,
    /// a dirty frame whose LSN is beyond it is passed over. Caller holds `core`.
    fn find_victim(&self, core: &mut Core, durable: Option<Lsn>) -> Sweep {
        let n = self.capacity;
        let mut blocked: Option<Lsn> = None;
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
            if let Some(point) = durable {
                if frame.dirty.load(Ordering::Acquire) {
                    let lsn = sync::read(&frame.page).lsn();
                    if lsn > point {
                        blocked = Some(blocked.map_or(lsn, |b| b.max(lsn)));
                        continue;
                    }
                }
            }
            return Sweep {
                victim: Some(slot),
                blocked,
            };
        }
        Sweep {
            victim: None,
            blocked,
        }
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
    /// The page is recorded dirty once the page lock is held and before the
    /// borrow is returned, so it will be flushed even if the actual mutation is
    /// conditional, and a flush running at the same time either completes
    /// before the write or sees the page dirty after it.
    #[inline]
    #[must_use]
    pub fn write(&self) -> PageMut<'_> {
        let guard = sync::write(&self.frame.page);
        self.frame.dirty.store(true, Ordering::Release);
        PageMut { guard }
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

    /// A write that overlaps a flush of the same page is never lost: whichever
    /// order the flush and the write take the page lock in, the write either
    /// reaches the store in that flush or leaves the page dirty for the next
    /// one, so after the guard is released and the page evicted, a fetch
    /// returns the written value.
    #[test]
    fn loom_write_during_flush_is_not_lost() {
        loom::model(|| {
            let pool = Arc::new(BufferPool::new(MemStore::new(4096), 1));
            {
                let guard = pool.new_page(PageId::new(0)).unwrap();
                guard.write().payload_mut()[0] = 1;
            }
            let guard = pool.fetch(PageId::new(0)).unwrap();

            let p = Arc::clone(&pool);
            let flusher = loom::thread::spawn(move || p.flush_all().unwrap());
            guard.write().payload_mut()[0] = 2;
            flusher.join().unwrap();
            drop(guard);

            // Evict page 0 (writing it if still dirty), then read it back.
            drop(pool.new_page(PageId::new(1)).unwrap());
            let back = pool.fetch(PageId::new(0)).unwrap();
            assert_eq!(
                back.read().payload()[0],
                2,
                "the overlapping write was lost"
            );
        });
    }

    /// A checkpoint never deadlocks against latch coupling: one thread holds a
    /// page's write lock and fetches another page while a checkpoint flushes the
    /// latched page. Loom reports a deadlock if both can block each other.
    #[test]
    fn loom_checkpoint_with_latch_coupling_does_not_deadlock() {
        loom::model(|| {
            let pool = Arc::new(BufferPool::new(MemStore::new(4096), 2));
            drop(pool.new_page(PageId::new(1)).unwrap());
            let parent = pool.new_page(PageId::new(0)).unwrap();

            let p = Arc::clone(&pool);
            let checkpointer = loom::thread::spawn(move || p.checkpoint().unwrap());
            {
                let mut latch = parent.write();
                let child = pool.fetch(PageId::new(1)).unwrap();
                latch.payload_mut()[0] = child.read().payload()[0];
            }
            checkpointer.join().unwrap();
            drop(parent);
        });
    }

    impl BufferPool<MemStore> {
        fn store_contains_loom(&self, id: u64) -> bool {
            self.store.contains(id)
        }
    }
}

//! The 1.1.0 additions that close review findings: the write-ahead-log
//! watermark on the buffer pool, `BufferPool::discard`, the allocator's
//! superblock copies and `PageAllocator::from_state`, and on-disk compatibility
//! with files written by 1.0.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use page_db::{
    BufferPool, LogWatermark, Lsn, Page, PageAllocator, PageError, PageFile, PageFileOptions,
    PageId, PageResult, PageStore,
};

fn buffered(path: &std::path::Path) -> PageFile {
    PageFileOptions::new()
        .direct_io(false)
        .open(path)
        .expect("open")
}

/// A `PageFile` that records the id of every page written to it.
struct Recording {
    inner: PageFile,
    written: Mutex<Vec<u64>>,
}

impl Recording {
    fn new(inner: PageFile) -> Self {
        Self {
            inner,
            written: Mutex::new(Vec::new()),
        }
    }

    fn wrote(&self, id: u64) -> bool {
        self.written.lock().expect("lock").contains(&id)
    }
}

impl PageStore for Recording {
    fn page_size(&self) -> usize {
        self.inner.page_size()
    }
    fn allocate_page(&self) -> Page {
        self.inner.allocate_page()
    }
    fn read_into(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        self.inner.read_into(id, page)
    }
    fn write_page(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        self.written.lock().expect("lock").push(id.get());
        self.inner.write_page(id, page)
    }
    fn sync(&self) -> PageResult<()> {
        self.inner.sync()
    }
}

/// A log whose durable end only moves when it is forced.
struct ForcedLog {
    durable: AtomicU64,
    forced: AtomicU64,
}

impl LogWatermark for ForcedLog {
    fn durable_lsn(&self) -> Lsn {
        Lsn::new(self.durable.load(Ordering::Acquire))
    }
    fn force(&self, lsn: Lsn) -> PageResult<Lsn> {
        let _ = self.forced.fetch_add(1, Ordering::AcqRel);
        let _ = self.durable.fetch_max(lsn.get(), Ordering::AcqRel);
        Ok(self.durable_lsn())
    }
}

fn dirty_page(pool: &BufferPool<Arc<Recording>>, id: u64, lsn: u64) {
    let g = pool.new_page(PageId::new(id)).expect("new");
    let mut page = g.write();
    page.set_lsn(Lsn::new(lsn));
    page.payload_mut()[0] = id as u8;
}

/// F-3: eviction passes over a dirty page whose LSN the log is not durable for.
#[test]
fn eviction_respects_the_log_watermark() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Recording::new(buffered(&dir.path().join("p"))));
    let pool = BufferPool::new(Arc::clone(&store), 2).with_log_watermark(|| Lsn::new(800));

    dirty_page(&pool, 1, 900); // beyond the durable point
    dirty_page(&pool, 2, 700); // within it
    // Admitting page 3 must evict page 2, never page 1.
    drop(pool.new_page(PageId::new(3)).expect("new"));
    assert!(pool.is_resident(PageId::new(1)));
    assert!(!pool.is_resident(PageId::new(2)));
    assert!(store.wrote(2));
    assert!(
        !store.wrote(1),
        "page 1 reached the store before its log record"
    );

    // With page 3 pinned and page 1 held back, nothing can be evicted.
    let _pin = pool.fetch(PageId::new(3)).expect("fetch");
    assert!(matches!(
        pool.new_page(PageId::new(4)),
        Err(PageError::BufferPoolExhausted { .. })
    ));
    assert!(!store.wrote(1));
}

/// F-3: a watermark that can force the log is forced before the page is
/// written, by eviction and by flush.
#[test]
fn eviction_and_flush_force_the_log_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Recording::new(buffered(&dir.path().join("p"))));
    let log = Arc::new(ForcedLog {
        durable: AtomicU64::new(0),
        forced: AtomicU64::new(0),
    });
    struct Shared(Arc<ForcedLog>);
    impl LogWatermark for Shared {
        fn durable_lsn(&self) -> Lsn {
            self.0.durable_lsn()
        }
        fn force(&self, lsn: Lsn) -> PageResult<Lsn> {
            self.0.force(lsn)
        }
    }
    let pool = BufferPool::new(Arc::clone(&store), 1).with_log_watermark(Shared(Arc::clone(&log)));

    dirty_page(&pool, 1, 50);
    drop(pool.new_page(PageId::new(2)).expect("new")); // must force to 50, then evict
    assert_eq!(log.durable.load(Ordering::Acquire), 50);
    assert!(store.wrote(1));

    {
        let g = pool.fetch(PageId::new(2)).expect("fetch");
        g.write().set_lsn(Lsn::new(80));
    }
    pool.flush(PageId::new(2)).expect("flush");
    assert_eq!(log.durable.load(Ordering::Acquire), 80);
    assert!(store.wrote(2));
    assert!(log.forced.load(Ordering::Acquire) >= 2);
}

/// F-3: a checkpoint with a watermark that cannot force leaves the page dirty
/// and unwritten, then writes it once the log catches up.
#[test]
fn checkpoint_skips_pages_beyond_the_durable_point() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Recording::new(buffered(&dir.path().join("p"))));
    let durable = Arc::new(AtomicU64::new(10));
    let seen = Arc::clone(&durable);
    let pool = BufferPool::new(Arc::clone(&store), 8)
        .with_log_watermark(move || Lsn::new(seen.load(Ordering::Acquire)));

    dirty_page(&pool, 1, 5);
    dirty_page(&pool, 2, 20);
    pool.checkpoint().expect("checkpoint");
    assert!(store.wrote(1));
    assert!(!store.wrote(2));
    assert!(pool.fetch(PageId::new(2)).expect("fetch").is_dirty());

    durable.store(20, Ordering::Release);
    pool.checkpoint().expect("checkpoint");
    assert!(store.wrote(2));
    assert!(!pool.fetch(PageId::new(2)).expect("fetch").is_dirty());
}

/// Without a watermark the pool writes whatever it evicts, as 1.0 did.
#[test]
fn no_watermark_keeps_the_old_behaviour() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Recording::new(buffered(&dir.path().join("p"))));
    let pool = BufferPool::new(Arc::clone(&store), 1);
    dirty_page(&pool, 1, u64::MAX);
    drop(pool.new_page(PageId::new(2)).expect("new"));
    assert!(store.wrote(1));
}

/// F-6: a pool flush that overwrites a free-list link must not brick the
/// allocator, and the overwritten page must not come back as free.
#[test]
fn overwritten_free_list_link_does_not_brick_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a");
    let id;
    {
        let store = Arc::new(buffered(&path));
        let alloc = PageAllocator::new(Arc::clone(&store)).expect("alloc");
        let pool = BufferPool::new(Arc::clone(&store), 16);
        id = alloc.allocate().expect("allocate");
        {
            let g = pool.new_page(id).expect("new");
            g.write().payload_mut()[..8].copy_from_slice(&7u64.to_le_bytes());
        }
        alloc.free(id).expect("free");
        alloc.sync().expect("sync"); // link written into page `id`
        pool.checkpoint().expect("checkpoint"); // the stale frame lands on it
    }
    let alloc = PageAllocator::new(buffered(&path)).expect("reopen");
    assert!(alloc.free_list_dropped());
    assert_eq!(alloc.free_count(), 0);
    assert_ne!(alloc.allocate().expect("allocate"), id);
}

/// F-6: discarding the page from the pool before freeing it keeps the link.
#[test]
fn discard_before_free_keeps_the_free_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a");
    let id;
    {
        let store = Arc::new(buffered(&path));
        let alloc = PageAllocator::new(Arc::clone(&store)).expect("alloc");
        let pool = BufferPool::new(Arc::clone(&store), 16);
        id = alloc.allocate().expect("allocate");
        {
            let g = pool.new_page(id).expect("new");
            g.write().payload_mut()[..8].copy_from_slice(&7u64.to_le_bytes());
            // A pinned page cannot be discarded.
            assert!(matches!(
                pool.discard(id),
                Err(PageError::PagePinned { page_id }) if page_id == id.get()
            ));
        }
        assert!(pool.discard(id).expect("discard"));
        assert!(!pool.is_resident(id));
        assert!(!pool.discard(id).expect("discard again"));
        alloc.free(id).expect("free");
        alloc.sync().expect("sync");
        pool.checkpoint().expect("checkpoint");
    }
    let alloc = PageAllocator::new(buffered(&path)).expect("reopen");
    assert!(!alloc.free_list_dropped());
    assert_eq!(alloc.free_count(), 1);
    assert_eq!(alloc.allocate().expect("allocate"), id);
}

/// F-5: `from_state` rebuilds an allocator over a store whose superblock is
/// gone, and validates what it is given.
#[test]
fn from_state_rebuilds_the_allocator() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a");
    {
        let alloc = PageAllocator::new(buffered(&path)).expect("alloc");
        for _ in 0..9 {
            let _ = alloc.allocate().expect("allocate");
        }
        alloc.sync().expect("sync");
    }
    // Wreck page 0 entirely.
    let mut bytes = std::fs::read(&path).expect("read");
    bytes[..4096].fill(0xEE);
    std::fs::write(&path, &bytes).expect("write");
    assert!(PageAllocator::new(buffered(&path)).is_err());

    for (next, ids) in [
        (0u64, vec![]),
        (10, vec![0]),
        (10, vec![10]),
        (10, vec![3, 3]),
    ] {
        let ids = ids.into_iter().map(PageId::new);
        assert!(matches!(
            PageAllocator::from_state(buffered(&path), next, ids),
            Err(PageError::InvalidPageId { .. })
        ));
    }

    {
        let ids = [3u64, 5].map(PageId::new);
        let alloc = PageAllocator::from_state(buffered(&path), 10, ids).expect("rebuild");
        assert_eq!(alloc.high_water(), 10);
        assert_eq!(alloc.free_count(), 2);
        alloc.sync().expect("sync");
    }
    let alloc = PageAllocator::new(buffered(&path)).expect("reopen");
    assert_eq!(alloc.high_water(), 10);
    assert_eq!(alloc.allocate().expect("allocate"), PageId::new(5));
    assert_eq!(alloc.allocate().expect("allocate"), PageId::new(3));
    assert_eq!(alloc.allocate().expect("allocate"), PageId::new(10));
}

// The 1.0 superblock: payload offsets of its fields.
const SB_MAGIC: u32 = u32::from_le_bytes(*b"PGSB");
const NO_PAGE: u64 = u64::MAX;

fn le_u64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

/// Write a superblock exactly as 1.0 does: the 1.0 fields, the rest of the
/// payload zero, and the free-list chain in the freed pages.
fn write_v1_0_file(file: &PageFile, next_new: u64, free: &[u64]) {
    for (i, &id) in free.iter().enumerate() {
        let mut page = file.allocate_page();
        let next = free.get(i + 1).copied().unwrap_or(NO_PAGE);
        page.payload_mut()[..8].copy_from_slice(&next.to_le_bytes());
        file.write_page(PageId::new(id), &mut page).expect("link");
    }
    let mut page = file.allocate_page();
    let p = page.payload_mut();
    p[0..4].copy_from_slice(&SB_MAGIC.to_le_bytes());
    p[4..6].copy_from_slice(&1u16.to_le_bytes());
    let head = free.first().copied().unwrap_or(NO_PAGE);
    p[8..16].copy_from_slice(&head.to_le_bytes());
    p[16..24].copy_from_slice(&next_new.to_le_bytes());
    p[24..32].copy_from_slice(&(free.len() as u64).to_le_bytes());
    file.write_page(PageId::new(0), &mut page)
        .expect("superblock");
    file.sync().expect("sync");
}

/// Read a superblock the way 1.0 does: verify page 0 as a whole, read the 1.0
/// fields, walk the chain. Returns (next_new, free ids in chain order).
fn read_as_v1_0(file: &PageFile) -> (u64, Vec<u64>) {
    let page = file.read_page(PageId::new(0)).expect("page 0 verifies");
    let p = page.payload();
    assert_eq!(&p[0..4], &SB_MAGIC.to_le_bytes());
    assert_eq!(&p[4..6], &1u16.to_le_bytes());
    let mut cur = le_u64(p, 8);
    let next_new = le_u64(p, 16);
    let count = le_u64(p, 24);
    let mut ids = Vec::new();
    while cur != NO_PAGE {
        assert!(cur >= 1 && cur < next_new);
        ids.push(cur);
        cur = le_u64(file.read_page(PageId::new(cur)).expect("link").payload(), 0);
    }
    assert_eq!(ids.len() as u64, count);
    (next_new, ids)
}

/// A file written by 1.0 opens, and once 1.1 has synced it a 1.0 reader still
/// sees the current state.
#[test]
fn v1_0_files_open_and_stay_readable_by_v1_0() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a");
    write_v1_0_file(&buffered(&path), 8, &[2, 6, 4]);

    {
        let alloc = PageAllocator::new(buffered(&path)).expect("open a 1.0 file");
        assert!(!alloc.free_list_dropped());
        assert_eq!(alloc.high_water(), 8);
        assert_eq!(alloc.free_count(), 3);
        assert_eq!(alloc.allocate().expect("allocate"), PageId::new(4));
        alloc.free(PageId::new(7)).expect("free");
        alloc.free(PageId::new(4)).expect("free");
        let _ = alloc.allocate().expect("allocate"); // 4 again
        let _ = alloc.allocate().expect("allocate"); // 7
        let _ = alloc.allocate().expect("allocate"); // 6
        alloc.free(PageId::new(1)).expect("free");
        alloc.sync().expect("sync");
    }
    assert_eq!(read_as_v1_0(&buffered(&path)), (8, vec![2, 1]));

    let alloc = PageAllocator::new(buffered(&path)).expect("reopen");
    assert_eq!(alloc.free_count(), 2);
    assert_eq!(alloc.allocate().expect("allocate"), PageId::new(1));
    assert_eq!(alloc.allocate().expect("allocate"), PageId::new(2));
    assert_eq!(alloc.allocate().expect("allocate"), PageId::new(8));
}

/// When 1.0 rewrites page 0 it zeroes the superblock copies; 1.1 then opens
/// from the 1.0 fields again rather than from a stale copy.
#[test]
fn a_v1_0_rewrite_after_v1_1_is_honoured() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a");
    {
        let alloc = PageAllocator::new(buffered(&path)).expect("alloc");
        for _ in 0..4 {
            let _ = alloc.allocate().expect("allocate");
        }
        alloc.free(PageId::new(2)).expect("free");
        alloc.sync().expect("sync");
    }
    // 1.0 opens it, allocates 2 and 5, frees 3, and syncs.
    write_v1_0_file(&buffered(&path), 6, &[3]);

    let alloc = PageAllocator::new(buffered(&path)).expect("reopen");
    assert_eq!(alloc.high_water(), 6);
    assert_eq!(alloc.free_count(), 1);
    assert_eq!(alloc.allocate().expect("allocate"), PageId::new(3));
}

/// A crafted high-water mark whose pages cannot be addressed is refused at
/// open instead of reaching an overflowing offset.
#[test]
fn unaddressable_high_water_mark_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a");
    write_v1_0_file(&buffered(&path), 1u64 << 52, &[]);
    assert!(matches!(
        PageAllocator::new(buffered(&path)),
        Err(PageError::InvalidSuperblock)
    ));

    write_v1_0_file(&buffered(&path), (1u64 << 52) - 1, &[]);
    let alloc = PageAllocator::new(buffered(&path)).expect("the last addressable mark opens");
    assert!(matches!(
        alloc.allocate(),
        Err(PageError::InvalidPageId { .. })
    ));
}

//! Regression tests for the 1.1.0 review findings. Each test drives the public
//! API through the failure the review reproduced and asserts the corrected
//! behaviour, so it fails against 1.0.0 and passes from 1.1.0 on.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::time::Duration;

use page_db::{
    BufferPool, Page, PageAllocator, PageError, PageFile, PageFileOptions, PageId, PageResult,
    PageStore,
};

fn buffered(path: &std::path::Path) -> PageFile {
    PageFileOptions::new()
        .direct_io(false)
        .open(path)
        .expect("open")
}

/// A real `PageFile` behind hooks: one write can be held inside `write_page`,
/// and syncs can be made to fail.
struct Hooked {
    inner: PageFile,
    slow_write: AtomicBool,
    in_write: Arc<Barrier>,
    fail_syncs: Mutex<u32>,
    writes: AtomicU32,
}

impl Hooked {
    fn new(inner: PageFile) -> Self {
        Self {
            inner,
            slow_write: AtomicBool::new(false),
            in_write: Arc::new(Barrier::new(2)),
            fail_syncs: Mutex::new(0),
            writes: AtomicU32::new(0),
        }
    }
}

impl PageStore for Hooked {
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
        let _ = self.writes.fetch_add(1, Ordering::SeqCst);
        if self.slow_write.swap(false, Ordering::SeqCst) {
            // Tell the writer thread the flush is inside `write_page`, then give
            // it time to reach `PageGuard::write`.
            let _ = self.in_write.wait();
            std::thread::sleep(Duration::from_millis(300));
        }
        self.inner.write_page(id, page)
    }
    fn sync(&self) -> PageResult<()> {
        {
            let mut left = self.fail_syncs.lock().expect("lock");
            if *left > 0 {
                *left -= 1;
                return Err(PageError::Io(std::io::Error::other("injected EIO")));
            }
        }
        self.inner.sync()
    }
}

/// F-1: a write that waits behind a concurrent flush must stay dirty and must
/// survive eviction.
#[test]
fn write_during_flush_is_not_lost() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Hooked::new(buffered(&dir.path().join("p"))));
    let pool = Arc::new(BufferPool::new(Arc::clone(&store), 1));
    let id = PageId::new(0);
    {
        let g = pool.new_page(id).expect("new");
        g.write().payload_mut()[0] = 1;
    }
    let guard = pool.fetch(id).expect("fetch");
    guard.write().payload_mut()[0] = 2;

    store.slow_write.store(true, Ordering::SeqCst);
    let barrier = Arc::clone(&store.in_write);
    let p = Arc::clone(&pool);
    let flusher = std::thread::spawn(move || p.flush_all());
    let _ = barrier.wait(); // the flush is inside write_page now
    {
        let mut m = guard.write();
        m.payload_mut()[0] = 3;
    }
    flusher.join().expect("join").expect("flush");
    assert!(guard.is_dirty(), "the write of 3 must be marked dirty");
    drop(guard);

    // Evict page 0 by admitting page 1 into the single frame.
    drop(pool.new_page(PageId::new(1)).expect("new"));
    let back = pool.fetch(id).expect("fetch");
    assert_eq!(back.read().payload()[0], 3, "the write of 3 was lost");
}

/// F-2: a checkpoint must not deadlock against a thread that holds a page
/// latch and fetches another page (latch coupling).
#[test]
fn checkpoint_does_not_deadlock_with_latch_coupling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = Arc::new(BufferPool::new(buffered(&dir.path().join("p")), 8));
    drop(pool.new_page(PageId::new(1)).expect("new"));
    pool.checkpoint().expect("checkpoint");

    let (tx, rx) = mpsc::channel();
    let (held_tx, held_rx) = mpsc::channel();
    let p = Arc::clone(&pool);
    let _latcher = std::thread::spawn(move || {
        let parent = p.new_page(PageId::new(0)).expect("new");
        let _latch = parent.write();
        held_tx.send(()).expect("send");
        std::thread::sleep(Duration::from_millis(300)); // checkpoint starts
        let child = p.fetch(PageId::new(1));
        let _ = tx.send(child.is_ok());
    });
    held_rx.recv().expect("recv");
    let p = Arc::clone(&pool);
    let (done_tx, done_rx) = mpsc::channel();
    let _checkpointer = std::thread::spawn(move || {
        let _ = done_tx.send(p.checkpoint().is_ok());
    });
    let got = rx.recv_timeout(Duration::from_secs(10));
    assert_eq!(got, Ok(true), "the latch holder's fetch never returned");
    let done = done_rx.recv_timeout(Duration::from_secs(10));
    assert_eq!(done, Ok(true), "the checkpoint never returned");
}

/// F-2: the same for `flush` of a single page and for `new_page` on a resident
/// page that another thread holds latched.
#[test]
fn flush_and_new_page_do_not_block_the_pool() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = Arc::new(BufferPool::new(buffered(&dir.path().join("p")), 8));
    drop(pool.new_page(PageId::new(1)).expect("new"));

    let (tx, rx) = mpsc::channel();
    let (held_tx, held_rx) = mpsc::channel();
    let p = Arc::clone(&pool);
    let _latcher = std::thread::spawn(move || {
        let parent = p.new_page(PageId::new(0)).expect("new");
        let _latch = parent.write();
        held_tx.send(()).expect("send");
        std::thread::sleep(Duration::from_millis(300));
        let _ = tx.send(p.fetch(PageId::new(1)).is_ok());
    });
    held_rx.recv().expect("recv");
    let p1 = Arc::clone(&pool);
    let _f = std::thread::spawn(move || {
        let _ = p1.flush(PageId::new(0));
    });
    let p2 = Arc::clone(&pool);
    let _n = std::thread::spawn(move || {
        let _ = p2.new_page(PageId::new(0));
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)),
        Ok(true),
        "the latch holder's fetch never returned"
    );
}

/// F-4: a double free is refused, an id is never handed to two owners, and
/// the file reopens after sync.
#[test]
fn double_free_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a");
    let a;
    {
        let alloc = PageAllocator::new(buffered(&path)).expect("alloc");
        a = alloc.allocate().expect("allocate");
        let _b = alloc.allocate().expect("allocate");
        alloc.free(a).expect("first free");
        assert!(matches!(
            alloc.free(a),
            Err(PageError::InvalidPageId { page_id }) if page_id == a.get()
        ));
        assert_eq!(alloc.free_count(), 1);
        let x = alloc.allocate().expect("allocate");
        let y = alloc.allocate().expect("allocate");
        assert_eq!(x, a);
        assert_ne!(x, y, "the same id went to two owners");
        alloc.free(x).expect("free");
        assert!(alloc.free(x).is_err());
        alloc.sync().expect("sync");
    }
    let alloc = PageAllocator::new(buffered(&path)).expect("reopen");
    assert_eq!(alloc.free_count(), 1);
    assert_eq!(alloc.allocate().expect("allocate"), a);
}

/// F-5: one torn superblock page no longer makes the allocator unopenable.
/// The second half of page 0 is damaged here (as when only the first sectors
/// of the write reached the disk).
#[test]
fn torn_superblock_second_half_still_opens() {
    torn_superblock_opens(2048);
}

/// F-5: the same with the first half of page 0 damaged.
#[test]
fn torn_superblock_first_half_still_opens() {
    torn_superblock_opens(70);
}

fn torn_superblock_opens(flip: usize) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a");
    {
        let alloc = PageAllocator::new(buffered(&path)).expect("alloc");
        for _ in 0..5 {
            let _ = alloc.allocate().expect("allocate");
        }
        alloc.free(PageId::new(2)).expect("free");
        alloc.free(PageId::new(4)).expect("free");
        alloc.sync().expect("sync");
    }
    let mut bytes = std::fs::read(&path).expect("read");
    bytes[flip] ^= 0x01;
    std::fs::write(&path, &bytes).expect("write");

    let alloc = PageAllocator::new(buffered(&path)).expect("a torn page 0 must still open");
    assert_eq!(alloc.high_water(), 6);
    assert_eq!(alloc.free_count(), 2);
    assert_eq!(alloc.allocate().expect("allocate"), PageId::new(4));
    assert_eq!(alloc.allocate().expect("allocate"), PageId::new(2));
    assert_eq!(alloc.allocate().expect("allocate"), PageId::new(6));
}

/// F-7: a page id whose byte offset overflows is a typed error, never a panic
/// and never a write over another page.
#[test]
fn huge_page_id_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("p");
    let file = buffered(&path);
    let mut p0 = file.allocate_page();
    p0.payload_mut()[0] = 0xAA;
    file.write_page(PageId::new(0), &mut p0).expect("write");

    for raw in [1u64 << 52, (1u64 << 52) - 1, u64::MAX - 1, u64::MAX] {
        let wrap = PageId::new(raw);
        let mut p = file.allocate_page();
        assert!(matches!(
            file.write_page(wrap, &mut p),
            Err(PageError::InvalidPageId { page_id }) if page_id == raw
        ));
        assert!(matches!(
            file.read_page(wrap),
            Err(PageError::InvalidPageId { page_id }) if page_id == raw
        ));
    }
    assert_eq!(
        file.read_page(PageId::new(0)).expect("read").payload()[0],
        0xAA
    );
}

/// F-9: after a failed sync, a retried checkpoint on the same handle must fail
/// rather than report success with nothing rewritten.
#[test]
fn failed_sync_is_sticky() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Hooked::new(buffered(&dir.path().join("p"))));
    let pool = BufferPool::new(Arc::clone(&store), 8);
    for i in 0..4 {
        let g = pool.new_page(PageId::new(i)).expect("new");
        g.write().payload_mut()[0] = i as u8;
    }
    *store.fail_syncs.lock().expect("lock") = 1;
    assert!(pool.checkpoint().is_err());
    assert!(pool.checkpoint().is_err(), "a retried checkpoint must fail");
    assert!(pool.sync().is_err(), "a retried sync must fail");
}

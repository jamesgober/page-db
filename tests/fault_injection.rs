//! Injected I/O failures must propagate as `PageError::Io`, never a panic and
//! never a silently swallowed error. A store that fails on demand sits behind the
//! buffer pool and the allocator through the public `PageStore` seam.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use page_db::{
    BufferPool, Page, PageAllocator, PageError, PageId, PageResult, PageSize, PageStore,
};

const PS: usize = 4096;

/// An in-memory store whose reads or writes can be made to fail.
struct FailingStore {
    pages: Mutex<HashMap<u64, Vec<u8>>>,
    fail_reads: AtomicBool,
    fail_writes: AtomicBool,
}

impl FailingStore {
    fn new() -> Self {
        Self {
            pages: Mutex::new(HashMap::new()),
            fail_reads: AtomicBool::new(false),
            fail_writes: AtomicBool::new(false),
        }
    }

    fn fail_reads(&self, on: bool) {
        self.fail_reads.store(on, Ordering::SeqCst);
    }

    fn fail_writes(&self, on: bool) {
        self.fail_writes.store(on, Ordering::SeqCst);
    }
}

fn injected() -> PageError {
    PageError::Io(std::io::Error::other("injected failure"))
}

impl PageStore for FailingStore {
    fn page_size(&self) -> usize {
        PS
    }

    fn allocate_page(&self) -> Page {
        Page::new(PageSize::new(PS).expect("valid"))
    }

    fn read_into(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(injected());
        }
        let pages = self.pages.lock().expect("lock");
        match pages.get(&id.get()) {
            Some(bytes) => {
                let dst = page.payload_mut();
                let header = PS - dst.len();
                dst.copy_from_slice(&bytes[header..]);
                // Re-verify via a fresh from_bytes round-trip is overkill here;
                // the bytes were produced by a real write_page, so they verify.
                Ok(())
            }
            None => Err(PageError::ShortRead {
                page_id: id.get(),
                got: 0,
                page_size: PS,
            }),
        }
    }

    fn write_page(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(injected());
        }
        // The pool tracks ids itself and `read_into` restores only the payload,
        // so a faithful header stamp is unnecessary here — store the bytes as-is.
        let mut pages = self.pages.lock().expect("lock");
        let _ = pages.insert(id.get(), page.to_checksummed_bytes());
        Ok(())
    }

    fn sync(&self) -> PageResult<()> {
        Ok(())
    }
}

#[test]
fn pool_fetch_propagates_read_error() {
    use std::sync::Arc;

    let store = Arc::new(FailingStore::new());
    let pool = BufferPool::new(Arc::clone(&store), 1); // one frame

    {
        let guard = pool.new_page(PageId::new(0)).expect("new_page");
        guard.write().payload_mut()[0] = 1;
    }
    // Evicting page 0 to admit a miss now triggers a read that fails.
    store.fail_reads(true);
    let result = pool.fetch(PageId::new(99));
    assert!(matches!(result, Err(PageError::Io(_))));

    // The pool stayed usable: turn faults off and the original page is back.
    store.fail_reads(false);
    let guard = pool.fetch(PageId::new(0)).expect("recovered");
    assert_eq!(guard.read().payload()[0], 1);
}

#[test]
fn pool_flush_write_error_surfaces() {
    use std::sync::Arc;

    let store = Arc::new(FailingStore::new());
    let pool = BufferPool::new(Arc::clone(&store), 8);

    {
        let guard = pool.new_page(PageId::new(1)).expect("new_page");
        guard.write().payload_mut()[0] = 0x42; // dirty
    }
    store.fail_writes(true);
    assert!(matches!(pool.flush_all(), Err(PageError::Io(_))));
}

#[test]
fn allocator_sync_propagates_write_error() {
    use std::sync::Arc;

    let store = Arc::new(FailingStore::new());
    let alloc = PageAllocator::new(Arc::clone(&store)).expect("new");

    let _ = alloc.allocate().expect("allocate");
    store.fail_writes(true);
    // sync persists the superblock; the failed write must propagate.
    assert!(matches!(alloc.sync(), Err(PageError::Io(_))));
}

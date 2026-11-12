//! An in-memory [`PageStore`](crate::store::PageStore) for tests and loom
//! models, shared by the buffer-pool and allocator test suites.
//!
//! It records the checksummed bytes of every written page, so a test can assert
//! that a page really was written, and uses the crate's `sync` shim so it works
//! under loom as well as under the normal test runner.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use crate::error::{PageError, PageResult};
use crate::page::{Page, PageId, PageSize};
use crate::store::PageStore;
use crate::sync::{self, Mutex};

/// A `HashMap`-backed [`PageStore`](crate::store::PageStore).
pub(crate) struct MemStore {
    page_size: usize,
    pub(crate) pages: Mutex<HashMap<u64, Vec<u8>>>,
}

impl MemStore {
    pub(crate) fn new(page_size: usize) -> Self {
        Self {
            page_size,
            pages: Mutex::new(HashMap::new()),
        }
    }

    /// Whether a page has ever been written to `id`.
    pub(crate) fn contains(&self, id: u64) -> bool {
        sync::lock(&self.pages).contains_key(&id)
    }
}

impl PageStore for MemStore {
    fn page_size(&self) -> usize {
        self.page_size
    }

    fn allocate_page(&self) -> Page {
        Page::new(PageSize::new(self.page_size).unwrap())
    }

    fn read_into(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        let pages = sync::lock(&self.pages);
        match pages.get(&id.get()) {
            Some(bytes) => {
                page.as_bytes_mut().copy_from_slice(bytes);
                page.verify(Some(id))
            }
            None => Err(PageError::ShortRead {
                page_id: id.get(),
                got: 0,
                page_size: self.page_size,
            }),
        }
    }

    fn write_page(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        page.stamp(id);
        let mut pages = sync::lock(&self.pages);
        let _ = pages.insert(id.get(), page.as_bytes().to_vec());
        Ok(())
    }

    fn sync(&self) -> PageResult<()> {
        Ok(())
    }
}

#[cfg(not(loom))]
impl MemStore {
    /// A store holding exactly `pages`, as a disk image after a crash.
    pub(crate) fn from_pages(page_size: usize, pages: HashMap<u64, Vec<u8>>) -> Self {
        Self {
            page_size,
            pages: Mutex::new(pages),
        }
    }
}

/// A store that models a disk with a volatile write cache: a write lands in
/// `pending` and becomes durable only at the next sync. After `budget` writes
/// and syncs it "loses power": every later call fails, and the test builds disk
/// images from `durable` plus any mix of the pending writes.
#[cfg(not(loom))]
pub(crate) struct CrashStore {
    page_size: usize,
    pub(crate) durable: std::sync::Mutex<HashMap<u64, Vec<u8>>>,
    pub(crate) pending: std::sync::Mutex<Vec<(u64, Vec<u8>)>>,
    budget: std::sync::Mutex<Option<usize>>,
    pub(crate) ops: std::sync::atomic::AtomicUsize,
}

#[cfg(not(loom))]
impl CrashStore {
    pub(crate) fn new(page_size: usize) -> Self {
        Self {
            page_size,
            durable: std::sync::Mutex::new(HashMap::new()),
            pending: std::sync::Mutex::new(Vec::new()),
            budget: std::sync::Mutex::new(None),
            ops: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Allow `n` more writes and syncs before the crash; `None` for no limit.
    pub(crate) fn set_budget(&self, n: Option<usize>) {
        *self.budget.lock().unwrap() = n;
    }

    fn spend(&self) -> PageResult<()> {
        let _ = self.ops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut budget = self.budget.lock().unwrap();
        match *budget {
            Some(0) => Err(PageError::Io(std::io::Error::other("power lost"))),
            Some(ref mut n) => {
                *n -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

#[cfg(not(loom))]
impl PageStore for CrashStore {
    fn page_size(&self) -> usize {
        self.page_size
    }

    fn allocate_page(&self) -> Page {
        Page::new(PageSize::new(self.page_size).unwrap())
    }

    fn read_into(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        // Reads see the newest write, cached or durable.
        let pending = self.pending.lock().unwrap();
        let newest = pending.iter().rev().find(|(p, _)| *p == id.get());
        let durable = self.durable.lock().unwrap();
        match newest.map(|(_, b)| b).or_else(|| durable.get(&id.get())) {
            Some(bytes) => {
                page.as_bytes_mut().copy_from_slice(bytes);
                page.verify(Some(id))
            }
            None => Err(PageError::ShortRead {
                page_id: id.get(),
                got: 0,
                page_size: self.page_size,
            }),
        }
    }

    fn write_page(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        self.spend()?;
        page.stamp(id);
        self.pending
            .lock()
            .unwrap()
            .push((id.get(), page.as_bytes().to_vec()));
        Ok(())
    }

    fn sync(&self) -> PageResult<()> {
        self.spend()?;
        let mut pending = self.pending.lock().unwrap();
        let mut durable = self.durable.lock().unwrap();
        for (id, bytes) in pending.drain(..) {
            let _ = durable.insert(id, bytes);
        }
        Ok(())
    }
}

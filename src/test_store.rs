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
    pages: Mutex<HashMap<u64, Vec<u8>>>,
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

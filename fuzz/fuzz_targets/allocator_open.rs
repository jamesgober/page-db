//! Fuzz the allocator open path: a corrupt superblock and free-list chain must
//! never panic, hang, or over-allocate while being parsed.
//!
//! The store hands the allocator the fuzzer's bytes directly, without checksum
//! verification — modeling the worst case of a checksum-valid but adversarially
//! crafted superblock (random corruption can't forge a CRC, but an attacker with
//! write access can). The free-chain walk must reject any cycle, out-of-range
//! link, or count mismatch with a typed error rather than looping.

#![no_main]

use std::collections::HashMap;

use libfuzzer_sys::fuzz_target;
use page_db::{PAGE_HEADER_SIZE, Page, PageAllocator, PageError, PageId, PageResult, PageSize};

const PAGE_SIZE: usize = 4096;

/// A non-verifying in-memory store seeded from the fuzz input: chunk `i` of the
/// input becomes the payload of page `i`.
struct FuzzStore {
    pages: HashMap<u64, Vec<u8>>,
}

impl FuzzStore {
    fn from_input(data: &[u8]) -> Self {
        let payload_len = PAGE_SIZE - PAGE_HEADER_SIZE;
        let mut pages = HashMap::new();
        for (i, chunk) in data.chunks(payload_len).enumerate() {
            let _ = pages.insert(i as u64, chunk.to_vec());
        }
        Self { pages }
    }
}

impl page_db::PageStore for FuzzStore {
    fn page_size(&self) -> usize {
        PAGE_SIZE
    }

    fn allocate_page(&self) -> Page {
        Page::new(PageSize::new(PAGE_SIZE).expect("valid page size"))
    }

    fn read_into(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        match self.pages.get(&id.get()) {
            Some(payload) => {
                let dst = page.payload_mut();
                let n = payload.len().min(dst.len());
                dst[..n].copy_from_slice(&payload[..n]);
                Ok(())
            }
            None => Err(PageError::ShortRead {
                page_id: id.get(),
                got: 0,
                page_size: PAGE_SIZE,
            }),
        }
    }

    fn write_page(&self, _id: PageId, _page: &mut Page) -> PageResult<()> {
        Ok(())
    }

    fn sync(&self) -> PageResult<()> {
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    let store = FuzzStore::from_input(data);
    if let Ok(alloc) = PageAllocator::new(store) {
        // The parsed state must be self-consistent and usable without panicking.
        let _ = alloc.high_water();
        let _ = alloc.free_count();
        if let Ok(id) = alloc.allocate() {
            let _ = alloc.free(id);
        }
        let _ = alloc.sync();
    }
});

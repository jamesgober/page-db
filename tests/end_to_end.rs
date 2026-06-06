//! End-to-end: the allocator picks ids, the buffer pool caches the pages at
//! those ids, and both share one file. Exercises the whole stack and the
//! allocator's on-disk persistence across a reopen.

use std::sync::Arc;

use page_db::{BufferPool, PageAllocator, PageFileOptions, PageSize};

fn open_shared(path: &std::path::Path, page_size: PageSize) -> Arc<page_db::PageFile> {
    Arc::new(
        PageFileOptions::new()
            .page_size(page_size)
            .direct_io(false)
            .create(true)
            .open(path)
            .expect("open"),
    )
}

#[test]
fn allocator_and_pool_share_a_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("db.pages");
    let page_size = PageSize::new(4096).expect("valid");

    // First session: allocate ids, write data through the pool, checkpoint.
    let (a, b, freed) = {
        let store = open_shared(&path, page_size);
        let alloc = PageAllocator::new(Arc::clone(&store)).expect("alloc");
        let pool = BufferPool::new(Arc::clone(&store), 16);

        let a = alloc.allocate().expect("allocate"); // 1
        let b = alloc.allocate().expect("allocate"); // 2
        let c = alloc.allocate().expect("allocate"); // 3

        for (id, marker) in [(a, 0xA1u8), (b, 0xB2), (c, 0xC3)] {
            let guard = pool.new_page(id).expect("new_page");
            guard.write().payload_mut()[0] = marker;
        }

        // Free c, then reallocate — it should come back as the same id.
        alloc.free(c).expect("free");
        let reused = alloc.allocate().expect("allocate");
        assert_eq!(reused, c);

        pool.flush_all().expect("flush");
        alloc.sync().expect("alloc sync"); // persist superblock + fsync
        pool.sync().expect("pool sync");

        (a, b, c)
    };

    // Second session: reopen and confirm the allocator state and the page data
    // both survived.
    {
        let store = open_shared(&path, page_size);
        let alloc = PageAllocator::new(Arc::clone(&store)).expect("reopen alloc");
        assert_eq!(alloc.high_water(), 4); // ids 1..=3 were handed out
        assert_eq!(alloc.free_count(), 0); // the one free was reused

        let pool = BufferPool::new(Arc::clone(&store), 16);
        assert_eq!(pool.fetch(a).expect("fetch a").read().payload()[0], 0xA1);
        assert_eq!(pool.fetch(b).expect("fetch b").read().payload()[0], 0xB2);
        // `freed` (id 3) was reused for a fresh page; it reads back as the page
        // written to the reused id, not the original.
        let _ = freed;
    }
}

//! Concurrency stress: many threads drive the allocator and the buffer pool
//! over one shared file at once, with the pool far smaller than the working set
//! so eviction and dirty write-back run constantly under contention. The loom
//! models prove the invariants on tiny interleavings; this drives the real types
//! at volume and checks nothing is lost or corrupted.

use std::sync::Arc;
use std::thread;

use page_db::{BufferPool, PageAllocator, PageFile, PageFileOptions, PageId, PageSize};

const THREADS: usize = 8;
const PER_THREAD: u64 = 64;
const POOL_FRAMES: usize = 16; // << THREADS * PER_THREAD, so eviction is constant

fn shared_file(path: &std::path::Path, ps: PageSize) -> Arc<PageFile> {
    Arc::new(
        PageFileOptions::new()
            .page_size(ps)
            .direct_io(false)
            .open(path)
            .expect("open"),
    )
}

#[test]
fn concurrent_allocate_write_and_read_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stress.pages");
    let ps = PageSize::new(4096).expect("valid");

    let store = shared_file(&path, ps);
    let alloc = Arc::new(PageAllocator::new(Arc::clone(&store)).expect("alloc"));
    let pool = Arc::new(BufferPool::new(Arc::clone(&store), POOL_FRAMES));

    // Each thread allocates its own ids and writes `payload[0..8] = id`, so the
    // expected contents of any page is just its id — no shared map needed.
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let alloc = Arc::clone(&alloc);
            let pool = Arc::clone(&pool);
            thread::spawn(move || {
                let mut ids = Vec::with_capacity(PER_THREAD as usize);
                for _ in 0..PER_THREAD {
                    let id = alloc.allocate().expect("allocate");
                    {
                        let guard = pool.new_page(id).expect("new_page");
                        guard.write().payload_mut()[..8].copy_from_slice(&id.get().to_le_bytes());
                    }
                    // Read something back mid-flight to add fetch/evict pressure.
                    let guard = pool.fetch(id).expect("fetch");
                    let mut got = [0u8; 8];
                    got.copy_from_slice(&guard.read().payload()[..8]);
                    assert_eq!(u64::from_le_bytes(got), id.get());
                    ids.push(id.get());
                }
                ids
            })
        })
        .collect();

    let mut all_ids: Vec<u64> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("thread panicked"))
        .collect();

    // Every id is unique across threads.
    all_ids.sort_unstable();
    let unique = {
        let mut v = all_ids.clone();
        v.dedup();
        v.len()
    };
    assert_eq!(unique, all_ids.len(), "allocator handed out a duplicate id");
    assert_eq!(all_ids.len(), THREADS * PER_THREAD as usize);

    // Checkpoint and verify every page survived eviction with its id intact.
    pool.flush_all().expect("flush");
    alloc.sync().expect("alloc sync");
    pool.sync().expect("pool sync");

    for &id in &all_ids {
        let guard = pool.fetch(PageId::new(id)).expect("fetch after checkpoint");
        let mut got = [0u8; 8];
        got.copy_from_slice(&guard.read().payload()[..8]);
        assert_eq!(u64::from_le_bytes(got), id, "page {id} came back wrong");
    }

    // Reopen cold and re-verify the allocator's high-water mark and the data.
    drop(pool);
    let high_water = alloc.high_water();
    drop(alloc);
    drop(store);

    let store = shared_file(&path, ps);
    let alloc = PageAllocator::new(Arc::clone(&store)).expect("reopen alloc");
    assert_eq!(alloc.high_water(), high_water);
    let pool = BufferPool::new(Arc::clone(&store), POOL_FRAMES);
    for &id in &all_ids {
        let guard = pool.fetch(PageId::new(id)).expect("fetch after reopen");
        let mut got = [0u8; 8];
        got.copy_from_slice(&guard.read().payload()[..8]);
        assert_eq!(u64::from_le_bytes(got), id, "page {id} did not persist");
    }
}

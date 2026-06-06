//! [`PageAllocator`]: hand out and reclaim page ids over a [`PageStore`].
//!
//! The allocator turns a flat array of page slots into a managed space of ids:
//! [`allocate`](PageAllocator::allocate) returns an unused id and
//! [`free`](PageAllocator::free) returns one for reuse. Reuse is tracked with a
//! free-list, and a high-water mark counts ids never yet handed out. Both
//! `allocate` and `free` are pure in-memory operations on the hot path — no I/O.
//!
//! On disk the free-list is an intrusive chain: each free page stores the id of
//! the next free page in its first eight bytes, with the chain head, the
//! high-water mark, and the free count in a **superblock** at page 0, which the
//! allocator reserves; ids it hands out start at 1.
//!
//! ## Durability
//!
//! `allocate` and `free` touch only memory. The on-disk state — the superblock
//! and the free-list chain — is written by [`sync`](PageAllocator::sync). Call
//! it as part of the same checkpoint that makes the pages it handed out durable:
//! the high-water mark must reach stable storage no later than the data written
//! to the ids beyond it, or a crash could re-hand-out an id whose page is
//! already durable. As everywhere in page-db, the write-ahead log above is the
//! authority on crash recovery; the superblock is a checkpoint of allocator
//! state.

use std::path::Path;

use crate::error::{PageError, PageResult};
use crate::file::PageFile;
use crate::page::{Page, PageId, PageSize};
use crate::store::PageStore;
use crate::sync::{self, Mutex};

/// Sentinel for "no page" — the empty free-list and end-of-chain marker.
const NO_PAGE: u64 = u64::MAX;

/// Superblock magic: `b"PGSB"`.
const SB_MAGIC: u32 = u32::from_le_bytes([b'P', b'G', b'S', b'B']);
const SB_VERSION: u16 = 1;

// Superblock field offsets within the page payload.
const SB_MAGIC_OFF: usize = 0;
const SB_VERSION_OFF: usize = 4;
const SB_HEAD_OFF: usize = 8;
const SB_NEXT_OFF: usize = 16;
const SB_FREECOUNT_OFF: usize = 24;

// Free-page link offset within the payload.
const LINK_NEXT_OFF: usize = 0;

/// The allocator's mutable state, behind one mutex.
struct AllocState {
    /// Ids available for reuse. `allocate` pops the back; `free` pushes it.
    free_list: Vec<u64>,
    /// The next id to hand out when the free-list is empty (high-water mark).
    next_new: u64,
    /// A reusable page buffer for chain and superblock I/O at sync time, so the
    /// allocator does no per-call allocation.
    scratch: Page,
}

/// A page-id allocator over a [`PageStore`].
///
/// `PageAllocator<S>` is generic over its backing store; the default is
/// [`PageFile`], so `PageAllocator` with no type parameter manages ids in a file
/// of pages. It is `Send + Sync` and every method takes `&self`.
///
/// The allocator owns page 0 (the superblock); the ids it returns are `>= 1`.
/// Pair it with a [`BufferPool`](crate::BufferPool) over the same store — the
/// allocator picks ids, the pool caches the pages at those ids — but do not use
/// page 0 for data, and free a page only once it is no longer in use.
///
/// # Examples
///
/// ```
/// use page_db::{PageAllocator, DEFAULT_PAGE_SIZE};
///
/// # let dir = tempfile::tempdir().unwrap();
/// # let path = dir.path().join("data.pages");
/// let alloc = PageAllocator::open(&path, DEFAULT_PAGE_SIZE)?;
///
/// let a = alloc.allocate()?;          // 1
/// let b = alloc.allocate()?;          // 2
/// alloc.free(a)?;                     // a goes on the free-list
/// let c = alloc.allocate()?;          // reuses a
/// assert_eq!(c, a);
/// assert_ne!(b, c);
/// alloc.sync()?;                      // persist the superblock durably
/// # Ok::<(), page_db::PageError>(())
/// ```
pub struct PageAllocator<S = PageFile> {
    store: S,
    state: Mutex<AllocState>,
}

impl PageAllocator<PageFile> {
    /// Open a page file and an allocator over it.
    ///
    /// Reads the superblock if the file already has one, or initializes a fresh
    /// one. A convenience over [`PageFile::open`] and [`PageAllocator::new`].
    ///
    /// # Errors
    ///
    /// - [`PageError::Io`] if the file cannot be opened.
    /// - [`PageError::InvalidSuperblock`] if page 0 exists but is not a
    ///   superblock this allocator wrote.
    pub fn open<P: AsRef<Path>>(path: P, page_size: PageSize) -> PageResult<Self> {
        let file = PageFile::open(path, page_size)?;
        Self::new(file)
    }
}

impl<S: PageStore> PageAllocator<S> {
    /// Build an allocator over `store`, reading or initializing its superblock.
    ///
    /// # Errors
    ///
    /// - [`PageError::InvalidSuperblock`] if page 0 exists but is not a valid
    ///   allocator superblock.
    /// - Whatever the store's read or write returns.
    pub fn new(store: S) -> PageResult<Self> {
        let mut scratch = store.allocate_page();

        let (free_list, next_new, fresh) = match store.read_into(PageId::new(0), &mut scratch) {
            Ok(()) => {
                let payload = scratch.payload();
                if read_u32(payload, SB_MAGIC_OFF) != SB_MAGIC
                    || read_u16(payload, SB_VERSION_OFF) != SB_VERSION
                {
                    return Err(PageError::InvalidSuperblock);
                }
                let head = read_u64(payload, SB_HEAD_OFF);
                let next_new = read_u64(payload, SB_NEXT_OFF);
                let free_count = read_u64(payload, SB_FREECOUNT_OFF);
                let free_list = walk_free_chain(&store, &mut scratch, head, free_count)?;
                (free_list, next_new, false)
            }
            // No page 0 yet: a fresh file. Reserve page 0 for the superblock and
            // start handing out ids at 1.
            Err(PageError::ShortRead { .. }) => (Vec::new(), 1, true),
            Err(err) => return Err(err),
        };

        let allocator = Self {
            store,
            state: Mutex::new(AllocState {
                free_list,
                next_new,
                scratch,
            }),
        };

        if fresh {
            let mut state = sync::lock(&allocator.state);
            allocator.persist_superblock(&mut state)?;
        }

        Ok(allocator)
    }

    /// Allocate an unused page id.
    ///
    /// Reuses a freed id if the free-list is non-empty, otherwise extends the
    /// high-water mark with a never-used id. This is an in-memory operation; the
    /// returned id's page has no defined contents until written.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidPageId`] only if the id space is exhausted (reachable
    /// at 2^64 pages).
    pub fn allocate(&self) -> PageResult<PageId> {
        let mut state = sync::lock(&self.state);

        if let Some(id) = state.free_list.pop() {
            return Ok(PageId::new(id));
        }

        let id = state.next_new;
        match id.checked_add(1) {
            Some(next) if next != NO_PAGE => {
                state.next_new = next;
                Ok(PageId::new(id))
            }
            _ => Err(PageError::InvalidPageId { page_id: id }),
        }
    }

    /// Return a page id to the free-list for reuse.
    ///
    /// An in-memory push; the free-list is written to the store at the next
    /// [`sync`](PageAllocator::sync). Freeing an id that is already free is a
    /// caller error and corrupts the free-list, exactly as with a system
    /// allocator.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidPageId`] if `id` is the reserved superblock (0) or was
    /// never allocated (at or beyond the high-water mark).
    pub fn free(&self, id: PageId) -> PageResult<()> {
        let raw = id.get();
        if raw == 0 {
            return Err(PageError::InvalidPageId { page_id: 0 });
        }
        let mut state = sync::lock(&self.state);
        if raw >= state.next_new {
            return Err(PageError::InvalidPageId { page_id: raw });
        }
        state.free_list.push(raw);
        Ok(())
    }

    /// The next id that would be newly allocated — one past the highest id ever
    /// handed out. Pages `1..high_water()` have been allocated at some point.
    #[must_use]
    pub fn high_water(&self) -> u64 {
        sync::lock(&self.state).next_new
    }

    /// The number of pages currently on the free-list, available for reuse
    /// without extending the file.
    #[must_use]
    pub fn free_count(&self) -> u64 {
        sync::lock(&self.state).free_list.len() as u64
    }

    /// Persist the superblock and free-list chain, then make the store durable.
    ///
    /// Writes the free-list onto its pages as an intrusive chain, records the
    /// head and high-water mark in page 0, then syncs the store. Call it as part
    /// of the checkpoint that makes the allocated pages durable (see the module
    /// docs on ordering). Cost is proportional to the free-list length.
    ///
    /// # Errors
    ///
    /// Whatever the store's writes or sync return.
    pub fn sync(&self) -> PageResult<()> {
        {
            let mut state = sync::lock(&self.state);
            self.persist_superblock(&mut state)?;
        }
        self.store.sync()
    }

    /// Write the free-list chain and the superblock at page 0. Caller holds the
    /// lock.
    fn persist_superblock(&self, state: &mut AllocState) -> PageResult<()> {
        // Write each free page's link so the on-disk chain matches `free_list`:
        // free_list[0] -> free_list[1] -> ... -> free_list[n-1] -> NO_PAGE,
        // with the superblock head pointing at free_list[0].
        let len = state.free_list.len();
        for i in 0..len {
            let id = state.free_list[i];
            let next = if i + 1 < len {
                state.free_list[i + 1]
            } else {
                NO_PAGE
            };
            state.scratch.reset();
            write_u64(state.scratch.payload_mut(), LINK_NEXT_OFF, next);
            self.store.write_page(PageId::new(id), &mut state.scratch)?;
        }
        let head = state.free_list.first().copied().unwrap_or(NO_PAGE);

        state.scratch.reset();
        let payload = state.scratch.payload_mut();
        write_u32(payload, SB_MAGIC_OFF, SB_MAGIC);
        write_u16(payload, SB_VERSION_OFF, SB_VERSION);
        write_u64(payload, SB_HEAD_OFF, head);
        write_u64(payload, SB_NEXT_OFF, state.next_new);
        write_u64(payload, SB_FREECOUNT_OFF, len as u64);
        self.store.write_page(PageId::new(0), &mut state.scratch)
    }
}

/// Follow the on-disk free-list chain from `head`, returning the ids in the same
/// order [`persist_superblock`](PageAllocator::persist_superblock) wrote them.
fn walk_free_chain<S: PageStore>(
    store: &S,
    scratch: &mut Page,
    head: u64,
    free_count: u64,
) -> PageResult<Vec<u64>> {
    let mut ids = Vec::new();
    let mut cur = head;
    for _ in 0..free_count {
        if cur == NO_PAGE {
            return Err(PageError::InvalidSuperblock);
        }
        ids.push(cur);
        store.read_into(PageId::new(cur), scratch)?;
        cur = read_u64(scratch.payload(), LINK_NEXT_OFF);
    }
    // The chain must end exactly at the recorded length.
    if cur != NO_PAGE {
        return Err(PageError::InvalidSuperblock);
    }
    Ok(ids)
}

#[inline]
fn read_u16(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

#[inline]
fn read_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

#[inline]
fn read_u64(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
        bytes[off + 4],
        bytes[off + 5],
        bytes[off + 6],
        bytes[off + 7],
    ])
}

#[inline]
fn write_u16(bytes: &mut [u8], off: usize, value: u16) {
    bytes[off..off + 2].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn write_u32(bytes: &mut [u8], off: usize, value: u32) {
    bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn write_u64(bytes: &mut [u8], off: usize, value: u64) {
    bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;

    use proptest::prelude::*;

    use super::*;
    use crate::test_store::MemStore;

    const PS: usize = 4096;

    fn allocator() -> PageAllocator<MemStore> {
        PageAllocator::new(MemStore::new(PS)).unwrap()
    }

    #[test]
    fn test_allocate_starts_at_one_and_increments() {
        let alloc = allocator();
        assert_eq!(alloc.allocate().unwrap(), PageId::new(1));
        assert_eq!(alloc.allocate().unwrap(), PageId::new(2));
        assert_eq!(alloc.allocate().unwrap(), PageId::new(3));
        assert_eq!(alloc.high_water(), 4);
    }

    #[test]
    fn test_free_then_allocate_reuses_id() {
        let alloc = allocator();
        let a = alloc.allocate().unwrap();
        let b = alloc.allocate().unwrap();
        alloc.free(a).unwrap();
        assert_eq!(alloc.free_count(), 1);
        let c = alloc.allocate().unwrap();
        assert_eq!(c, a);
        assert_ne!(c, b);
        assert_eq!(alloc.free_count(), 0);
    }

    #[test]
    fn test_free_list_is_lifo() {
        let alloc = allocator();
        let ids: Vec<_> = (0..4).map(|_| alloc.allocate().unwrap()).collect();
        for &id in &ids {
            alloc.free(id).unwrap();
        }
        // Freed 1,2,3,4; the list pops most-recently-freed first.
        let mut reused = Vec::new();
        for _ in 0..4 {
            reused.push(alloc.allocate().unwrap());
        }
        let expected: Vec<_> = ids.into_iter().rev().collect();
        assert_eq!(reused, expected);
    }

    #[test]
    fn test_free_rejects_superblock_and_unallocated() {
        let alloc = allocator();
        let _ = alloc.allocate().unwrap(); // high_water now 2
        assert!(matches!(
            alloc.free(PageId::new(0)),
            Err(PageError::InvalidPageId { page_id: 0 })
        ));
        assert!(matches!(
            alloc.free(PageId::new(5)),
            Err(PageError::InvalidPageId { page_id: 5 })
        ));
    }

    #[test]
    fn test_state_survives_reopen() {
        let store = MemStore::new(PS);
        {
            let alloc = PageAllocator::new(store).unwrap();
            let _ = alloc.allocate().unwrap(); // 1
            let b = alloc.allocate().unwrap(); // 2
            let _ = alloc.allocate().unwrap(); // 3
            alloc.free(b).unwrap(); // 2 freed
            alloc.sync().unwrap(); // persist superblock
            // Recover the store from the allocator to reopen over it.
            let alloc2 = PageAllocator::new(alloc.into_store()).unwrap();
            assert_eq!(alloc2.high_water(), 4);
            assert_eq!(alloc2.free_count(), 1);
            // Next allocate reuses the freed id 2.
            assert_eq!(alloc2.allocate().unwrap(), PageId::new(2));
        }
    }

    #[test]
    fn test_new_rejects_non_superblock_page_zero() {
        let store = MemStore::new(PS);
        // Write a non-superblock page at slot 0.
        {
            let mut page = store.allocate_page();
            page.payload_mut()[0] = 0xFF;
            store.write_page(PageId::new(0), &mut page).unwrap();
        }
        assert!(matches!(
            PageAllocator::new(store),
            Err(PageError::InvalidSuperblock)
        ));
    }

    // Test-only accessor to reopen over the same store.
    impl PageAllocator<MemStore> {
        fn into_store(self) -> MemStore {
            self.store
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Allocated ids are always unique while live: through any sequence of
        /// allocates and frees, two outstanding ids never coincide, and a freed
        /// id is only ever handed back out after being freed.
        #[test]
        fn allocate_free_never_double_allocates(
            ops in proptest::collection::vec(any::<bool>(), 1..200),
        ) {
            let alloc = allocator();
            let mut live: HashSet<u64> = HashSet::new();
            let mut freed_pool: Vec<u64> = Vec::new();

            for want_alloc in ops {
                if want_alloc || live.is_empty() {
                    let id = alloc.allocate().unwrap().get();
                    // The id must not already be live.
                    prop_assert!(!live.contains(&id), "id {} double-allocated", id);
                    prop_assert!(id >= 1, "id 0 is reserved");
                    let _ = live.insert(id);
                    freed_pool.retain(|&f| f != id);
                } else {
                    // Free an arbitrary live id (the first one).
                    let victim = *live.iter().next().unwrap();
                    let _ = live.remove(&victim);
                    alloc.free(PageId::new(victim)).unwrap();
                    freed_pool.push(victim);
                }
                prop_assert_eq!(alloc.free_count(), freed_pool.len() as u64);
            }
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use crate::sync::Arc;
    use crate::test_store::MemStore;

    /// Concurrent allocations never collide: two threads each allocate, and the
    /// two ids they get are always distinct, under every interleaving.
    #[test]
    fn loom_concurrent_allocate_is_unique() {
        loom::model(|| {
            let alloc = Arc::new(PageAllocator::new(MemStore::new(4096)).unwrap());

            let a = Arc::clone(&alloc);
            let t = loom::thread::spawn(move || a.allocate().unwrap());

            let first = alloc.allocate().unwrap();
            let second = t.join().unwrap();
            assert_ne!(first, second);
        });
    }
}

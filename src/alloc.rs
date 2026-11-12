//! [`PageAllocator`]: hand out and reclaim page ids over a [`PageStore`].
//!
//! The allocator turns a flat array of page slots into a managed space of ids:
//! [`allocate`](PageAllocator::allocate) returns an unused id and
//! [`free`](PageAllocator::free) returns one for reuse. Reuse is tracked with a
//! free-list, and a high-water mark counts ids never yet handed out. Both
//! `allocate` and `free` are pure in-memory operations on the hot path, with no
//! I/O. `free` refuses an id that is already free, so a double free is an error
//! instead of an id handed to two owners.
//!
//! On disk the free-list is an intrusive chain: each free page stores the id of
//! the next free page in its first eight bytes, with the chain head, the
//! high-water mark, and the free count in a **superblock** at page 0, which the
//! allocator reserves; ids it hands out start at 1.
//!
//! ## Superblock copies
//!
//! Page 0 holds the 1.0 superblock fields and, in the payload space 1.0 leaves
//! zero, two **superblock records**: copy A in the first sector of the page and
//! copy B in its second half. Each record carries a generation number, the
//! allocator state, a checksum of the free-list ids, and its own CRC32C. A
//! [`sync`](PageAllocator::sync) writes the changed free-list links and syncs
//! them, then writes page 0 with only copy B updated and syncs, then writes it
//! with the 1.0 fields and copy A updated and syncs. A torn write of page 0
//! therefore leaves at least one intact record, and opening picks the newest
//! record whose free-list chain still checks out, so one damaged superblock page
//! no longer makes the file unopenable. Releases before 1.1 ignore the records
//! and read the 1.0 fields, which always hold the newest committed state; files
//! written by 1.0 have no records and open from the 1.0 fields as before.
//!
//! If no record's chain checks out (a crash in the middle of a sync, or a
//! free-list page overwritten by something else), the allocator opens from the
//! newest valid record with an **empty free-list**: the pages that were free are
//! leaked, never handed out twice. [`free_list_dropped`](PageAllocator::free_list_dropped)
//! reports it, and [`from_state`](PageAllocator::from_state) rebuilds an
//! allocator from state the write-ahead log holds.
//!
//! ## Durability
//!
//! `allocate` and `free` touch only memory. The on-disk state (the superblock
//! and the free-list chain) is written by [`sync`](PageAllocator::sync). Call
//! it as part of the same checkpoint that makes the pages it handed out durable:
//! the high-water mark must reach stable storage no later than the data written
//! to the ids beyond it, or a crash could re-hand-out an id whose page is
//! already durable. As everywhere in page-db, the write-ahead log above is the
//! authority on crash recovery; the superblock is a checkpoint of allocator
//! state.
//!
//! The free-list links live inside the freed pages and are written straight to
//! the store. When a [`BufferPool`](crate::BufferPool) shares the store, discard
//! a page from the pool ([`BufferPool::discard`](crate::BufferPool::discard))
//! before freeing it, or the pool's next flush of a stale dirty frame can
//! overwrite the link.

use std::collections::HashSet;
use std::path::Path;

use crate::checksum::Crc32c;
use crate::error::{PageError, PageResult};
use crate::file::PageFile;
use crate::page::{PAGE_HEADER_SIZE, Page, PageId, PageSize};
use crate::store::PageStore;
use crate::sync::{self, Mutex};

/// Sentinel for "no page": the empty free-list and end-of-chain marker.
const NO_PAGE: u64 = u64::MAX;

/// Superblock magic: `b"PGSB"`.
const SB_MAGIC: u32 = u32::from_le_bytes(*b"PGSB");
const SB_VERSION: u16 = 1;

// Superblock field offsets within the page payload (the 1.0 fields).
const SB_MAGIC_OFF: usize = 0;
const SB_VERSION_OFF: usize = 4;
const SB_HEAD_OFF: usize = 8;
const SB_NEXT_OFF: usize = 16;
const SB_FREECOUNT_OFF: usize = 24;

/// Superblock record magic: `b"PGSR"`.
const REC_MAGIC: u32 = u32::from_le_bytes(*b"PGSR");
const REC_VERSION: u16 = 1;

// Superblock record layout, relative to the record's start.
const REC_MAGIC_OFF: usize = 0;
const REC_VERSION_OFF: usize = 4;
const REC_GEN_OFF: usize = 8;
const REC_HEAD_OFF: usize = 16;
const REC_NEXT_OFF: usize = 24;
const REC_FREECOUNT_OFF: usize = 32;
const REC_CHAIN_OFF: usize = 40;
const REC_CRC_OFF: usize = 44;
const REC_LEN: usize = 48;

/// Payload offset of superblock record A, right after the 1.0 fields, in the
/// page's first 512-byte sector.
const REC_A_OFF: usize = 32;

/// Payload offset of superblock record B: the start of the page's second half.
#[inline]
const fn rec_b_off(page_size: usize) -> usize {
    page_size / 2 - PAGE_HEADER_SIZE
}

// Free-page link offset within the payload.
const LINK_NEXT_OFF: usize = 0;

/// One copy of the allocator's persistent state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    generation: u64,
    head: u64,
    next_new: u64,
    free_count: u64,
    /// Checksum of the free-list ids in chain order; `None` for the 1.0
    /// fields, which carry none.
    chain_crc: Option<u32>,
}

/// The allocator's mutable state, behind one mutex.
struct AllocState {
    /// Ids available for reuse. `allocate` pops the back; `free` pushes it.
    free_list: Vec<u64>,
    /// The same ids as `free_list`, to refuse a double free.
    free_set: HashSet<u64>,
    /// The next id to hand out when the free-list is empty (high-water mark).
    next_new: u64,
    /// The length of the leading run of `free_list` whose links on disk are
    /// still current, except that the last entry of the run may point at a
    /// stale successor. `sync` rewrites links only from there on.
    stable: usize,
    /// Whether `free_list` changed since the last successful sync.
    links_dirty: bool,
}

/// State used only by `sync`, behind its own mutex so syncs are serialized
/// while `allocate` and `free` proceed during the slow part of a sync.
struct SyncState {
    /// The highest record generation written or seen.
    generation: u64,
    /// The record last committed to the 1.0 fields and copy A.
    committed: Option<Record>,
    /// A reusable page buffer for chain and superblock I/O, so the allocator
    /// does no per-call allocation.
    scratch: Page,
}

/// A page-id allocator over a [`PageStore`].
///
/// `PageAllocator<S>` is generic over its backing store; the default is
/// [`PageFile`], so `PageAllocator` with no type parameter manages ids in a file
/// of pages. It is `Send + Sync` and every method takes `&self`.
///
/// The allocator owns page 0 (the superblock); the ids it returns are `>= 1`.
/// Pair it with a [`BufferPool`](crate::BufferPool) over the same store (the
/// allocator picks ids, the pool caches the pages at those ids), but do not use
/// page 0 for data, free a page only once it is no longer in use, and
/// [`discard`](crate::BufferPool::discard) it from the pool before freeing it.
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
/// assert!(alloc.free(a).is_err());    // a second free is refused
/// let c = alloc.allocate()?;          // reuses a
/// assert_eq!(c, a);
/// assert_ne!(b, c);
/// alloc.sync()?;                      // persist the superblock durably
/// # Ok::<(), page_db::PageError>(())
/// ```
pub struct PageAllocator<S = PageFile> {
    store: S,
    state: Mutex<AllocState>,
    sync_state: Mutex<SyncState>,
    /// One past the largest id whose page has a representable byte offset.
    max_next_new: u64,
    /// Whether opening had to drop the free-list (see `free_list_dropped`).
    dropped: bool,
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
    /// Page 0 is read and the newest intact superblock copy whose free-list
    /// chain checks out is used (see the module docs). A torn or corrupt page 0
    /// still opens as long as one copy survived; for that, the store's
    /// `read_into` must leave the bytes it read in the page when it reports an
    /// integrity error, as [`PageFile`] does.
    ///
    /// # Errors
    ///
    /// - [`PageError::InvalidSuperblock`] if page 0 exists but is not a valid
    ///   allocator superblock.
    /// - The integrity error from reading page 0 if it is damaged and no
    ///   superblock copy in it survived.
    /// - Whatever the store's read or write returns.
    pub fn new(store: S) -> PageResult<Self> {
        let page_size = store.page_size();
        let max_next_new = PageId::addressable(page_size);
        let mut scratch = store.allocate_page();

        let opened = match store.read_into(PageId::new(0), &mut scratch) {
            // No page 0 yet: a fresh file. Reserve page 0 for the superblock and
            // start handing out ids at 1.
            Err(PageError::ShortRead { .. }) => None,
            Ok(()) => Some(recover(&store, &mut scratch, max_next_new, None)?),
            Err(err) if is_integrity(&err) => {
                Some(recover(&store, &mut scratch, max_next_new, Some(err))?)
            }
            Err(err) => return Err(err),
        };

        let (free_list, free_set, next_new, generation, committed, dropped) = match opened {
            Some(r) => {
                let mut record = r.record;
                // The 1.0 fields carry no chain checksum; the chain just walked
                // is what is on disk, so record its checksum for the next sync.
                if record.chain_crc.is_none() {
                    record.chain_crc = Some(chain_crc(&r.free_list));
                }
                (
                    r.free_list,
                    r.free_set,
                    record.next_new,
                    r.generation,
                    Some(record),
                    r.dropped,
                )
            }
            None => (Vec::new(), HashSet::new(), 1, 0, None, false),
        };
        let fresh = committed.is_none();
        let stable = free_list.len();

        let allocator = Self {
            store,
            state: Mutex::new(AllocState {
                free_list,
                free_set,
                next_new,
                stable,
                links_dirty: false,
            }),
            sync_state: Mutex::new(SyncState {
                generation,
                committed,
                scratch,
            }),
            max_next_new,
            dropped,
        };

        if fresh {
            // As in 1.0, a fresh superblock is written but not synced; the first
            // `sync` makes it durable.
            let mut ss = sync::lock(&allocator.sync_state);
            let record = Record {
                generation: 1,
                head: NO_PAGE,
                next_new: 1,
                free_count: 0,
                chain_crc: Some(chain_crc(&[])),
            };
            write_superblock(&allocator.store, &mut ss.scratch, &record, &record)?;
            ss.generation = 1;
            ss.committed = Some(record);
        }

        Ok(allocator)
    }

    /// Build an allocator over `store` from state held elsewhere, typically
    /// rebuilt from a write-ahead log, ignoring whatever superblock the store
    /// holds.
    ///
    /// `next_new` is the high-water mark (pages `1..next_new` have been
    /// allocated at some point) and `free_ids` the ids available for reuse; the
    /// last one is handed out first. Nothing is written until the next
    /// [`sync`](PageAllocator::sync), which rewrites the superblock and every
    /// free-list link.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidPageId`] if `next_new` is 0 or beyond the addressable
    /// range of the store's page size, or if a free id is 0, at or beyond
    /// `next_new`, or listed twice.
    pub fn from_state<I>(store: S, next_new: u64, free_ids: I) -> PageResult<Self>
    where
        I: IntoIterator<Item = PageId>,
    {
        let page_size = store.page_size();
        let max_next_new = PageId::addressable(page_size);
        if next_new == 0 || next_new > max_next_new {
            return Err(PageError::InvalidPageId { page_id: next_new });
        }
        let mut free_list = Vec::new();
        let mut free_set = HashSet::new();
        for id in free_ids {
            let raw = id.get();
            if raw == 0 || raw >= next_new || !free_set.insert(raw) {
                return Err(PageError::InvalidPageId { page_id: raw });
            }
            free_list.push(raw);
        }

        // Continue numbering after any superblock record still on disk, so a
        // stale record can never outrank the ones this allocator writes.
        let mut scratch = store.allocate_page();
        let generation = match store.read_into(PageId::new(0), &mut scratch) {
            Ok(()) => newest_generation(&scratch),
            Err(err) if is_integrity(&err) => newest_generation(&scratch),
            Err(_) => 0,
        };

        Ok(Self {
            store,
            state: Mutex::new(AllocState {
                free_list,
                free_set,
                next_new,
                stable: 0,
                links_dirty: true,
            }),
            sync_state: Mutex::new(SyncState {
                generation,
                committed: None,
                scratch,
            }),
            max_next_new,
            dropped: false,
        })
    }

    /// Allocate an unused page id.
    ///
    /// Reuses a freed id if the free-list is non-empty, otherwise extends the
    /// high-water mark with a never-used id. This is an in-memory operation; the
    /// returned id's page has no defined contents until written.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidPageId`] only if the id space is exhausted: the next
    /// id's byte offset would not fit in a 64-bit file offset (2^52 pages at
    /// 4 KiB).
    pub fn allocate(&self) -> PageResult<PageId> {
        let mut state = sync::lock(&self.state);

        if let Some(id) = state.free_list.pop() {
            let _ = state.free_set.remove(&id);
            state.stable = state.stable.min(state.free_list.len());
            state.links_dirty = true;
            return Ok(PageId::new(id));
        }

        let id = state.next_new;
        if id >= self.max_next_new {
            return Err(PageError::InvalidPageId { page_id: id });
        }
        state.next_new = id + 1;
        Ok(PageId::new(id))
    }

    /// Return a page id to the free-list for reuse.
    ///
    /// An in-memory push; the free-list is written to the store at the next
    /// [`sync`](PageAllocator::sync). If a [`BufferPool`](crate::BufferPool)
    /// shares the store, [`discard`](crate::BufferPool::discard) the page from
    /// it first.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidPageId`] if `id` is the reserved superblock (0), was
    /// never allocated (at or beyond the high-water mark), or is already free.
    /// The free-list is unchanged.
    pub fn free(&self, id: PageId) -> PageResult<()> {
        let raw = id.get();
        if raw == 0 {
            return Err(PageError::InvalidPageId { page_id: 0 });
        }
        let mut state = sync::lock(&self.state);
        if raw >= state.next_new || !state.free_set.insert(raw) {
            return Err(PageError::InvalidPageId { page_id: raw });
        }
        state.free_list.push(raw);
        state.links_dirty = true;
        Ok(())
    }

    /// The next id that would be newly allocated: one past the highest id ever
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

    /// Whether opening found no superblock copy whose free-list chain checked
    /// out, and so started with an empty free-list.
    ///
    /// This happens after a crash in the middle of a
    /// [`sync`](PageAllocator::sync), or when a free page's link was overwritten
    /// (for example by a buffer pool flushing a page that was freed without
    /// being discarded). The high-water mark is still the newest one on disk, so
    /// no id is handed out twice, but the pages that were free are leaked until
    /// the allocator is rebuilt with [`from_state`](PageAllocator::from_state).
    #[must_use]
    pub fn free_list_dropped(&self) -> bool {
        self.dropped
    }

    /// Persist the superblock and free-list chain, then make the store durable.
    ///
    /// Writes the free-list links that changed since the last sync and syncs
    /// them, then writes page 0 twice, syncing after each write, so the two
    /// superblock copies are never both in flight (see the module docs). Call it
    /// as part of the checkpoint that makes the allocated pages durable (see the
    /// module docs on ordering). `allocate` and `free` are blocked only while
    /// the links are written, not during the syncs.
    ///
    /// # Errors
    ///
    /// Whatever the store's writes or syncs return. A failed sync can be
    /// retried, but a [`PageFile`] refuses every sync after a failed one.
    pub fn sync(&self) -> PageResult<()> {
        let mut ss = sync::lock(&self.sync_state);
        let ss = &mut *ss;

        let (record, old_stable, wrote_links) = {
            let mut state = sync::lock(&self.state);
            let len = state.free_list.len();
            let old_stable = state.stable;
            let mut wrote_links = false;
            if state.links_dirty {
                // Entries before `stable - 1` still point at the right successor
                // on disk; rewrite the rest. A failure leaves `stable` as it was.
                for i in old_stable.saturating_sub(1)..len {
                    let id = state.free_list[i];
                    let next = state.free_list.get(i + 1).copied().unwrap_or(NO_PAGE);
                    ss.scratch.reset();
                    write_u64(ss.scratch.payload_mut(), LINK_NEXT_OFF, next);
                    self.store.write_page(PageId::new(id), &mut ss.scratch)?;
                    wrote_links = true;
                }
            }
            let record = Record {
                generation: ss.generation + 1,
                head: state.free_list.first().copied().unwrap_or(NO_PAGE),
                next_new: state.next_new,
                free_count: len as u64,
                chain_crc: Some(chain_crc(&state.free_list)),
            };
            // Assume success; operations from here on lower `stable` as usual,
            // and a failure below lowers it back to what it was.
            state.stable = len;
            state.links_dirty = false;
            (record, old_stable, wrote_links)
        };
        ss.generation = record.generation;

        let result = self.commit(ss, &record, wrote_links);
        if result.is_err() {
            let mut state = sync::lock(&self.state);
            state.stable = state.stable.min(old_stable);
            state.links_dirty = true;
        }
        result
    }

    /// The durable part of `sync`: links, then copy B, then the 1.0 fields and
    /// copy A, each step synced before the next starts.
    fn commit(&self, ss: &mut SyncState, record: &Record, wrote_links: bool) -> PageResult<()> {
        if wrote_links {
            self.store.sync()?;
        }
        let previous = ss.committed.unwrap_or(*record);
        write_superblock(&self.store, &mut ss.scratch, &previous, record)?;
        self.store.sync()?;
        write_superblock(&self.store, &mut ss.scratch, record, record)?;
        self.store.sync()?;
        ss.committed = Some(*record);
        Ok(())
    }
}

/// What opening an existing superblock produced.
struct Opened {
    record: Record,
    generation: u64,
    free_list: Vec<u64>,
    free_set: HashSet<u64>,
    dropped: bool,
}

/// Choose the allocator state from the page 0 bytes in `scratch`.
///
/// `read_err` is `None` when page 0 verified as a whole, or the integrity error
/// it failed with; then only the superblock records, which carry their own
/// checksums, are trusted.
fn recover<S: PageStore>(
    store: &S,
    scratch: &mut Page,
    max_next_new: u64,
    read_err: Option<PageError>,
) -> PageResult<Opened> {
    let page_size = scratch.page_size();
    let mut records: Vec<Record> = [REC_A_OFF, rec_b_off(page_size)]
        .iter()
        .filter_map(|&off| decode_record(scratch.payload(), off))
        .collect();
    records.sort_by_key(|r| std::cmp::Reverse(r.generation));
    records.dedup();
    let generation = records.first().map_or(0, |r| r.generation);

    let mut candidates = records.clone();
    match read_err {
        None => {
            // A page that verifies must carry the 1.0 fields; it is the only
            // copy in a file written before 1.1.
            let payload = scratch.payload();
            if read_u32(payload, SB_MAGIC_OFF) != SB_MAGIC
                || read_u16(payload, SB_VERSION_OFF) != SB_VERSION
            {
                return Err(PageError::InvalidSuperblock);
            }
            candidates.push(Record {
                generation: 0,
                head: read_u64(payload, SB_HEAD_OFF),
                next_new: read_u64(payload, SB_NEXT_OFF),
                free_count: read_u64(payload, SB_FREECOUNT_OFF),
                chain_crc: None,
            });
        }
        Some(err) => {
            if records.is_empty() {
                return Err(err);
            }
        }
    }

    let mut last_err = PageError::InvalidSuperblock;
    for candidate in &candidates {
        if candidate.next_new < 1 || candidate.next_new > max_next_new {
            last_err = PageError::InvalidSuperblock;
            continue;
        }
        match walk_free_chain(
            store,
            scratch,
            candidate.head,
            candidate.next_new,
            candidate.free_count,
        ) {
            Ok((free_list, free_set)) => {
                if candidate
                    .chain_crc
                    .is_some_and(|crc| crc != chain_crc(&free_list))
                {
                    last_err = PageError::InvalidSuperblock;
                    continue;
                }
                return Ok(Opened {
                    record: *candidate,
                    generation,
                    free_list,
                    free_set,
                    dropped: false,
                });
            }
            Err(err) if is_integrity(&err) => last_err = err,
            Err(err) => return Err(err),
        }
    }

    // No chain checks out. With only the 1.0 fields to go on, that is an error,
    // as it was in 1.0. With records, open from the highest high-water mark any
    // of them holds and leak the free pages, which never hands out an id twice.
    let usable = records
        .iter()
        .filter(|r| r.next_new >= 1 && r.next_new <= max_next_new)
        .max_by_key(|r| r.next_new);
    match usable {
        Some(record) => Ok(Opened {
            record: *record,
            generation,
            free_list: Vec::new(),
            free_set: HashSet::new(),
            dropped: true,
        }),
        None => Err(last_err),
    }
}

/// The highest generation among the valid superblock records in `page`, or 0.
fn newest_generation(page: &Page) -> u64 {
    [REC_A_OFF, rec_b_off(page.page_size())]
        .iter()
        .filter_map(|&off| decode_record(page.payload(), off))
        .map(|r| r.generation)
        .max()
        .unwrap_or(0)
}

/// Whether `err` says the bytes read were bad (as opposed to the read failing).
fn is_integrity(err: &PageError) -> bool {
    matches!(
        err,
        PageError::BadMagic { .. }
            | PageError::UnsupportedVersion { .. }
            | PageError::ChecksumMismatch { .. }
            | PageError::MisdirectedPage { .. }
            | PageError::ShortRead { .. }
            | PageError::InvalidSuperblock
            | PageError::InvalidPageId { .. }
    )
}

/// Write page 0: the 1.0 fields and copy A from `a`, copy B from `b`.
fn write_superblock<S: PageStore>(
    store: &S,
    scratch: &mut Page,
    a: &Record,
    b: &Record,
) -> PageResult<()> {
    let page_size = scratch.page_size();
    scratch.reset();
    let payload = scratch.payload_mut();
    write_u32(payload, SB_MAGIC_OFF, SB_MAGIC);
    write_u16(payload, SB_VERSION_OFF, SB_VERSION);
    write_u64(payload, SB_HEAD_OFF, a.head);
    write_u64(payload, SB_NEXT_OFF, a.next_new);
    write_u64(payload, SB_FREECOUNT_OFF, a.free_count);
    encode_record(payload, REC_A_OFF, a);
    encode_record(payload, rec_b_off(page_size), b);
    store.write_page(PageId::new(0), scratch)
}

/// The checksum of a free-list in chain order.
fn chain_crc(ids: &[u64]) -> u32 {
    let mut crc = Crc32c::new();
    for id in ids {
        crc.update(&id.to_le_bytes());
    }
    crc.finalize()
}

fn encode_record(payload: &mut [u8], off: usize, record: &Record) {
    let bytes = &mut payload[off..off + REC_LEN];
    bytes.fill(0);
    write_u32(bytes, REC_MAGIC_OFF, REC_MAGIC);
    write_u16(bytes, REC_VERSION_OFF, REC_VERSION);
    write_u64(bytes, REC_GEN_OFF, record.generation);
    write_u64(bytes, REC_HEAD_OFF, record.head);
    write_u64(bytes, REC_NEXT_OFF, record.next_new);
    write_u64(bytes, REC_FREECOUNT_OFF, record.free_count);
    write_u32(bytes, REC_CHAIN_OFF, record.chain_crc.unwrap_or(0));
    let crc = crate::checksum::crc32c(&bytes[..REC_CRC_OFF]);
    write_u32(bytes, REC_CRC_OFF, crc);
}

fn decode_record(payload: &[u8], off: usize) -> Option<Record> {
    let bytes = payload.get(off..off + REC_LEN)?;
    if read_u32(bytes, REC_MAGIC_OFF) != REC_MAGIC
        || read_u16(bytes, REC_VERSION_OFF) != REC_VERSION
        || read_u32(bytes, REC_CRC_OFF) != crate::checksum::crc32c(&bytes[..REC_CRC_OFF])
    {
        return None;
    }
    Some(Record {
        generation: read_u64(bytes, REC_GEN_OFF),
        head: read_u64(bytes, REC_HEAD_OFF),
        next_new: read_u64(bytes, REC_NEXT_OFF),
        free_count: read_u64(bytes, REC_FREECOUNT_OFF),
        chain_crc: Some(read_u32(bytes, REC_CHAIN_OFF)),
    })
}

/// Follow the on-disk free-list chain from `head`, returning the ids in the same
/// order `sync` wrote them, and the same ids as a set.
///
/// This walks an untrusted, possibly corrupt superblock, so it is bounded
/// against a malicious chain: every link must be a real allocated id
/// (`1..next_new`), no id may repeat (a cycle is rejected, not followed), the
/// walk never collects more than `free_count` ids, and the final length must
/// match `free_count` exactly. Any deviation is [`PageError::InvalidSuperblock`].
fn walk_free_chain<S: PageStore>(
    store: &S,
    scratch: &mut Page,
    head: u64,
    next_new: u64,
    free_count: u64,
) -> PageResult<(Vec<u64>, HashSet<u64>)> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = head;

    while cur != NO_PAGE {
        // Never collect more links than the superblock claims, so a corrupt
        // count can never drive an unbounded walk.
        if ids.len() as u64 >= free_count {
            return Err(PageError::InvalidSuperblock);
        }
        // Every free id must be a real allocated page (page 0 is reserved).
        if cur == 0 || cur >= next_new {
            return Err(PageError::InvalidSuperblock);
        }
        // A repeat means the chain cycles; reject rather than loop. `seen` grows
        // only to the number of distinct existing pages, so it cannot run away.
        if !seen.insert(cur) {
            return Err(PageError::InvalidSuperblock);
        }
        ids.push(cur);
        store.read_into(PageId::new(cur), scratch)?;
        cur = read_u64(scratch.payload(), LINK_NEXT_OFF);
    }

    // The chain must end exactly at the recorded length.
    if ids.len() as u64 != free_count {
        return Err(PageError::InvalidSuperblock);
    }
    Ok((ids, seen))
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

    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use proptest::prelude::*;

    use super::*;
    use crate::test_store::{CrashStore, MemStore};

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

    /// Write a raw superblock (no consistency checks) to drive the corrupt-input
    /// rejection tests.
    fn write_legacy_superblock(store: &MemStore, head: u64, next_new: u64, free_count: u64) {
        let mut page = store.allocate_page();
        let payload = page.payload_mut();
        write_u32(payload, SB_MAGIC_OFF, SB_MAGIC);
        write_u16(payload, SB_VERSION_OFF, SB_VERSION);
        write_u64(payload, SB_HEAD_OFF, head);
        write_u64(payload, SB_NEXT_OFF, next_new);
        write_u64(payload, SB_FREECOUNT_OFF, free_count);
        store.write_page(PageId::new(0), &mut page).unwrap();
    }

    fn write_link(store: &MemStore, id: u64, next: u64) {
        let mut page = store.allocate_page();
        write_u64(page.payload_mut(), LINK_NEXT_OFF, next);
        store.write_page(PageId::new(id), &mut page).unwrap();
    }

    #[test]
    fn test_new_rejects_cycled_free_chain() {
        let store = MemStore::new(PS);
        write_legacy_superblock(&store, 1, 3, 10); // claims 10 free in id space 1..3
        write_link(&store, 1, 2);
        write_link(&store, 2, 1); // 1 -> 2 -> 1 cycles
        assert!(matches!(
            PageAllocator::new(store),
            Err(PageError::InvalidSuperblock)
        ));
    }

    #[test]
    fn test_new_rejects_out_of_range_link() {
        let store = MemStore::new(PS);
        write_legacy_superblock(&store, 5, 3, 1); // head 5 is past the high-water mark
        assert!(matches!(
            PageAllocator::new(store),
            Err(PageError::InvalidSuperblock)
        ));
    }

    #[test]
    fn test_new_rejects_free_count_mismatch() {
        let store = MemStore::new(PS);
        write_legacy_superblock(&store, 1, 3, 5); // claims 5 free
        write_link(&store, 1, NO_PAGE); // chain is only 1 long
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

    impl<S: PageStore> PageAllocator<S> {
        /// The high-water mark and the free-list in chain order.
        fn snapshot(&self) -> (u64, Vec<u64>) {
            let state = sync::lock(&self.state);
            (state.next_new, state.free_list.clone())
        }
    }

    #[test]
    fn test_double_free_is_refused() {
        let alloc = allocator();
        let a = alloc.allocate().unwrap();
        alloc.free(a).unwrap();
        assert!(matches!(
            alloc.free(a),
            Err(PageError::InvalidPageId { page_id }) if page_id == a.get()
        ));
        assert_eq!(alloc.free_count(), 1);
        assert_eq!(alloc.allocate().unwrap(), a);
        assert_ne!(alloc.allocate().unwrap(), a);
    }

    #[test]
    fn test_sync_rewrites_only_changed_links() {
        let store = Arc::new(CrashStore::new(PS));
        let alloc = PageAllocator::new(Arc::clone(&store)).unwrap();
        let ids: Vec<_> = (0..50).map(|_| alloc.allocate().unwrap()).collect();
        for &id in &ids {
            alloc.free(id).unwrap();
        }
        alloc.sync().unwrap();
        let ops = |s: &CrashStore| s.ops.load(std::sync::atomic::Ordering::SeqCst);

        // Nothing changed: two superblock writes and two syncs, no links.
        let before = ops(&store);
        alloc.sync().unwrap();
        assert_eq!(ops(&store) - before, 4);

        // One pop and one push: the tail entry's link plus the new one.
        let a = alloc.allocate().unwrap();
        alloc.free(a).unwrap();
        let before = ops(&store);
        alloc.sync().unwrap();
        assert_eq!(ops(&store) - before, 2 + 1 + 4);

        let reopened = PageAllocator::new(Arc::clone(&store)).unwrap();
        assert_eq!(reopened.snapshot(), alloc.snapshot());
    }

    #[derive(Clone, Copy)]
    enum Op {
        Alloc,
        Free(u64),
    }

    fn apply(alloc: &PageAllocator<Arc<CrashStore>>, ops: &[Op]) {
        for op in ops {
            match *op {
                Op::Alloc => {
                    let _ = alloc.allocate().unwrap();
                }
                Op::Free(id) => alloc.free(PageId::new(id)).unwrap(),
            }
        }
    }

    /// Every way a crashed disk can hold one page: the old version (or none),
    /// the new one, or a torn mix of the two halves.
    fn variants(old: Option<&Vec<u8>>, new: &[u8]) -> Vec<Option<Vec<u8>>> {
        let half = new.len() / 2;
        let zero = vec![0u8; new.len()];
        let base = old.map_or(&zero[..], |o| &o[..]);
        let mut first_new = base.to_vec();
        first_new[..half].copy_from_slice(&new[..half]);
        let mut second_new = base.to_vec();
        second_new[half..].copy_from_slice(&new[half..]);
        vec![
            old.cloned(),
            Some(new.to_vec()),
            Some(first_new),
            Some(second_new),
        ]
    }

    /// A crash at any point of a sync, with any mix of the writes in flight
    /// reaching the disk (each page old, new, or torn), reopens to the state of
    /// the previous sync, the state of this one, or an empty free-list under
    /// one of their high-water marks. It never fails to open and never brings
    /// back a free-list that differs from both.
    #[test]
    fn test_crash_during_sync_recovers_old_or_new_state() {
        let scenarios: Vec<(Vec<Op>, Vec<Op>)> = vec![
            (
                vec![Op::Alloc; 6]
                    .into_iter()
                    .chain([Op::Free(2), Op::Free(4)])
                    .collect(),
                vec![Op::Alloc, Op::Free(5), Op::Free(1)],
            ),
            (
                vec![Op::Alloc; 3],
                vec![Op::Free(1), Op::Free(2), Op::Free(3)],
            ),
            (
                vec![Op::Alloc; 4]
                    .into_iter()
                    .chain((1..=4).map(Op::Free))
                    .collect(),
                vec![Op::Alloc, Op::Alloc],
            ),
            (vec![], vec![Op::Alloc, Op::Alloc, Op::Alloc, Op::Free(2)]),
            (vec![Op::Alloc, Op::Free(1)], vec![]),
        ];

        for (first, second) in scenarios {
            // A dry run counts the writes and syncs the second sync makes.
            let total = {
                let store = Arc::new(CrashStore::new(PS));
                let alloc = PageAllocator::new(Arc::clone(&store)).unwrap();
                apply(&alloc, &first);
                alloc.sync().unwrap();
                apply(&alloc, &second);
                let before = store.ops.load(std::sync::atomic::Ordering::SeqCst);
                alloc.sync().unwrap();
                store.ops.load(std::sync::atomic::Ordering::SeqCst) - before
            };

            for budget in 0..=total {
                let store = Arc::new(CrashStore::new(PS));
                let alloc = PageAllocator::new(Arc::clone(&store)).unwrap();
                apply(&alloc, &first);
                alloc.sync().unwrap();
                let old = alloc.snapshot();
                apply(&alloc, &second);
                let new = alloc.snapshot();
                store.set_budget(Some(budget));
                let synced = alloc.sync().is_ok();
                assert_eq!(synced, budget == total);

                let durable = store.durable.lock().unwrap().clone();
                let mut latest: HashMap<u64, Vec<u8>> = HashMap::new();
                for (id, bytes) in store.pending.lock().unwrap().iter() {
                    let _ = latest.insert(*id, bytes.clone());
                }
                let pages: Vec<(u64, Vec<Option<Vec<u8>>>)> = latest
                    .iter()
                    .map(|(&id, bytes)| (id, variants(durable.get(&id), bytes)))
                    .collect();

                let combos: usize = pages.iter().map(|(_, v)| v.len()).product();
                for mut n in 0..combos {
                    let mut image = durable.clone();
                    for (id, choices) in &pages {
                        match &choices[n % choices.len()] {
                            Some(bytes) => {
                                let _ = image.insert(*id, bytes.clone());
                            }
                            None => {
                                let _ = image.remove(id);
                            }
                        }
                        n /= choices.len();
                    }
                    let reopened = PageAllocator::new(MemStore::from_pages(PS, image))
                        .unwrap_or_else(|e| panic!("budget {budget}: reopen failed: {e}"));
                    let got = reopened.snapshot();
                    let ok = got == old
                        || got == new
                        || (reopened.free_list_dropped()
                            && got.1.is_empty()
                            && (got.0 == old.0 || got.0 == new.0));
                    assert!(ok, "budget {budget}: got {got:?}, old {old:?}, new {new:?}");
                    if synced {
                        assert_eq!(got, new, "a completed sync must reopen to its state");
                    }
                }
            }
        }
    }

    #[test]
    fn test_from_state_validates_and_persists() {
        assert!(PageAllocator::from_state(MemStore::new(PS), 0, []).is_err());
        let bad = [PageId::new(4), PageId::new(4)];
        assert!(PageAllocator::from_state(MemStore::new(PS), 9, bad).is_err());

        let alloc =
            PageAllocator::from_state(MemStore::new(PS), 9, [PageId::new(2), PageId::new(7)])
                .unwrap();
        assert!(alloc.free(PageId::new(7)).is_err(), "7 is already free");
        alloc.sync().unwrap();
        let reopened = PageAllocator::new(alloc.into_store()).unwrap();
        assert_eq!(reopened.snapshot(), (9, vec![2, 7]));
    }

    #[test]
    fn test_torn_superblock_falls_back_to_the_other_copy() {
        let store = MemStore::new(PS);
        let alloc = PageAllocator::new(store).unwrap();
        for _ in 0..4 {
            let _ = alloc.allocate().unwrap();
        }
        alloc.free(PageId::new(3)).unwrap();
        alloc.sync().unwrap();
        let expect = alloc.snapshot();
        let store = alloc.into_store();
        let mut pages = sync::lock(&store.pages).clone();
        drop(store);

        for flip in [40, 64 + 9, PS / 2 + 3, PS - 1] {
            let mut image = pages.clone();
            image.get_mut(&0).unwrap()[flip] ^= 0x40;
            let reopened = PageAllocator::new(MemStore::from_pages(PS, image)).unwrap();
            assert_eq!(reopened.snapshot(), expect, "flip at {flip}");
            assert!(!reopened.free_list_dropped());
        }

        // With both copies damaged as well as the page, nothing is left.
        let page0 = pages.get_mut(&0).unwrap();
        page0[PAGE_HEADER_SIZE + REC_A_OFF + 9] ^= 1;
        page0[PAGE_HEADER_SIZE + rec_b_off(PS) + 9] ^= 1;
        assert!(matches!(
            PageAllocator::new(MemStore::from_pages(PS, pages)),
            Err(PageError::ChecksumMismatch { .. })
        ));
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

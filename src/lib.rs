//! # page-db
//!
//! The paging substrate beneath B-tree and heap storage engines: fixed-size
//! pages on disk, each carrying a versioned header with a CRC32C integrity
//! check and an LSN slot for write-ahead-log coordination, read and written
//! through cross-platform Direct I/O that bypasses the OS page cache.
//!
//! Three layers ship today. The [`PageFile`] is the durable foundation: an array
//! of fixed-size [`Page`]s addressed by [`PageId`], read and written through
//! Direct I/O, every read verified against its header and checksum. The
//! [`BufferPool`] sits on top: a bounded cache of frames with pinning and dirty
//! tracking, so hot pages stay resident and the engine above asks for a page by
//! id and gets back a pinned frame. The [`PageAllocator`] manages the id space:
//! it hands out unused ids and reclaims freed ones through an on-disk free-list.
//! Compose them over one shared store with `Arc` (see [`PageStore`]). The public
//! API is feature-frozen as of v0.4.0; see `dev/ROADMAP.md`.
//!
//! ## Straight to the file
//!
//! Every page carries a header; on write the header's checksum is stamped over
//! the page bytes, and on read it is verified before the page is handed back —
//! a corrupt or misdirected page is a typed [`PageError`], never a silent read.
//!
//! ```no_run
//! use page_db::{PageFile, PageId, Lsn};
//!
//! # fn main() -> Result<(), page_db::PageError> {
//! // A 4 KiB-page file, Direct I/O, created if absent.
//! let file = PageFile::open("data.pages", page_db::DEFAULT_PAGE_SIZE)?;
//!
//! // Allocate a fresh page, fill its payload, tag it with a log sequence number.
//! let mut page = file.allocate_page();
//! page.set_lsn(Lsn::new(1));
//! page.payload_mut()[..5].copy_from_slice(b"hello");
//!
//! // Write it to slot 0 and make it durable.
//! let id = PageId::new(0);
//! file.write_page(id, &mut page)?;
//! file.sync()?;
//!
//! // Read it back — the header and checksum are verified on the way out.
//! let read = file.read_page(id)?;
//! assert_eq!(&read.payload()[..5], b"hello");
//! assert_eq!(read.lsn(), Lsn::new(1));
//! # Ok(())
//! # }
//! ```
//!
//! ## Through the buffer pool
//!
//! ```no_run
//! use page_db::{BufferPool, PageId, Lsn, DEFAULT_PAGE_SIZE};
//!
//! # fn main() -> Result<(), page_db::PageError> {
//! let pool = BufferPool::open("data.pages", DEFAULT_PAGE_SIZE, 256)?;
//!
//! // Create a page; writing through the guard marks the frame dirty.
//! {
//!     let guard = pool.new_page(PageId::new(0))?;
//!     guard.write().set_lsn(Lsn::new(1));
//! }
//! pool.checkpoint()?;   // flush dirty frames, then make the file durable
//!
//! // Fetch it — served from cache, the page stays pinned for the guard's life.
//! let guard = pool.fetch(PageId::new(0))?;
//! assert_eq!(guard.read().lsn(), Lsn::new(1));
//! # Ok(())
//! # }
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unused_must_use)]
#![deny(unused_results)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod alloc;
mod buffer;
pub mod checksum;
mod error;
mod file;
mod page;
mod pool;
mod store;
mod sync;
mod sys;
#[cfg(test)]
mod test_store;

pub use crate::alloc::PageAllocator;
pub use crate::checksum::crc32c;
pub use crate::error::{PageError, PageResult};
pub use crate::file::{PageFile, PageFileOptions};
pub use crate::page::{
    DEFAULT_PAGE_SIZE, Lsn, MAX_PAGE_SIZE, MIN_PAGE_SIZE, PAGE_HEADER_SIZE, Page, PageId, PageSize,
};
pub use crate::pool::{BufferPool, PageGuard, PageMut, PageRef};
pub use crate::store::PageStore;

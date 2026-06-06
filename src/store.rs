//! The [`PageStore`] trait: the storage seam the buffer pool sits on.

use crate::error::PageResult;
use crate::page::{Page, PageId};

/// A backing store of fixed-size pages addressed by [`PageId`].
///
/// This is the extension seam between the buffer pool and what holds the pages.
/// [`PageFile`](crate::PageFile) is the implementation that matters — pages on
/// disk through Direct I/O — but the pool is written against this trait so it
/// can be driven by an in-memory store in tests, or by an alternative backend
/// later, without change.
///
/// A store does not cache, pin, or track dirtiness; those are the pool's job. A
/// store only has to read a page by id, write a page to an id, allocate a blank
/// page of the right size, and flush to durability.
///
/// # Examples
///
/// A [`PageFile`](crate::PageFile) is a `PageStore`, so it can back a pool:
///
/// ```
/// use page_db::{BufferPool, PageFile, PageId, DEFAULT_PAGE_SIZE};
///
/// # let dir = tempfile::tempdir().unwrap();
/// # let path = dir.path().join("data.pages");
/// let file = PageFile::open(&path, DEFAULT_PAGE_SIZE)?;
/// let pool = BufferPool::new(file, 64);   // 64 frames over the file
/// let _ = pool.new_page(PageId::new(0))?;
/// # Ok::<(), page_db::PageError>(())
/// ```
pub trait PageStore: Send + Sync {
    /// The fixed page size of this store, in bytes.
    fn page_size(&self) -> usize;

    /// Allocate a blank page sized for this store. The page is in memory only.
    fn allocate_page(&self) -> Page;

    /// Read the page at `id` into `page`, verifying it.
    ///
    /// Reusing the caller's buffer is what lets the pool recycle a frame on a
    /// miss without allocating.
    ///
    /// # Errors
    ///
    /// Implementation-defined; for a file this is a short read past end-of-file
    /// or a failed integrity check.
    fn read_into(&self, id: PageId, page: &mut Page) -> PageResult<()>;

    /// Write `page` to slot `id`. The page's header is stamped (id + checksum)
    /// as part of the write.
    ///
    /// # Errors
    ///
    /// Implementation-defined; for a file this is an I/O failure or a page-size
    /// mismatch.
    fn write_page(&self, id: PageId, page: &mut Page) -> PageResult<()>;

    /// Flush all written pages to stable storage.
    ///
    /// # Errors
    ///
    /// Implementation-defined; for a file this is an I/O failure.
    fn sync(&self) -> PageResult<()>;
}

impl<S: PageStore> PageStore for std::sync::Arc<S> {
    #[inline]
    fn page_size(&self) -> usize {
        (**self).page_size()
    }

    #[inline]
    fn allocate_page(&self) -> Page {
        (**self).allocate_page()
    }

    #[inline]
    fn read_into(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        (**self).read_into(id, page)
    }

    #[inline]
    fn write_page(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        (**self).write_page(id, page)
    }

    #[inline]
    fn sync(&self) -> PageResult<()> {
        (**self).sync()
    }
}

impl PageStore for crate::file::PageFile {
    #[inline]
    fn page_size(&self) -> usize {
        crate::file::PageFile::page_size(self)
    }

    #[inline]
    fn allocate_page(&self) -> Page {
        crate::file::PageFile::allocate_page(self)
    }

    #[inline]
    fn read_into(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        crate::file::PageFile::read_into(self, id, page)
    }

    #[inline]
    fn write_page(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        crate::file::PageFile::write_page(self, id, page)
    }

    #[inline]
    fn sync(&self) -> PageResult<()> {
        crate::file::PageFile::sync(self)
    }
}

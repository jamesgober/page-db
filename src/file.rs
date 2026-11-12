//! [`PageFile`]: a file of fixed-size pages, read and written through Direct I/O.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{PageError, PageResult, sync_poisoned};
use crate::page::{DEFAULT_PAGE_SIZE, Page, PageId, PageSize};
use crate::sys;

/// Options for opening a [`PageFile`].
///
/// Build with [`PageFileOptions::new`], adjust, then [`open`](PageFileOptions::open).
/// The defaults are a 4 KiB page size, Direct I/O enabled, and create-if-absent.
///
/// # Examples
///
/// ```
/// use page_db::{PageFileOptions, PageSize};
///
/// # let dir = tempfile::tempdir().unwrap();
/// # let path = dir.path().join("data.pages");
/// let file = PageFileOptions::new()
///     .page_size(PageSize::new(8192)?)
///     .direct_io(false)          // buffered, e.g. on a filesystem without O_DIRECT
///     .open(&path)?;
/// assert_eq!(file.page_size(), 8192);
/// # Ok::<(), page_db::PageError>(())
/// ```
#[derive(Debug, Clone)]
#[must_use = "PageFileOptions does nothing until `open` is called"]
pub struct PageFileOptions {
    page_size: PageSize,
    direct_io: bool,
    create: bool,
}

impl Default for PageFileOptions {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
            direct_io: true,
            create: true,
        }
    }
}

impl PageFileOptions {
    /// Start from the defaults: 4 KiB pages, Direct I/O on, create-if-absent.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the page size. Every page in the file is this size; it is fixed for
    /// the life of the file and the caller is responsible for reopening with the
    /// same size.
    pub fn page_size(mut self, page_size: PageSize) -> Self {
        self.page_size = page_size;
        self
    }

    /// Enable or disable Direct I/O (cache-bypass).
    ///
    /// Direct I/O is the default and the point of this crate. Disable it for a
    /// filesystem that does not support it (some network and overlay
    /// filesystems reject `O_DIRECT`); durability via [`PageFile::sync`] is
    /// unaffected, only the page cache is.
    pub fn direct_io(mut self, enabled: bool) -> Self {
        self.direct_io = enabled;
        self
    }

    /// Create the file if it does not exist (the default). When `false`, opening
    /// a missing file is an error.
    pub fn create(mut self, create: bool) -> Self {
        self.create = create;
        self
    }

    /// Open the page file at `path` with these options.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::Io`] if the file cannot be opened (including a
    /// filesystem that rejects Direct I/O, surfaced as the OS error).
    pub fn open<P: AsRef<Path>>(self, path: P) -> PageResult<PageFile> {
        let path = path.as_ref();
        let (file, created) = sys::open(path, self.direct_io, self.create)?;
        Ok(PageFile {
            file,
            page_size: self.page_size,
            sync_failed: AtomicBool::new(false),
            new_entry: Mutex::new(created.then(|| path.to_path_buf())),
        })
    }
}

/// A file of fixed-size pages.
///
/// A `PageFile` is an array of [`Page`]s on disk, addressed by [`PageId`]: page
/// `n` occupies the byte range `n * page_size .. (n + 1) * page_size`. Reads and
/// writes are positioned and take `&self`, so the handle is shared freely across
/// threads — there is no shared file cursor to contend on. (The cache that will
/// front these reads is a later release; today every read goes to disk.)
///
/// Durability is two steps, deliberately: [`write_page`](PageFile::write_page)
/// places bytes, and [`sync`](PageFile::sync) makes them durable. Batch many
/// writes, then sync once. When `open` creates the file, the first successful
/// `sync` also flushes the parent directory on Unix, so the file's name is as
/// durable as its pages. A failed `sync` is permanent for the handle: every
/// later `sync` fails too, because the pages it covered may already be gone from
/// the OS cache. Reopen the file and recover from the log.
///
/// # Examples
///
/// ```
/// use page_db::{PageFile, PageFileOptions, PageId, Lsn};
///
/// # let dir = tempfile::tempdir().unwrap();
/// # let path = dir.path().join("data.pages");
/// let file = PageFileOptions::new().direct_io(false).open(&path)?;
///
/// let mut page = file.allocate_page();
/// page.set_lsn(Lsn::new(1));
/// page.payload_mut()[..3].copy_from_slice(b"abc");
/// file.write_page(PageId::new(0), &mut page)?;
/// file.sync()?;
///
/// let got = file.read_page(PageId::new(0))?;
/// assert_eq!(&got.payload()[..3], b"abc");
/// assert_eq!(file.page_count()?, 1);
/// # Ok::<(), page_db::PageError>(())
/// ```
#[derive(Debug)]
pub struct PageFile {
    file: File,
    page_size: PageSize,
    /// Set once a sync has failed; every later sync then fails as well.
    sync_failed: AtomicBool,
    /// The path of a file this handle created, until the first successful sync
    /// has made its directory entry durable.
    new_entry: Mutex<Option<PathBuf>>,
}

impl PageFile {
    /// Open a page file at `path` with the given page size and the default
    /// options (Direct I/O on, create-if-absent).
    ///
    /// For buffered I/O or other tuning, use [`PageFileOptions`].
    ///
    /// # Errors
    ///
    /// Returns [`PageError::Io`] if the file cannot be opened.
    pub fn open<P: AsRef<Path>>(path: P, page_size: PageSize) -> PageResult<Self> {
        PageFileOptions::new().page_size(page_size).open(path)
    }

    /// The page size of this file, in bytes.
    #[inline]
    #[must_use]
    pub fn page_size(&self) -> usize {
        self.page_size.get()
    }

    /// The number of whole pages currently in the file.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::Io`] if the file metadata cannot be read.
    pub fn page_count(&self) -> PageResult<u64> {
        let len = self.file.metadata()?.len();
        Ok(len / self.page_size.get() as u64)
    }

    /// Allocate a fresh, zeroed page sized and aligned for this file.
    ///
    /// The page is in memory only; write it with
    /// [`write_page`](PageFile::write_page) to place it in a slot.
    #[must_use]
    pub fn allocate_page(&self) -> Page {
        Page::new(self.page_size)
    }

    /// The byte offset of slot `id`, or [`PageError::InvalidPageId`] if the slot
    /// lies beyond what a 64-bit file offset can address.
    #[inline]
    fn offset_of(&self, id: PageId) -> PageResult<u64> {
        id.byte_offset(self.page_size.get())
            .ok_or(PageError::InvalidPageId { page_id: id.get() })
    }

    /// Read the page at slot `id`, verifying its header and checksum.
    ///
    /// The page's magic, version, and CRC32C are checked, and its stamped id is
    /// matched against `id`, before it is returned — so a corrupt or misdirected
    /// page surfaces as an error rather than bad data.
    ///
    /// # Errors
    ///
    /// - [`PageError::ShortRead`] if the slot is past the end of the file.
    /// - [`PageError::BadMagic`] / [`PageError::UnsupportedVersion`] /
    ///   [`PageError::ChecksumMismatch`] / [`PageError::MisdirectedPage`] if the
    ///   page fails validation.
    /// - [`PageError::InvalidPageId`] if the slot's byte offset does not fit in
    ///   a 64-bit file offset.
    /// - [`PageError::Io`] on an I/O failure.
    pub fn read_page(&self, id: PageId) -> PageResult<Page> {
        let mut page = Page::new(self.page_size);
        self.read_into(id, &mut page)?;
        Ok(page)
    }

    /// Read the page at slot `id` into an existing buffer, verifying it.
    ///
    /// This is the zero-allocation form of [`read_page`](PageFile::read_page):
    /// it reuses `page`'s buffer instead of allocating a fresh one, which is how
    /// a buffer pool recycles a frame on a cache miss. `page.page_size()` must
    /// match the file's.
    ///
    /// # Errors
    ///
    /// As [`read_page`](PageFile::read_page), plus
    /// [`PageError::InvalidPageSize`] if `page`'s size does not match the file's.
    /// When validation fails, `page` still holds the bytes that were read.
    pub fn read_into(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        if page.page_size() != self.page_size.get() {
            return Err(PageError::InvalidPageSize {
                size: page.page_size(),
            });
        }
        let offset = self.offset_of(id)?;
        let got = sys::read_at_full(&self.file, page.as_bytes_mut(), offset)?;
        if got != self.page_size.get() {
            return Err(PageError::ShortRead {
                page_id: id.get(),
                got,
                page_size: self.page_size.get(),
            });
        }
        page.verify(Some(id))?;
        Ok(())
    }

    /// Write `page` to slot `id`, stamping the slot id and a fresh checksum.
    ///
    /// The page's id and checksum header fields are updated in place, so the
    /// same page can be written, mutated, and written again. The write places
    /// the bytes; call [`sync`](PageFile::sync) to make them durable.
    ///
    /// # Errors
    ///
    /// - [`PageError::InvalidPageSize`] if the page's size does not match the
    ///   file's.
    /// - [`PageError::InvalidPageId`] if the slot's byte offset does not fit in
    ///   a 64-bit file offset. Nothing is written.
    /// - [`PageError::Io`] on an I/O failure.
    pub fn write_page(&self, id: PageId, page: &mut Page) -> PageResult<()> {
        if page.page_size() != self.page_size.get() {
            return Err(PageError::InvalidPageSize {
                size: page.page_size(),
            });
        }
        let offset = self.offset_of(id)?;
        page.stamp(id);
        sys::write_all_at(&self.file, page.as_bytes(), offset)?;
        Ok(())
    }

    /// Flush all written pages to stable storage.
    ///
    /// Returns once the data is durable: `fdatasync` on Linux,
    /// `FlushFileBuffers` on Windows, `F_FULLFSYNC` on macOS. If this handle
    /// created the file, the first successful sync also flushes the parent
    /// directory on Unix so the file's name survives a power loss.
    ///
    /// A failed sync is sticky: the OS may already have dropped the pages it
    /// could not write, so a retry could report success without them. After the
    /// first failure every later call returns an error; reopen the file and
    /// recover from the log.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::Io`] if the flush fails, or if an earlier sync on
    /// this handle failed.
    pub fn sync(&self) -> PageResult<()> {
        if self.sync_failed.load(Ordering::Acquire) {
            return Err(sync_poisoned());
        }
        if let Err(err) = sys::sync_data(&self.file) {
            self.sync_failed.store(true, Ordering::Release);
            return Err(err.into());
        }
        let mut entry = match self.new_entry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(path) = entry.as_deref() {
            sys::sync_dir(path)?;
            *entry = None;
        }
        Ok(())
    }

    /// Mark this handle as having failed a sync, for tests of the sticky path.
    #[cfg(test)]
    pub(crate) fn poison_sync(&self) {
        self.sync_failed.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::page::Lsn;

    fn temp_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.pages");
        (dir, path)
    }

    fn open_buffered(path: &Path) -> PageFile {
        PageFileOptions::new()
            .direct_io(false)
            .open(path)
            .expect("open")
    }

    #[test]
    fn test_write_read_roundtrip() {
        let (_dir, path) = temp_file();
        let file = open_buffered(&path);

        let mut page = file.allocate_page();
        page.set_lsn(Lsn::new(11));
        page.payload_mut()[..5].copy_from_slice(b"world");
        file.write_page(PageId::new(2), &mut page).expect("write");
        file.sync().expect("sync");

        let got = file.read_page(PageId::new(2)).expect("read");
        assert_eq!(got.id(), PageId::new(2));
        assert_eq!(got.lsn(), Lsn::new(11));
        assert_eq!(&got.payload()[..5], b"world");
    }

    #[test]
    fn test_read_past_end_is_short_read() {
        let (_dir, path) = temp_file();
        let file = open_buffered(&path);
        assert!(matches!(
            file.read_page(PageId::new(0)),
            Err(PageError::ShortRead { .. })
        ));
    }

    #[test]
    fn test_page_count_tracks_writes() {
        let (_dir, path) = temp_file();
        let file = open_buffered(&path);
        assert_eq!(file.page_count().expect("count"), 0);

        let mut page = file.allocate_page();
        file.write_page(PageId::new(0), &mut page).expect("write");
        assert_eq!(file.page_count().expect("count"), 1);

        file.write_page(PageId::new(4), &mut page).expect("write");
        assert_eq!(file.page_count().expect("count"), 5);
    }

    #[test]
    fn test_corruption_on_disk_is_detected() {
        let (_dir, path) = temp_file();
        {
            let file = open_buffered(&path);
            let mut page = file.allocate_page();
            page.payload_mut()[0] = 0x42;
            file.write_page(PageId::new(0), &mut page).expect("write");
            file.sync().expect("sync");
        }
        // Flip a payload byte directly in the file, past the 32-byte header.
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut raw = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .expect("reopen");
            let _ = raw.seek(SeekFrom::Start(40)).expect("seek");
            let mut b = [0u8; 1];
            let _ = raw.read_exact(&mut b);
            b[0] ^= 0xFF;
            let _ = raw.seek(SeekFrom::Start(40)).expect("seek");
            raw.write_all(&b).expect("write");
            raw.sync_all().expect("sync");
        }
        let file = open_buffered(&path);
        assert!(matches!(
            file.read_page(PageId::new(0)),
            Err(PageError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn test_failed_sync_is_sticky() {
        let (_dir, path) = temp_file();
        let file = open_buffered(&path);
        file.sync().expect("sync");
        file.poison_sync();
        assert!(matches!(file.sync(), Err(PageError::Io(_))));
        assert!(matches!(file.sync(), Err(PageError::Io(_))));
        // A fresh handle starts clean.
        drop(file);
        open_buffered(&path).sync().expect("sync after reopen");
    }

    #[test]
    fn test_new_file_entry_is_flushed_once() {
        let (_dir, path) = temp_file();
        let file = open_buffered(&path);
        assert!(file.new_entry.lock().expect("lock").is_some());
        file.sync().expect("sync");
        assert!(file.new_entry.lock().expect("lock").is_none());
        drop(file);
        // Reopening an existing file has no directory entry to flush.
        let file = open_buffered(&path);
        assert!(file.new_entry.lock().expect("lock").is_none());
    }

    #[test]
    fn test_huge_id_is_invalid_not_wrapped() {
        let (_dir, path) = temp_file();
        let file = open_buffered(&path);
        let mut page = file.allocate_page();
        let id = PageId::new(1 << 52);
        assert!(matches!(
            file.write_page(id, &mut page),
            Err(PageError::InvalidPageId { .. })
        ));
        assert_eq!(file.page_count().expect("count"), 0);
    }

    #[test]
    fn test_write_rejects_wrong_page_size() {
        let (_dir, path) = temp_file();
        let file = open_buffered(&path);
        let mut wrong = Page::new(PageSize::new(8192).expect("valid"));
        assert!(matches!(
            file.write_page(PageId::new(0), &mut wrong),
            Err(PageError::InvalidPageSize { size: 8192 })
        ));
    }
}

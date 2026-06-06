//! The crate error type.

use std::io;

/// A convenience alias for results returned by this crate.
pub type PageResult<T> = Result<T, PageError>;

/// Everything that can go wrong reading, writing, or framing a page.
///
/// The variants split into two families: I/O failures from the underlying file
/// ([`PageError::Io`]) and integrity failures detected while validating a page
/// against its header. The latter are the interesting ones — they are how a
/// torn write, a bit-rotted block, or a misdirected read surfaces as a value
/// instead of silently corrupting the layer above.
///
/// The type is `#[non_exhaustive]`: later releases may add variants (for
/// example as the allocator and buffer pool land), so match with a wildcard
/// arm.
///
/// # Examples
///
/// ```
/// use page_db::PageError;
///
/// // I/O errors convert in with `?` via the `From<std::io::Error>` impl.
/// fn classify(err: &PageError) -> &'static str {
///     match err {
///         PageError::Io(_) => "io",
///         PageError::ChecksumMismatch { .. } => "corruption",
///         _ => "other",
///     }
/// }
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PageError {
    /// An I/O operation on the underlying file failed.
    ///
    /// The source [`std::io::Error`] is preserved, so callers that need the OS
    /// error kind (for example to distinguish [`io::ErrorKind::NotFound`] on
    /// open) can reach it through [`std::error::Error::source`] or by matching
    /// this variant directly.
    #[error("page i/o error: {0}")]
    Io(#[from] io::Error),

    /// A requested page size is not a power of two within
    /// [`MIN_PAGE_SIZE`](crate::MIN_PAGE_SIZE)..=[`MAX_PAGE_SIZE`](crate::MAX_PAGE_SIZE).
    #[error(
        "invalid page size {size}: must be a power of two in {min}..={max}",
        min = crate::MIN_PAGE_SIZE,
        max = crate::MAX_PAGE_SIZE
    )]
    InvalidPageSize {
        /// The rejected size, in bytes.
        size: usize,
    },

    /// The page's leading magic bytes did not match. The block is not a
    /// page-db page, or its first bytes are corrupt.
    #[error("bad page magic: found {found:#010x}, expected {expected:#010x}")]
    BadMagic {
        /// The magic value read from disk.
        found: u32,
        /// The magic value this build writes.
        expected: u32,
    },

    /// The page's format version is newer than this build understands.
    #[error("unsupported page format version {found} (this build writes {supported})")]
    UnsupportedVersion {
        /// The version read from disk.
        found: u16,
        /// The version this build writes and can read.
        supported: u16,
    },

    /// The page's stored CRC32C did not match the checksum recomputed over its
    /// bytes. The page is corrupt — a torn write, bit rot, or a wrong byte.
    #[error(
        "checksum mismatch on page {page_id}: stored {stored:#010x}, computed {computed:#010x}"
    )]
    ChecksumMismatch {
        /// The id of the slot that was read.
        page_id: u64,
        /// The checksum stored in the page header.
        stored: u32,
        /// The checksum recomputed over the page on read.
        computed: u32,
    },

    /// The page read back from a slot carries a different id than the one
    /// requested. This catches a misdirected read or write — the file handed
    /// back the wrong block.
    #[error("misdirected page: slot {requested} holds a page stamped {found}")]
    MisdirectedPage {
        /// The slot id that was read.
        requested: u64,
        /// The id stamped in the page header found there.
        found: u64,
    },

    /// A read returned fewer bytes than a whole page. The slot is past the end
    /// of the file, or the file's length is not a whole number of pages.
    #[error("short read on page {page_id}: got {got} of {page_size} bytes")]
    ShortRead {
        /// The slot id that was read.
        page_id: u64,
        /// The number of bytes actually read.
        got: usize,
        /// The configured page size.
        page_size: usize,
    },

    /// A buffer pool could not admit another page because every frame is
    /// pinned. The pool refuses to evict a pinned page, so this is the signal to
    /// release some pins or size the pool larger.
    #[error("buffer pool exhausted: all {capacity} frames are pinned")]
    BufferPoolExhausted {
        /// The pool's frame capacity.
        capacity: usize,
    },

    /// An id handed to the allocator's `free` is not one it could have
    /// allocated: it is the reserved superblock (page 0), or it is beyond the
    /// high-water mark of pages ever allocated.
    #[error("invalid page id to free: {page_id}")]
    InvalidPageId {
        /// The rejected id.
        page_id: u64,
    },

    /// The file's page 0 is not a valid allocator superblock. The file was not
    /// initialized by the allocator, or its superblock is corrupt.
    #[error("page 0 is not a valid allocator superblock")]
    InvalidSuperblock,
}

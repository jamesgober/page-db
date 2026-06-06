//! The page format: identifiers, sizes, the on-disk header, and [`Page`].
//!
//! A page is a fixed-size block — `page_size` bytes — laid out as a 32-byte
//! header followed by the payload the layer above is free to use. The header is
//! little-endian on disk so a file is portable across architectures:
//!
//! | offset | size | field      | meaning                                      |
//! |-------:|-----:|------------|----------------------------------------------|
//! | 0      | 4    | magic      | `b"PGDB"`, identifies a page-db page          |
//! | 4      | 2    | version    | header format version (currently 1)          |
//! | 6      | 2    | flags      | reserved, written as 0                        |
//! | 8      | 8    | page id    | the slot this page belongs to                 |
//! | 16     | 8    | lsn        | write-ahead-log sequence number               |
//! | 24     | 4    | crc32c     | checksum over the whole page, this field zero |
//! | 28     | 4    | reserved   | reserved, written as 0                        |
//!
//! The checksum covers every byte of the page except its own four bytes, so it
//! protects the header and the payload together. A page is never trusted
//! without recomputing and matching that checksum first.

use crate::buffer::AlignedBuffer;
use crate::checksum::Crc32c;
use crate::error::{PageError, PageResult};

/// The smallest accepted page size, in bytes.
///
/// Below 4 KiB a page no longer reliably satisfies Direct I/O block alignment
/// on common 4 KiB-sector devices, so it is the floor.
pub const MIN_PAGE_SIZE: usize = 4096;

/// The largest accepted page size, in bytes.
pub const MAX_PAGE_SIZE: usize = 1 << 20;

/// The size of the page header, in bytes. The usable payload of a page is
/// `page_size - PAGE_HEADER_SIZE`.
pub const PAGE_HEADER_SIZE: usize = 32;

/// The default page size (4 KiB), matching the common OS and device page size.
pub const DEFAULT_PAGE_SIZE: PageSize = PageSize(4096);

const MAGIC: u32 = u32::from_le_bytes([b'P', b'G', b'D', b'B']);
const FORMAT_VERSION: u16 = 1;

const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_PAGE_ID: usize = 8;
const OFF_LSN: usize = 16;
const OFF_CRC: usize = 24;

/// The id of a page within a [`PageFile`](crate::PageFile) — its slot index.
///
/// Page ids are dense from zero: page `n` lives at byte offset `n * page_size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PageId(u64);

impl PageId {
    /// Wrap a raw slot index.
    #[inline]
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw slot index.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The byte offset of this page in a file of the given page size.
    #[inline]
    #[must_use]
    pub(crate) const fn byte_offset(self, page_size: usize) -> u64 {
        self.0 * page_size as u64
    }
}

impl std::fmt::Display for PageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A write-ahead-log sequence number stamped into a page header.
///
/// page-db does not interpret the LSN; it carries the value so that a log
/// (`wal-db`) and the recovery code above can order a page against the log
/// records that describe it. [`Lsn::ZERO`] marks a page that has never been
/// associated with a log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Lsn(u64);

impl Lsn {
    /// The sentinel for "no log record" — a page that has not been logged.
    pub const ZERO: Lsn = Lsn(0);

    /// Wrap a raw sequence number.
    #[inline]
    #[must_use]
    pub const fn new(lsn: u64) -> Self {
        Self(lsn)
    }

    /// The raw sequence number.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A validated page size.
///
/// A page size must be a power of two within
/// [`MIN_PAGE_SIZE`]..=[`MAX_PAGE_SIZE`]. Validating once, here, means the rest
/// of the crate can treat the size as a trusted invariant — buffer alignment,
/// offset arithmetic, and the payload length all rely on it.
///
/// # Examples
///
/// ```
/// use page_db::PageSize;
///
/// assert!(PageSize::new(8192).is_ok());
/// assert!(PageSize::new(4096).is_ok());
/// assert!(PageSize::new(5000).is_err());   // not a power of two
/// assert!(PageSize::new(1024).is_err());   // below the 4 KiB floor
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageSize(usize);

impl PageSize {
    /// Validate and wrap a page size in bytes.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::InvalidPageSize`] if `size` is not a power of two,
    /// or falls outside [`MIN_PAGE_SIZE`]..=[`MAX_PAGE_SIZE`].
    pub const fn new(size: usize) -> PageResult<Self> {
        if size < MIN_PAGE_SIZE || size > MAX_PAGE_SIZE || !size.is_power_of_two() {
            return Err(PageError::InvalidPageSize { size });
        }
        Ok(Self(size))
    }

    /// The page size in bytes.
    #[inline]
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// The usable payload length of a page of this size.
    #[inline]
    #[must_use]
    pub const fn payload_len(self) -> usize {
        self.0 - PAGE_HEADER_SIZE
    }
}

impl Default for PageSize {
    #[inline]
    fn default() -> Self {
        DEFAULT_PAGE_SIZE
    }
}

/// A single fixed-size page: a header and a payload in one aligned buffer.
///
/// A `Page` owns a buffer aligned for Direct I/O, so it can be read into and
/// written from a [`PageFile`](crate::PageFile) without an intermediate copy.
/// Build one with [`Page::new`] (an empty page) or get one back from
/// [`PageFile::read_page`](crate::PageFile::read_page) /
/// [`PageFile::allocate_page`](crate::PageFile::allocate_page).
///
/// The checksum is not maintained on every mutation — it would be wasted work
/// to rechecksum after each `set_lsn` or payload write. Instead the page is
/// checksummed once, when it is written
/// ([`PageFile::write_page`](crate::PageFile::write_page) stamps it), or on
/// demand via [`Page::from_bytes`], which verifies as it loads.
///
/// # Examples
///
/// ```
/// use page_db::{Page, PageSize, Lsn};
///
/// let mut page = Page::new(PageSize::new(4096)?);
/// page.set_lsn(Lsn::new(42));
/// page.payload_mut()[..3].copy_from_slice(b"abc");
///
/// // Serialize to a checksummed byte block and load it back, verified.
/// let bytes = page.to_checksummed_bytes();
/// let loaded = Page::from_bytes(PageSize::new(4096)?, &bytes)?;
/// assert_eq!(loaded.lsn(), Lsn::new(42));
/// assert_eq!(&loaded.payload()[..3], b"abc");
/// # Ok::<(), page_db::PageError>(())
/// ```
pub struct Page {
    buf: AlignedBuffer,
    size: usize,
}

impl Page {
    /// Create an empty, zeroed page of the given size with a valid header.
    #[must_use]
    pub fn new(page_size: PageSize) -> Self {
        let size = page_size.get();
        let mut buf = AlignedBuffer::new_zeroed(size, size);
        {
            let bytes = buf.as_mut_slice();
            write_u32(bytes, OFF_MAGIC, MAGIC);
            write_u16(bytes, OFF_VERSION, FORMAT_VERSION);
        }
        Self { buf, size }
    }

    /// Load a page from a byte block, verifying its header and checksum.
    ///
    /// The block must be exactly `page_size` bytes. This is the inverse of
    /// [`Page::to_checksummed_bytes`] and the same validation
    /// [`PageFile::read_page`](crate::PageFile::read_page) performs after a read.
    ///
    /// # Errors
    ///
    /// - [`PageError::ShortRead`] if `bytes.len()` is not `page_size`.
    /// - [`PageError::BadMagic`] / [`PageError::UnsupportedVersion`] if the
    ///   header is not a page-db page this build understands.
    /// - [`PageError::ChecksumMismatch`] if the checksum does not match.
    pub fn from_bytes(page_size: PageSize, bytes: &[u8]) -> PageResult<Self> {
        let size = page_size.get();
        if bytes.len() != size {
            return Err(PageError::ShortRead {
                page_id: 0,
                got: bytes.len(),
                page_size: size,
            });
        }
        let mut buf = AlignedBuffer::new_zeroed(size, size);
        buf.as_mut_slice().copy_from_slice(bytes);
        let page = Self { buf, size };
        page.verify(None)?;
        Ok(page)
    }

    /// The page size in bytes.
    #[inline]
    #[must_use]
    pub fn page_size(&self) -> usize {
        self.size
    }

    /// The id stamped in the header. For a page from [`Page::new`] this is `0`
    /// until the page is written to a slot.
    #[inline]
    #[must_use]
    pub fn id(&self) -> PageId {
        PageId(read_u64(self.buf.as_slice(), OFF_PAGE_ID))
    }

    /// The log sequence number stamped in the header.
    #[inline]
    #[must_use]
    pub fn lsn(&self) -> Lsn {
        Lsn(read_u64(self.buf.as_slice(), OFF_LSN))
    }

    /// Set the log sequence number. Takes effect in the checksum the next time
    /// the page is stamped (on [`PageFile::write_page`](crate::PageFile::write_page)).
    #[inline]
    pub fn set_lsn(&mut self, lsn: Lsn) {
        write_u64(self.buf.as_mut_slice(), OFF_LSN, lsn.0);
    }

    /// The payload — the page bytes after the header.
    #[inline]
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.buf.as_slice()[PAGE_HEADER_SIZE..]
    }

    /// The payload, mutably.
    #[inline]
    pub fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.buf.as_mut_slice()[PAGE_HEADER_SIZE..]
    }

    /// The whole page as a checksummed byte block, ready to persist elsewhere.
    ///
    /// The returned vector is `page_size` bytes with a freshly computed checksum
    /// in the header; feed it back through [`Page::from_bytes`] to recover and
    /// verify the page. The stamped id is left untouched (`0` unless the page
    /// came from a file).
    #[must_use]
    pub fn to_checksummed_bytes(&self) -> Vec<u8> {
        let mut out = self.buf.as_slice().to_vec();
        let crc = compute_checksum(&out);
        write_u32(&mut out, OFF_CRC, crc);
        out
    }

    /// Stamp the slot id into the header and recompute the checksum.
    pub(crate) fn stamp(&mut self, id: PageId) {
        {
            let bytes = self.buf.as_mut_slice();
            write_u32(bytes, OFF_MAGIC, MAGIC);
            write_u16(bytes, OFF_VERSION, FORMAT_VERSION);
            write_u64(bytes, OFF_PAGE_ID, id.0);
        }
        let crc = compute_checksum(self.buf.as_slice());
        write_u32(self.buf.as_mut_slice(), OFF_CRC, crc);
    }

    /// Verify magic, version, checksum, and — if `expected` is set — the slot id.
    pub(crate) fn verify(&self, expected: Option<PageId>) -> PageResult<()> {
        let bytes = self.buf.as_slice();

        let magic = read_u32(bytes, OFF_MAGIC);
        if magic != MAGIC {
            return Err(PageError::BadMagic {
                found: magic,
                expected: MAGIC,
            });
        }

        let version = read_u16(bytes, OFF_VERSION);
        if version != FORMAT_VERSION {
            return Err(PageError::UnsupportedVersion {
                found: version,
                supported: FORMAT_VERSION,
            });
        }

        let stored = read_u32(bytes, OFF_CRC);
        let computed = compute_checksum(bytes);
        if stored != computed {
            return Err(PageError::ChecksumMismatch {
                page_id: read_u64(bytes, OFF_PAGE_ID),
                stored,
                computed,
            });
        }

        if let Some(expected) = expected {
            let found = read_u64(bytes, OFF_PAGE_ID);
            if found != expected.0 {
                return Err(PageError::MisdirectedPage {
                    requested: expected.0,
                    found,
                });
            }
        }

        Ok(())
    }

    /// The whole page buffer, for positioned I/O.
    #[inline]
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.buf.as_slice()
    }

    /// The whole page buffer, mutably, for reading into.
    #[inline]
    pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8] {
        self.buf.as_mut_slice()
    }
}

impl Clone for Page {
    fn clone(&self) -> Self {
        Self {
            buf: self.buf.clone(),
            size: self.size,
        }
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("id", &self.id())
            .field("lsn", &self.lsn())
            .field("page_size", &self.size)
            .finish()
    }
}

/// Checksum the whole page, treating the four checksum bytes as absent.
fn compute_checksum(bytes: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(&bytes[..OFF_CRC]);
    crc.update(&bytes[OFF_CRC + 4..]);
    crc.finalize()
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn test_page_size_rejects_non_power_of_two() {
        assert!(matches!(
            PageSize::new(5000),
            Err(PageError::InvalidPageSize { size: 5000 })
        ));
    }

    #[test]
    fn test_page_size_rejects_out_of_range() {
        assert!(PageSize::new(2048).is_err());
        assert!(PageSize::new(MAX_PAGE_SIZE * 2).is_err());
    }

    #[test]
    fn test_page_size_payload_len() {
        let ps = PageSize::new(4096).expect("valid");
        assert_eq!(ps.payload_len(), 4096 - PAGE_HEADER_SIZE);
    }

    #[test]
    fn test_new_page_has_valid_header() {
        let page = Page::new(DEFAULT_PAGE_SIZE);
        assert_eq!(page.id(), PageId::new(0));
        assert_eq!(page.lsn(), Lsn::ZERO);
        assert_eq!(page.page_size(), 4096);
    }

    #[test]
    fn test_stamp_then_verify_roundtrips() {
        let mut page = Page::new(DEFAULT_PAGE_SIZE);
        page.set_lsn(Lsn::new(7));
        page.payload_mut()[..4].copy_from_slice(b"data");
        page.stamp(PageId::new(3));

        assert_eq!(page.id(), PageId::new(3));
        page.verify(Some(PageId::new(3))).expect("verifies");
        assert_eq!(page.lsn(), Lsn::new(7));
    }

    #[test]
    fn test_verify_detects_corruption() {
        let mut page = Page::new(DEFAULT_PAGE_SIZE);
        page.stamp(PageId::new(1));
        page.payload_mut()[10] ^= 0xFF;
        assert!(matches!(
            page.verify(Some(PageId::new(1))),
            Err(PageError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn test_verify_detects_misdirected_page() {
        let mut page = Page::new(DEFAULT_PAGE_SIZE);
        page.stamp(PageId::new(5));
        assert!(matches!(
            page.verify(Some(PageId::new(6))),
            Err(PageError::MisdirectedPage {
                requested: 6,
                found: 5
            })
        ));
    }

    #[test]
    fn test_from_bytes_rejects_wrong_length() {
        let bytes = vec![0u8; 100];
        assert!(matches!(
            Page::from_bytes(DEFAULT_PAGE_SIZE, &bytes),
            Err(PageError::ShortRead { .. })
        ));
    }

    #[test]
    fn test_from_bytes_rejects_bad_magic() {
        let bytes = vec![0u8; 4096];
        assert!(matches!(
            Page::from_bytes(DEFAULT_PAGE_SIZE, &bytes),
            Err(PageError::BadMagic { .. })
        ));
    }

    #[test]
    fn test_to_bytes_from_bytes_roundtrips() {
        let mut page = Page::new(DEFAULT_PAGE_SIZE);
        page.set_lsn(Lsn::new(99));
        page.payload_mut()[..5].copy_from_slice(b"hello");
        let bytes = page.to_checksummed_bytes();

        let loaded = Page::from_bytes(DEFAULT_PAGE_SIZE, &bytes).expect("verifies");
        assert_eq!(loaded.lsn(), Lsn::new(99));
        assert_eq!(&loaded.payload()[..5], b"hello");
    }
}

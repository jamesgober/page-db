<h1 align="center">
        <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>page-db</b><br>
    <sub><sup>API REFERENCE</sup></sub>
</h1>
<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>API</span>
        <span>&nbsp;│&nbsp;</span>
        <a href="../CHANGELOG.md" title="Changelog"><b>CHANGELOG</b></a>
    </sup>
</div>
<br>

> Complete reference for every public item in `page-db`, with examples.
> **Status: pre-1.0.** The surface below is what ships in `v0.2.0` — the page
> format and the Direct I/O file. The LRU buffer pool, pinning, and the page
> allocator land across the rest of the 0.x series (see
> [`dev/ROADMAP.md`](../dev/ROADMAP.md)); the public API is frozen at `1.0.0`.

## Table of Contents

- [Installation](#installation)
- [Quick Start](#quick-start)
- [Concepts](#concepts)
- [Public API](#public-api)
  - [Constants](#constants)
  - [`PageSize`](#pagesize)
  - [`PageId`](#pageid)
  - [`Lsn`](#lsn)
  - [`Page`](#page)
  - [`PageFile`](#pagefile)
  - [`PageFileOptions`](#pagefileoptions)
  - [`PageError` & `PageResult`](#pageerror--pageresult)
  - [`checksum` — CRC32C](#checksum--crc32c)
- [Feature Flags](#feature-flags)
- [Notes](#notes)

<br>

## Installation

```toml
[dependencies]
page-db = "0.2"
```

With serde derives on the small value types:

```toml
[dependencies]
page-db = { version = "0.2", features = ["serde"] }
```

MSRV: Rust 1.85 (2024 edition).

<br>

## Quick Start

```rust
use page_db::{PageFile, PageId, Lsn, DEFAULT_PAGE_SIZE};

# fn main() -> Result<(), page_db::PageError> {
# let dir = tempfile::tempdir().unwrap();
# let path = dir.path().join("data.pages");
// Open a 4 KiB-page file (Direct I/O, created if absent).
let file = PageFile::open(&path, DEFAULT_PAGE_SIZE)?;

// Fill a page, tag it with a log sequence number, write it to slot 0.
let mut page = file.allocate_page();
page.set_lsn(Lsn::new(1));
page.payload_mut()[..5].copy_from_slice(b"hello");
file.write_page(PageId::new(0), &mut page)?;
file.sync()?;

// Read it back — the header and checksum are verified on the way out.
let got = file.read_page(PageId::new(0))?;
assert_eq!(&got.payload()[..5], b"hello");
assert_eq!(got.lsn(), Lsn::new(1));
# Ok(())
# }
```

<br>

## Concepts

A **page** is a fixed-size block of bytes — `page_size` bytes — made of a
32-byte header followed by a payload the layer above owns. The header carries
a magic number, a format version, the page's id, an [`Lsn`](#lsn) for
write-ahead-log coordination, and a CRC32C checksum over the whole page.

A [`PageFile`](#pagefile) is an array of pages on disk addressed by
[`PageId`](#pageid): page `n` occupies bytes `n * page_size .. (n+1) * page_size`.
Reads and writes go through **Direct I/O**, bypassing the OS page cache, into
buffers aligned to the page size. Every read verifies the header and checksum
before returning — a corrupt or misdirected page is a typed
[`PageError`](#pageerror--pageresult), never silent bad data.

Durability is two explicit steps: [`write_page`](#pagefile) places bytes,
[`sync`](#pagefile) makes them durable. Write many pages, then sync once.

<br>

## Public API

### Constants

| Constant | Type | Value | Meaning |
|----------|------|-------|---------|
| `MIN_PAGE_SIZE` | `usize` | `4096` | Smallest accepted page size. |
| `MAX_PAGE_SIZE` | `usize` | `1 << 20` | Largest accepted page size (1 MiB). |
| `PAGE_HEADER_SIZE` | `usize` | `32` | Header size; payload is `page_size - 32`. |
| `DEFAULT_PAGE_SIZE` | [`PageSize`](#pagesize) | `4096` | Default page size. |

```rust
use page_db::{MIN_PAGE_SIZE, MAX_PAGE_SIZE, PAGE_HEADER_SIZE, DEFAULT_PAGE_SIZE};

assert_eq!(MIN_PAGE_SIZE, 4096);
assert_eq!(MAX_PAGE_SIZE, 1 << 20);
assert_eq!(PAGE_HEADER_SIZE, 32);
assert_eq!(DEFAULT_PAGE_SIZE.get(), 4096);
```

<br>

### `PageSize`

A validated page size. A page size must be a power of two within
`MIN_PAGE_SIZE..=MAX_PAGE_SIZE`. Validating once means the rest of the crate
treats the size as a trusted invariant.

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `const fn new(size: usize) -> PageResult<PageSize>` | Validate and wrap a size in bytes. |
| `get` | `const fn get(self) -> usize` | The size in bytes. |
| `payload_len` | `const fn payload_len(self) -> usize` | Usable payload length (`size - 32`). |
| `default` | `fn default() -> PageSize` | `DEFAULT_PAGE_SIZE` (4 KiB). |

**Parameters.** `new` takes `size`, the page size in bytes. It returns
[`PageError::InvalidPageSize`](#pageerror--pageresult) if `size` is not a power
of two or falls outside `MIN_PAGE_SIZE..=MAX_PAGE_SIZE`.

```rust
use page_db::PageSize;

// Valid power-of-two sizes in range.
let ps = PageSize::new(8192)?;
assert_eq!(ps.get(), 8192);
assert_eq!(ps.payload_len(), 8192 - 32);

// The default is 4 KiB.
assert_eq!(PageSize::default().get(), 4096);
# Ok::<(), page_db::PageError>(())
```

```rust
use page_db::PageSize;

// Rejected: not a power of two, and below the 4 KiB floor.
assert!(PageSize::new(5000).is_err());
assert!(PageSize::new(1024).is_err());
assert!(PageSize::new(1 << 21).is_err());   // above 1 MiB
```

<br>

### `PageId`

The id of a page within a file — its slot index. Page ids are dense from zero:
page `n` lives at byte offset `n * page_size`.

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `const fn new(id: u64) -> PageId` | Wrap a raw slot index. |
| `get` | `const fn get(self) -> u64` | The raw slot index. |

`PageId` is `Copy`, `Ord`, `Hash`, and `Display`.

```rust
use page_db::PageId;

let id = PageId::new(42);
assert_eq!(id.get(), 42);
assert_eq!(format!("page {id}"), "page 42");

// Ordered and hashable — usable as a map key.
use std::collections::BTreeSet;
let mut pinned = BTreeSet::new();
pinned.insert(PageId::new(7));
assert!(pinned.contains(&PageId::new(7)));
```

<br>

### `Lsn`

A write-ahead-log sequence number stamped into a page header. page-db does not
interpret the LSN; it carries the value so a log (`wal-db`) and the recovery
code above can order a page against the records that describe it.

| Item | Signature | Description |
|------|-----------|-------------|
| `ZERO` | `const Lsn` | Sentinel for "never logged". |
| `new` | `const fn new(lsn: u64) -> Lsn` | Wrap a raw sequence number. |
| `get` | `const fn get(self) -> u64` | The raw sequence number. |

`Lsn` is `Copy`, `Ord`, `Hash`, and `Display`.

```rust
use page_db::Lsn;

assert_eq!(Lsn::ZERO.get(), 0);

let a = Lsn::new(100);
let b = Lsn::new(200);
assert!(b > a);            // sequence numbers order naturally
assert_eq!(a.get(), 100);
```

<br>

### `Page`

A single fixed-size page: a header and a payload in one buffer aligned for
Direct I/O. Build one with `Page::new`, or get one back from
[`PageFile::read_page`](#pagefile) / [`PageFile::allocate_page`](#pagefile).

The checksum is not maintained on every mutation — it is stamped once, when the
page is written ([`PageFile::write_page`](#pagefile) does it) or by
`to_checksummed_bytes`, and verified once, on read or by `from_bytes`.

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `fn new(page_size: PageSize) -> Page` | A zeroed page with a valid header. |
| `from_bytes` | `fn from_bytes(page_size: PageSize, bytes: &[u8]) -> PageResult<Page>` | Load and verify a page from a byte block. |
| `page_size` | `fn page_size(&self) -> usize` | The page size in bytes. |
| `id` | `fn id(&self) -> PageId` | The id stamped in the header. |
| `lsn` | `fn lsn(&self) -> Lsn` | The log sequence number in the header. |
| `set_lsn` | `fn set_lsn(&mut self, lsn: Lsn)` | Set the LSN (takes effect at the next stamp). |
| `payload` | `fn payload(&self) -> &[u8]` | The bytes after the header. |
| `payload_mut` | `fn payload_mut(&mut self) -> &mut [u8]` | The payload, mutably. |
| `to_checksummed_bytes` | `fn to_checksummed_bytes(&self) -> Vec<u8>` | The whole page as a checksummed byte block. |

`Page` is `Clone`, `Debug`, `Send`, and `Sync`.

**Building and filling a page.**

```rust
use page_db::{Page, PageSize, Lsn};

let mut page = Page::new(PageSize::new(4096)?);
assert_eq!(page.page_size(), 4096);
assert_eq!(page.lsn(), Lsn::ZERO);

page.set_lsn(Lsn::new(7));
page.payload_mut()[..4].copy_from_slice(b"data");
assert_eq!(&page.payload()[..4], b"data");
# Ok::<(), page_db::PageError>(())
```

**Framing a page without a file** — serialize to a checksummed block, persist
it anywhere, and load it back verified.

```rust
use page_db::{Page, PageSize, Lsn};

let size = PageSize::new(4096)?;
let mut page = Page::new(size);
page.set_lsn(Lsn::new(99));
page.payload_mut()[..3].copy_from_slice(b"abc");

let bytes = page.to_checksummed_bytes();          // 4096 bytes, checksummed
assert_eq!(bytes.len(), 4096);

let loaded = Page::from_bytes(size, &bytes)?;     // verifies header + checksum
assert_eq!(loaded.lsn(), Lsn::new(99));
assert_eq!(&loaded.payload()[..3], b"abc");
# Ok::<(), page_db::PageError>(())
```

**Corruption is rejected on load.**

```rust
use page_db::{Page, PageSize, PageError};

let size = PageSize::new(4096)?;
let mut bytes = Page::new(size).to_checksummed_bytes();
bytes[100] ^= 0xFF;                                // flip a payload byte

assert!(matches!(
    Page::from_bytes(size, &bytes),
    Err(PageError::ChecksumMismatch { .. })
));
# Ok::<(), page_db::PageError>(())
```

<br>

### `PageFile`

A file of fixed-size pages, read and written through Direct I/O. Reads and
writes are positioned and take `&self`, so the handle is shared freely across
threads — there is no shared file cursor to contend on.

| Method | Signature | Description |
|--------|-----------|-------------|
| `open` | `fn open<P: AsRef<Path>>(path: P, page_size: PageSize) -> PageResult<PageFile>` | Open with default options (Direct I/O on, create-if-absent). |
| `page_size` | `fn page_size(&self) -> usize` | The file's page size. |
| `page_count` | `fn page_count(&self) -> PageResult<u64>` | Number of whole pages in the file. |
| `allocate_page` | `fn allocate_page(&self) -> Page` | A fresh zeroed page sized for this file. |
| `read_page` | `fn read_page(&self, id: PageId) -> PageResult<Page>` | Read and verify the page at `id`. |
| `write_page` | `fn write_page(&self, id: PageId, page: &mut Page) -> PageResult<()>` | Stamp and write `page` to slot `id`. |
| `sync` | `fn sync(&self) -> PageResult<()>` | Flush all writes to stable storage. |

`PageFile` is `Send` and `Sync`.

**Parameters.**

- `open` — `path` is the file path; `page_size` is the fixed size of every page
  in the file (the caller must reopen with the same size). Use
  [`PageFileOptions`](#pagefileoptions) for buffered I/O or other tuning.
- `read_page` — `id` is the slot to read. The page's magic, version, CRC32C, and
  stamped id are all checked before it is returned.
- `write_page` — `id` is the destination slot; the page's id and checksum header
  fields are updated in place, so the same page can be written, mutated, and
  written again. `page.page_size()` must equal the file's.

**Errors.** `read_page` returns [`PageError::ShortRead`](#pageerror--pageresult)
past the end of the file, or `BadMagic` / `UnsupportedVersion` /
`ChecksumMismatch` / `MisdirectedPage` on a failed check. `write_page` returns
`InvalidPageSize` on a size mismatch. All three I/O methods return
`PageError::Io` on an OS failure.

**Write, sync, read.**

```rust
use page_db::{PageFile, PageId, Lsn, DEFAULT_PAGE_SIZE};

# let dir = tempfile::tempdir().unwrap();
# let path = dir.path().join("data.pages");
let file = PageFile::open(&path, DEFAULT_PAGE_SIZE)?;

let mut page = file.allocate_page();
page.set_lsn(Lsn::new(5));
page.payload_mut()[..2].copy_from_slice(b"hi");
file.write_page(PageId::new(3), &mut page)?;
file.sync()?;

let got = file.read_page(PageId::new(3))?;
assert_eq!(got.id(), PageId::new(3));
assert_eq!(&got.payload()[..2], b"hi");
assert_eq!(file.page_count()?, 4);   // slots 0..=3 now exist
# Ok::<(), page_db::PageError>(())
```

**Batch many writes, then one sync** — the efficient durability pattern.

```rust
use page_db::{PageFile, PageId, DEFAULT_PAGE_SIZE};

# let dir = tempfile::tempdir().unwrap();
# let path = dir.path().join("data.pages");
let file = PageFile::open(&path, DEFAULT_PAGE_SIZE)?;

for id in 0..16u64 {
    let mut page = file.allocate_page();
    page.payload_mut()[0] = id as u8;
    file.write_page(PageId::new(id), &mut page)?;
}
file.sync()?;   // one flush makes all sixteen durable
assert_eq!(file.page_count()?, 16);
# Ok::<(), page_db::PageError>(())
```

**Reading a missing slot is an error, not a panic.**

```rust
use page_db::{PageFile, PageId, PageError, DEFAULT_PAGE_SIZE};

# let dir = tempfile::tempdir().unwrap();
# let path = dir.path().join("empty.pages");
let file = PageFile::open(&path, DEFAULT_PAGE_SIZE)?;
assert!(matches!(
    file.read_page(PageId::new(0)),
    Err(PageError::ShortRead { .. })
));
# Ok::<(), page_db::PageError>(())
```

<br>

### `PageFileOptions`

A builder for opening a [`PageFile`](#pagefile). Defaults: 4 KiB pages, Direct
I/O enabled, create-if-absent.

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `fn new() -> PageFileOptions` | Start from the defaults. |
| `page_size` | `fn page_size(self, page_size: PageSize) -> Self` | Set the page size. |
| `direct_io` | `fn direct_io(self, enabled: bool) -> Self` | Enable/disable cache-bypass I/O. |
| `create` | `fn create(self, create: bool) -> Self` | Create the file if absent. |
| `open` | `fn open<P: AsRef<Path>>(self, path: P) -> PageResult<PageFile>` | Open with these options. |

**When to disable Direct I/O.** Some overlay and network filesystems reject
`O_DIRECT`. Disabling it keeps the same API and the same durability via `sync`;
only the page cache behaves differently.

```rust
use page_db::{PageFileOptions, PageSize};

# let dir = tempfile::tempdir().unwrap();
# let path = dir.path().join("data.pages");
let file = PageFileOptions::new()
    .page_size(PageSize::new(8192)?)
    .direct_io(false)        // buffered, e.g. on a filesystem without O_DIRECT
    .open(&path)?;
assert_eq!(file.page_size(), 8192);
# Ok::<(), page_db::PageError>(())
```

**Open an existing file read-only-ish** — refuse to create a missing one.

```rust
use page_db::PageFileOptions;

# let dir = tempfile::tempdir().unwrap();
# let path = dir.path().join("missing.pages");
let result = PageFileOptions::new()
    .create(false)
    .open(&path);
assert!(result.is_err());    // the file does not exist
```

<br>

### `PageError` & `PageResult`

`PageResult<T>` is `Result<T, PageError>`. `PageError` is `#[non_exhaustive]`;
match with a wildcard arm.

| Variant | Meaning | What to do |
|---------|---------|------------|
| `Io(std::io::Error)` | Underlying file I/O failed. | Inspect the source `io::Error` (e.g. `NotFound`, `PermissionDenied`). |
| `InvalidPageSize { size }` | A size is not a power of two in range, or a page's size does not match the file's. | Use a valid `PageSize`; reopen with the file's size. |
| `BadMagic { found, expected }` | The block is not a page-db page. | The slot is uninitialized or the file is not a page file. |
| `UnsupportedVersion { found, supported }` | The page format is newer than this build. | Upgrade the reader. |
| `ChecksumMismatch { page_id, stored, computed }` | The page is corrupt (torn write, bit rot). | Treat the page as lost; recover from the log or a replica. |
| `MisdirectedPage { requested, found }` | The slot holds a page stamped with a different id. | A misdirected read/write — surface as corruption. |
| `ShortRead { page_id, got, page_size }` | The slot is past end-of-file, or the file is not a whole number of pages. | Allocate the slot first, or check `page_count`. |

```rust
use page_db::{PageFile, PageId, PageError, DEFAULT_PAGE_SIZE};

# let dir = tempfile::tempdir().unwrap();
# let path = dir.path().join("data.pages");
let file = PageFile::open(&path, DEFAULT_PAGE_SIZE)?;
match file.read_page(PageId::new(0)) {
    Ok(page)                                   => { let _ = page; }
    Err(PageError::ShortRead { .. })           => { /* slot not written yet */ }
    Err(PageError::ChecksumMismatch { page_id, .. }) => {
        eprintln!("page {page_id} is corrupt");
    }
    Err(other)                                 => return Err(other),
}
# Ok::<(), page_db::PageError>(())
```

I/O errors convert in with `?`:

```rust
use page_db::PageError;

fn wrap(e: std::io::Error) -> PageError {
    PageError::from(e)   // via `From<std::io::Error>`
}
```

<br>

### `checksum` — CRC32C

The `page_db::checksum` module exposes the page checksum directly. It is CRC32C
(Castagnoli), the variant CPUs accelerate and the one ext4 and `wal-db` use.

| Item | Signature | Description |
|------|-----------|-------------|
| `crc32c` | `fn crc32c(data: &[u8]) -> u32` | One-shot checksum of a buffer (also re-exported at the crate root). |
| `Crc32c::new` | `const fn new() -> Crc32c` | Start a streaming checksum. |
| `Crc32c::update` | `fn update(&mut self, data: &[u8])` | Fold bytes into the running checksum. |
| `Crc32c::finalize` | `fn finalize(self) -> u32` | The final CRC32C value. |

The standard `"123456789"` check value is `0xE3069283`.

```rust
use page_db::crc32c;

assert_eq!(crc32c(b""), 0);
assert_eq!(crc32c(b"123456789"), 0xE306_9283);
```

**Streaming non-contiguous ranges** gives the same result as checksumming their
concatenation — what the page header uses to skip its own checksum field.

```rust
use page_db::checksum::Crc32c;

let mut h = Crc32c::new();
h.update(b"123");
h.update(b"456789");
assert_eq!(h.finalize(), page_db::crc32c(b"123456789"));
```

<br>

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `serde` | no | `Serialize`/`Deserialize` for `PageId` and `Lsn`. |

The crate is `std`-only: a file-backed, Direct-I/O page store is inherently
`std`.

<br>

## Notes

- **On-disk format is unstable across 0.x.** Files written by 0.2 are not
  guaranteed readable by later 0.x releases; the format is frozen for 1.x before
  1.0.
- **Direct I/O alignment** is handled for you: page buffers are aligned to the
  page size, which satisfies the block-alignment rules on every supported
  platform for page sizes of 4 KiB and up.
- **Thread safety.** `PageFile`, `Page`, `PageId`, and `Lsn` are `Send + Sync`.
  Concurrent positioned reads and writes do not contend on a shared cursor; the
  caller is responsible for not writing the same slot from two threads at once.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>

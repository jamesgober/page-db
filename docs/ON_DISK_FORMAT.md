<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>page-db</b><br>
    <sub><sup>ON-DISK FORMAT</sup></sub>
</h1>

<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <a href="./API.md" title="API Reference"><b>API</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>ON-DISK FORMAT</span>
        <span>&nbsp;│&nbsp;</span>
        <a href="./BENCHMARKS.md" title="Benchmarks"><b>BENCHMARKS</b></a>
    </sup>
</div>

<br>

> Normative specification of the bytes `page-db` writes. The **page format** and
> the **allocator superblock** in this document are **frozen for the 1.x line** as
> of `1.0.0`: a file written by any `>= 1.0.0`, `< 2.0.0` release reads back
> identically on any other.

## Status and stability

| Element | Stability |
|---------|-----------|
| Page header | Frozen for 1.x as of 1.0.0 |
| Allocator superblock + free-list chain | Frozen for 1.x as of 1.0.0 |

A change to a frozen layout is a breaking change requiring a major version and a
documented migration. Additive, backward-compatible changes (a new meaning for a
currently-reserved field that older readers ignore safely) may appear in a minor
version. The `version` fields exist so a reader can refuse a format it does not
understand rather than misread it.

## Conventions

- All multi-byte integers are **little-endian**, independent of host byte order.
- Offsets and lengths are in **bytes**.
- `u16`/`u32`/`u64` denote unsigned little-endian integers of that width.
- A **page** is a fixed power-of-two number of bytes, the *page size*, chosen at
  creation and constant for the life of the file. It is in `4096 ..= 1048576`.

## File structure

A file is a dense array of equal-size pages, back to back, starting at offset 0.
Page `n` occupies the byte range `[n * page_size, (n + 1) * page_size)`. There is
no file header: the first page is page 0, addressed like any other.

```text
offset 0          page_size        2*page_size                end
  |    page 0     |    page 1      |    page 2     | ...  |
```

The page size is not stored in the file; the reader supplies it on open and must
use the same size the file was written with. A file used with the
[allocator](#allocator-superblock) additionally reserves page 0 as a superblock.

## Page layout

Every page is a 32-byte header followed by `page_size - 32` bytes of payload:

```text
        +--------+--------+--------+----------+----------+--------+----------+--------------+
 field  | magic  | version| flags  | page id  | lsn      | crc32c | reserved | payload      |
 type   | u32    | u16    | u16    | u64      | u64      | u32    | u32      | bytes        |
 offset | +0     | +4     | +6     | +8       | +16      | +24    | +28      | +32          |
        +--------+--------+--------+----------+----------+--------+----------+--------------+
```

| Field | Type | Offset | Meaning |
|-------|------|--------|---------|
| `magic` | `u32` | 0 | Constant `0x42444750` — the ASCII bytes `P G D B` read little-endian. Identifies a page-db page. |
| `version` | `u16` | 4 | Page-header format version. Currently `1`. A reader rejects a version it does not understand. |
| `flags` | `u16` | 6 | Reserved. Written as `0`; readers ignore unknown bits. |
| `page id` | `u64` | 8 | The slot this page belongs to. Stamped on write; checked against the requested slot on read to catch a misdirected read. |
| `lsn` | `u64` | 16 | A write-ahead-log sequence number the caller stamps. page-db does not interpret it; `0` means "not logged". |
| `crc32c` | `u32` | 24 | CRC32C over the whole page **except these four bytes** (see [Checksum](#checksum)). |
| `reserved` | `u32` | 28 | Reserved. Written as `0`. |
| `payload` | bytes | 32 | The caller's bytes, `page_size - 32` of them. |

## Checksum

The page checksum is **CRC32C** (Castagnoli), the standard storage checksum:

| Parameter | Value |
|-----------|-------|
| Width | 32 bits |
| Polynomial | `0x1EDC6F41` (reflected `0x82F63B78`) |
| Initial value | `0xFFFFFFFF` |
| Reflect input | yes |
| Reflect output | yes |
| Final XOR | `0xFFFFFFFF` |
| Check value (`"123456789"`) | `0xE3069283` |

This matches the CRC-32C used by iSCSI, SCTP, and ext4. The checksum covers every
byte of the page **except** the four `crc32c` bytes at offset 24 — that is, bytes
`[0, 24)` followed by bytes `[28, page_size)`. Equivalently, the page is
checksummed with its `crc32c` field treated as absent. This protects the header
(magic, version, page id, lsn) and the payload together.

## Writing a page

To write a page to slot `n`:

1. Lay out the payload and set `lsn` as the caller requested.
2. Write `magic`, `version = 1`, `flags = 0`, `page id = n`, `reserved = 0`.
3. Compute the CRC32C over the page with the `crc32c` field excluded, and write
   it at offset 24.
4. Write the whole page at byte offset `n * page_size`.

The bytes reach stable storage only when a subsequent `sync` (the platform
durability barrier — `fdatasync`, `FlushFileBuffers`, or macOS `F_FULLFSYNC`)
returns.

## Reading a page

To read slot `n`, read `page_size` bytes at offset `n * page_size`, then validate
in order:

1. **Length.** A short read means the slot is past the end of the file.
2. **Magic.** If `magic` is not the page-db constant, the block is not a page.
3. **Version.** If `version` is not understood, refuse it.
4. **Checksum.** Recompute the CRC32C and compare to the stored `crc32c`. A
   mismatch means the page is corrupt — a torn write or bit rot.
5. **Page id.** If the stored `page id` is not `n`, the read was misdirected.

A page is never trusted until its checksum verifies, and every failure is a typed
error, never a silent read of bad data.

## Allocator superblock

A file managed by the page allocator reserves **page 0** as a *superblock*. It is
a normal page (same 32-byte header, same checksum), whose payload holds the
allocator's persistent state:

```text
 payload  +----------+--------+----------+-----------+-----------+------------+
 field    | sb magic | version| reserved | free head | next new  | free count |
 type     | u32      | u16    | u16      | u64       | u64       | u64        |
 offset   | +0       | +4     | +6       | +8        | +16       | +24        |
        (offsets are within the payload, i.e. +32 from the page start)
```

| Field | Type | Payload offset | Meaning |
|-------|------|----------------|---------|
| `sb magic` | `u32` | 0 | Constant `0x42534750` — ASCII `P G S B` read little-endian. |
| `version` | `u16` | 4 | Superblock format version. Currently `1`. |
| `reserved` | `u16` | 6 | Reserved, `0`. |
| `free head` | `u64` | 8 | First page id on the free-list, or `0xFFFFFFFFFFFFFFFF` (the "no page" sentinel) if the free-list is empty. |
| `next new` | `u64` | 16 | The high-water mark: the next id to hand out when the free-list is empty. Pages `1 .. next_new` have been allocated at some point. Always `>= 1` (page 0 is the superblock). |
| `free count` | `u64` | 24 | The number of ids on the free-list. |

The allocator hands out ids starting at `1`; page 0 is never a data page.

## Free-list chain

Freed pages form an intrusive singly-linked list. The `free head` in the
superblock is the first free id; each free page stores the **next** free id in
the first eight bytes of its payload:

```text
 payload  +-----------+----------------------------+
 field    | next      | unused                     |
 type     | u64       | bytes                      |
 offset   | +0        | +8                         |
```

The last page on the list stores the "no page" sentinel
(`0xFFFFFFFFFFFFFFFF`) as its `next`. The chain has exactly `free count` links.

### Reading the free-list defensively

The superblock is untrusted input. A reader rebuilding the free-list MUST bound
the walk against a corrupt or hostile superblock:

- Follow at most `free count` links.
- Every link MUST be a real allocated id: in `1 .. next_new`, never `0`.
- A repeated id means the chain cycles; reject it rather than follow it.
- The walk MUST end at the sentinel after exactly `free count` links.

Any deviation makes the superblock invalid. With these rules the walk's time and
memory are bounded by the pages that actually exist, so a crafted superblock can
never drive an unbounded walk.

## Durability and ordering

A page write and a `sync` are separate steps: the write places bytes, the `sync`
makes them durable. The allocator's superblock is written by its own `sync`,
which MUST be ordered as part of the same checkpoint that makes the pages it
handed out durable — the high-water mark must be durable no later than the data
written to the ids beyond it. page-db provides the mechanism; a write-ahead log
above is the authority on crash recovery, and the superblock is a checkpoint of
allocator state, not a transaction log.

<hr>
<br>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>

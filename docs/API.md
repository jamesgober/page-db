# page-db &mdash; API Reference

> Complete reference for every public item in `page-db`, with examples.
> **Status: pre-1.0.** Sections below describe the intended surface as it lands across the 0.x series (see [`dev/ROADMAP.md`](../dev/ROADMAP.md)).

## Table of Contents

- [Overview](#overview)
- [Fixed-size pages](#fixed-size-pages) _(planned)_
- [CRC32 integrity](#crc32-integrity) _(planned)_
- [LRU buffer pool](#lru-buffer-pool) _(planned)_
- [Dirty-page pinning](#dirty-page-pinning) _(planned)_
- [Cross-platform Direct I/O](#cross-platform-direct-io) _(planned)_
- [Page allocation](#page-allocation) _(planned)_
- [Feature flags](#feature-flags)

---

## Overview

page-db is the paging substrate that sits beneath B-tree and heap storage engines. It owns the unglamorous, get-it-exactly-right layer every database needs: fixed-size pages on disk, each with a header carrying a CRC32 integrity check and an LSN slot for write-ahead-log coordination, read and written through cross-platform Direct I/O that bypasses the OS page cache.

---

### Fixed-size pages

_configurable page size; a versioned page header with magic, CRC32, and an LSN slot. Documented as this lands across the 0.x series._

### CRC32 integrity

_every page is checksummed; a torn or corrupt page is detected on read, never silently trusted. Documented as this lands across the 0.x series._

### LRU buffer pool

_bounded in-memory frame cache with clock/LRU eviction. Documented as this lands across the 0.x series._

### Dirty-page pinning

_pin pages against eviction while in use; track and flush dirty frames on a schedule. Documented as this lands across the 0.x series._

### Cross-platform Direct I/O

_O_DIRECT (Linux), F_NOCACHE (macOS), FILE_FLAG_NO_BUFFERING (Windows), with aligned buffers. Documented as this lands across the 0.x series._

### Page allocation

_a free-list / allocator for new and reclaimed page ids. Documented as this lands across the 0.x series._

---

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std` | yes | Standard library. |
| `serde` | no | Serialization for public types. |

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>

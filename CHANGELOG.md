# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

## [0.2.0] - 2026-06-05

The first working release: the page format and the durable file underneath it.
A fixed-size page with a checksummed, LSN-bearing header, and a `PageFile` that
reads and writes pages through cross-platform Direct I/O.

### Added

- `Page` &mdash; a fixed-size page over a Direct-I/O-aligned buffer, with a
  32-byte header (magic, version, page id, LSN, CRC32C). `set_lsn`, `payload` /
  `payload_mut`, `to_checksummed_bytes`, and `from_bytes` (load-and-verify).
- `PageFile` &mdash; an array of pages addressed by `PageId`, with `open`,
  `read_page` (verifies header, checksum, and slot id), `write_page` (stamps id
  and checksum), `allocate_page`, `page_count`, and `sync`. Positioned I/O on
  `&self`, so the handle is shared across threads.
- `PageFileOptions` &mdash; builder for page size, Direct I/O on/off, and
  create-if-absent.
- Cross-platform Direct I/O: `O_DIRECT` (Linux), `F_NOCACHE` (macOS),
  `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH` (Windows), with a buffered
  mode for filesystems that reject it. Durable `sync` via `fdatasync`,
  `FlushFileBuffers`, and macOS `F_FULLFSYNC`.
- `PageId`, `Lsn`, and the validated `PageSize`, plus the `MIN_PAGE_SIZE`,
  `MAX_PAGE_SIZE`, `PAGE_HEADER_SIZE`, and `DEFAULT_PAGE_SIZE` constants.
- `checksum` module: a software slice-by-eight CRC32C (`crc32c` one-shot and the
  streaming `Crc32c`), with const-generated tables.
- `PageError` / `PageResult` &mdash; typed I/O and integrity failures
  (`ChecksumMismatch`, `MisdirectedPage`, `BadMagic`, `UnsupportedVersion`,
  `ShortRead`, `InvalidPageSize`).
- Property tests for the round-trip, single-byte-corruption-always-caught, and
  no-cross-talk invariants; a real Direct I/O integration test; criterion
  benchmarks for the checksum and the read / write / sync paths.

### Changed

- **Breaking (relative to the 0.1 scaffold).** The crate is now `std`-only; the
  `no_std` posture and the `std` feature flag are removed, because a file-backed
  Direct-I/O store is inherently `std`. The default feature set is now empty. The
  0.1 scaffold shipped no functional code, so this breaks no real consumer.
- The page checksum is CRC32C (Castagnoli), not IEEE CRC32 &mdash; the variant
  hardware accelerates and the one the sibling `wal-db` uses.

### Notes

- The on-disk format is unstable across 0.x and frozen for 1.x before 1.0.

## [0.1.0] - 2026-06-05

Initial scaffold and repository bootstrap. No domain logic yet &mdash; this release establishes the structure, tooling, and quality gates the implementation will be built on.

### Added

- `Cargo.toml` with crate metadata, Rust 2024 edition, MSRV 1.85, dual `Apache-2.0 OR MIT` license.
- `README.md`, `docs/API.md`, `CONTRIBUTING.md`, and a documentation skeleton.
- `dev/DIRECTIVES.md` and `dev/ROADMAP.md` (committed engineering standards + plan).
- `REPS.md` compliance baseline; `deny.toml`, `clippy.toml`, `rustfmt.toml`.
- `.github/workflows/ci.yml` (Node 24 actions; fmt, clippy, test, doc, audit, deny) and `.github/FUNDING.yml`.

<!-- LINKS -->
[Unreleased]: https://github.com/jamesgober/page-db/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jamesgober/page-db/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/page-db/releases/tag/v0.1.0

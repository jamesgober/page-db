<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>page-db</b>
    <br>
    <sub><sup>PAGING SUBSTRATE FOR STORAGE ENGINES</sup></sub>
</h1>

<div align="center">
    <a href="https://crates.io/crates/page-db"><img alt="Crates.io" src="https://img.shields.io/crates/v/page-db"></a>
    <a href="https://crates.io/crates/page-db" alt="Download page-db"><img alt="Crates.io Downloads" src="https://img.shields.io/crates/d/page-db?color=%230099ff"></a>
    <a href="https://docs.rs/page-db" title="page-db Documentation"><img alt="docs.rs" src="https://img.shields.io/docsrs/page-db"></a>
    <a href="https://github.com/jamesgober/page-db/actions"><img alt="GitHub CI" src="https://github.com/jamesgober/page-db/actions/workflows/ci.yml/badge.svg"></a>
    <a href="https://github.com/rust-lang/rfcs/blob/master/text/2495-min-rust-version.md" title="MSRV"><img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85%2B-blue"></a>
</div>

<br>

<div align="left">
    <p>
        <strong>page-db</strong> is the <b>paging substrate</b> that sits beneath B-tree and heap storage engines. It owns the unglamorous, get-it-exactly-right layer every database needs: <b>fixed-size pages</b> on disk, each with a header carrying a <b>CRC32</b> integrity check and an <b>LSN slot</b> for write-ahead-log coordination, read and written through <b>cross-platform Direct I/O</b> that bypasses the OS page cache.
    </p>
    <p>
        Above the file sits an <b>LRU buffer pool</b> with <b>dirty-page pinning</b>: hot pages stay resident, in-flight pages are pinned against eviction, and dirty pages are flushed on a controlled schedule. The engine above asks for a page by id and gets a pinned, checksummed frame back.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition). Fixed-size pages. CRC32 + LSN headers. LRU buffer pool. Direct I/O.
    </p>
    <blockquote>
        <strong>Status: pre-1.0, in active development.</strong> This is the <code>v0.1.0</code> scaffold &mdash; structure, tooling, and CI gates are in place; the implementation lands across the 0.x series per <a href="./dev/ROADMAP.md"><code>dev/ROADMAP.md</code></a>. The public API is frozen at <code>1.0.0</code>.
    </blockquote>
</div>

<hr>
<br>

<h2>What it does</h2>

- **Fixed-size pages** &mdash; configurable page size; a versioned page header with magic, CRC32, and an LSN slot
- **CRC32 integrity** &mdash; every page is checksummed; a torn or corrupt page is detected on read, never silently trusted
- **LRU buffer pool** &mdash; bounded in-memory frame cache with clock/LRU eviction
- **Dirty-page pinning** &mdash; pin pages against eviction while in use; track and flush dirty frames on a schedule
- **Cross-platform Direct I/O** &mdash; O_DIRECT (Linux), F_NOCACHE (macOS), FILE_FLAG_NO_BUFFERING (Windows), with aligned buffers
- **Page allocation** &mdash; a free-list / allocator for new and reclaimed page ids

<br>
<hr>
<br>

## Installation

```toml
[dependencies]
page-db = "0.1"
```

<br>

## API Overview

For the complete reference, see [`docs/API.md`](./docs/API.md).

- [`Fixed-size pages`](./docs/API.md)
- [`CRC32 integrity`](./docs/API.md)
- [`LRU buffer pool`](./docs/API.md)
- [`Dirty-page pinning`](./docs/API.md)

<br>
<hr>
<br>

## Where It Fits

`page-db` is the lowest layer of the storage-engine stack. It is built on by:

- [`index-db`](https://github.com/jamesgober/index-db) &mdash; B+tree nodes are pages allocated and cached here
- [`lock-db`](https://github.com/jamesgober/lock-db) &mdash; the concurrency-control sibling over the same paged store
- [`wal-db`](https://github.com/jamesgober/wal-db) &mdash; the LSN slot in each page header coordinates with the write-ahead log
- heap / B-tree engines &mdash; any storage engine that needs durable, cached, fixed-size pages

It has no first-party dependencies, so it builds and tests standalone today.

<br>

## Cross-Platform Support

Linux (x86_64, aarch64), macOS (x86_64, Apple Silicon), and Windows (x86_64) are first-class and verified by the CI matrix.

<br>

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md) and [`dev/DIRECTIVES.md`](./dev/DIRECTIVES.md). Before a PR: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` must be clean.

<br>

<div id="license">
    <h2>License</h2>
    <p>Licensed under either of</p>
    <ul>
        <li><b>Apache License, Version 2.0</b> &mdash; <a href="./LICENSE-APACHE">LICENSE-APACHE</a></li>
        <li><b>MIT License</b> &mdash; <a href="./LICENSE-MIT">LICENSE-MIT</a></li>
    </ul>
    <p>at your option.</p>
</div>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>

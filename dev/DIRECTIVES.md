# page-db -- Engineering Directives

> Engineering standards and the definition of done for this project. Read alongside `REPS.md` (root, authoritative) and `dev/ROADMAP.md` (current phase). If anything here conflicts with `REPS.md`, `REPS.md` wins.

---

## 0. Philosophy

This library is built and maintained to a production standard and treated as a flagship piece of work. Plan the full path, then build one verified step at a time. "Good enough" is treated as a defect.

---

## 1. What this is

page-db is the paging substrate that sits beneath B-tree and heap storage engines. It owns the unglamorous, get-it-exactly-right layer every database needs: fixed-size pages on disk, each with a header carrying a CRC32 integrity check and an LSN slot for write-ahead-log coordination, read and written through cross-platform Direct I/O that bypasses the OS page cache. Above the file sits an LRU buffer pool with dirty-page pinning: hot pages stay resident, in-flight pages are pinned against eviction, and dirty pages are flushed on a controlled schedule. The engine above asks for a page by id and gets a pinned, checksummed frame back.

---

## 2. Engineering law (non-negotiable)

- **Performance** -- peak is the baseline; borrow over clone; no steady-state hot-path allocation; no "faster" claim without `criterion` numbers.
- **Concurrency** -- correctness under contention is proven with `loom`, not assumed.
- **Correctness** -- the invariants in section 4 are covered by property tests.
- **Security** -- all untrusted input validated; every allocation bounded; library code never panics on hostile input; parse/recovery paths fuzzed.
- **Architecture** -- SOLID, KISS, YAGNI; one responsibility; trait seams are the extension points.
- **Cross-platform** -- Linux/macOS/Windows first-class, verified by CI.
- **Error handling** -- every fallible path returns `Result`; errors are never silently swallowed.
- **Production-ready** -- no commented-out code, no stray `println!`/`dbg!`; every public item has rustdoc with a runnable example.

---

## 3. Definition of done

1. Compiles clean on Linux/macOS/Windows, stable and MSRV.
2. `fmt`, `clippy -D warnings`, `test --all-features`, `cargo doc -D warnings` clean.
3. `cargo audit` + `cargo deny check` pass.
4. No `unwrap`/`expect`/`todo!`/`dbg!` in shipping code; `unsafe` only with `// SAFETY:`.
5. A Tier-1 API exists and headlines the docs.
6. Property tests cover every section-4 invariant; `loom` covers every concurrent path.
7. Hot-path changes carry benchmarks; no regression over 5%.
8. Docs and `CHANGELOG.md` updated.

---

## 4. Project-specific invariants

- A page is never trusted without verifying its CRC32 first; a checksum mismatch is a typed error, never a silent read.
- The buffer pool must never evict a pinned page, and must never lose a dirty page without flushing it - both are property-tested invariants.
- Direct I/O buffers are alignment-correct on every platform; unaligned access is a bug, not a fallback.

Per-phase exit criteria in `dev/ROADMAP.md` encode these.

---

## 5. Integration points

- `index-db`: B+tree nodes are pages allocated and cached here
- `lock-db`: the concurrency-control sibling over the same paged store
- `wal-db`: the LSN slot in each page header coordinates with the write-ahead log
- heap / B-tree engines: any storage engine that needs durable, cached, fixed-size pages

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>

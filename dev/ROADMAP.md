# page-db -- Roadmap

> Path from scaffold to a stable 1.0. Hard parts are front-loaded; each phase has hard exit criteria.
>
> **Anti-deferral rule:** no listed hard task moves to a later phase unless this file records the move and the reason.

---

## v0.1.0 -- Scaffold (DONE)

Compiles, CI green, structure correct, no domain logic.

- [x] Manifest, README, CHANGELOG, REPS, dual license, CI, deny, clippy, rustfmt, FUNDING.
- [x] API surface sketched in `docs/API.md`.

---

## v0.2.0 -- page format + header (CRC32C + LSN) + Direct I/O read/write (DONE)

Exit criteria:
- [x] Every public item has rustdoc + a runnable example.
- [x] Core invariants property-tested.

---

## v0.3.0 -- LRU buffer pool + pinning + dirty tracking (DONE)

Exit criteria:
- [x] New surface tested; hot paths benchmarked.
- [x] Pin/dirty invariants loom-checked.

---

## v0.4.0 -- page allocator / free-list + cross-platform Direct I/O hardening + feature freeze (DONE)

Exit criteria:
- [x] No `todo!`/`unimplemented!`. **Feature freeze declared** -- the public API
  is complete for 1.0; remaining 0.x work is hardening only.

---

## v0.5.0 -- torn-page / corruption fuzzing + alignment edge cases + API freeze (DONE)

Exit criteria:
- [x] **Public API frozen.** No additions before 1.0 -- only bug fixes and the
  on-disk-format freeze. Frozen surface: `PageFile`/`PageFileOptions`,
  `BufferPool`/`PageGuard`/`PageRef`/`PageMut`, `PageAllocator`, `PageStore`,
  `Page`, `PageId`/`Lsn`/`PageSize`, `PageError`/`PageResult`, `checksum`.
- [x] `cargo audit` + `cargo deny` clean.
- [x] Parse + recovery paths fuzzed (`fuzz/`: `page_parse`, `allocator_open`).
  Fuzzing fixed a corrupt-superblock DoS in the free-chain walk.

---

## v1.0.0 -- Stable (DONE)

Shipped straight to stable. Broadened testing (multi-threaded stress + I/O
fault-injection on top of the loom models, property tests, and fuzzing), captured
final benchmarks ([`docs/BENCHMARKS.md`](../docs/BENCHMARKS.md)), and froze the
on-disk format for 1.x ([`docs/ON_DISK_FORMAT.md`](../docs/ON_DISK_FORMAT.md)).

Exit criteria:
- [x] **API frozen until 2.0.** On-disk format frozen for 1.x.
- [x] Full suite green on Linux/macOS/Windows, stable + MSRV; `cargo audit` +
  `cargo deny` clean.

---

## Out of scope for 1.0

- B+tree or heap logic - that lives in `index-db` and the engines above.
- The write-ahead log itself - `wal-db` owns the WAL; page-db only reserves the LSN slot.
- Locking / concurrency control - that is `lock-db`.

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

## v0.2.0 -- page format + header (CRC32 + LSN) + Direct I/O read/write (THE HARD PART, NOT DEFERRED)

Exit criteria:
- [ ] Every public item has rustdoc + a runnable example.
- [ ] Core invariants property-tested.

---

## v0.3.0 -- LRU buffer pool + pinning + dirty tracking

Exit criteria:
- [ ] New surface tested; hot paths benchmarked.

---

## v0.4.0 -- page allocator / free-list + cross-platform Direct I/O hardening + feature freeze

Exit criteria:
- [ ] No `todo!`/`unimplemented!`. Feature freeze declared.

---

## v0.5.0 -- torn-page / corruption fuzzing + alignment edge cases + API freeze

Exit criteria:
- [ ] Public API frozen (recorded here). `cargo audit` + `cargo deny` clean.

---

## v0.6.0 -> v1.0.0 -- Alpha / Beta / RC / Stable

Integrate against real consumers, broaden testing, capture final benchmarks, then freeze the public API until 2.0 and publish.

---

## Out of scope for 1.0

- B+tree or heap logic - that lives in `index-db` and the engines above.
- The write-ahead log itself - `wal-db` owns the WAL; page-db only reserves the LSN slot.
- Locking / concurrency control - that is `lock-db`.

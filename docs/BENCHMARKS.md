<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>page-db</b><br>
    <sub><sup>BENCHMARKS</sup></sub>
</h1>

<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <a href="./API.md" title="API Reference"><b>API</b></a>
        <span>&nbsp;│&nbsp;</span>
        <a href="./ON_DISK_FORMAT.md" title="On-Disk Format"><b>ON-DISK FORMAT</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>BENCHMARKS</span>
    </sup>
</div>

<br>

> What the hot paths cost, and how the numbers were taken. Run them yourself with
> `cargo bench --bench page_bench`.

## Method

The benchmarks live in [`benches/page_bench.rs`](../benches/page_bench.rs) and use
[criterion](https://github.com/bheisler/criterion.rs). Each figure is criterion's
reported median over its sample set. The file benchmarks use **buffered** I/O so
the framing and syscall cost is visible without a disk-flush barrier dominating
every sample; the durable path (`write_page` + `sync`) is measured separately.

These are single-machine numbers for orientation, not a cross-hardware promise —
they tell you the *shape* of the cost (an in-memory op is nanoseconds, a buffered
page write is microseconds, a durable flush is sub-millisecond and disk-bound).
Re-run on your target to get figures that mean something for your deployment.

**Environment.** Windows 11 x86_64, Rust stable 1.95, `--release`, 4 KiB pages.

## Results

| Benchmark | What it measures | Time |
|-----------|------------------|------|
| `crc32c/512` | CRC32C of a 512-byte buffer | ~0.15 µs (~3.1 GiB/s) |
| `crc32c/4096` | CRC32C of a 4 KiB page | ~1.37 µs (~2.8 GiB/s) |
| `crc32c/16384` | CRC32C of a 16 KiB buffer | ~5.4 µs (~2.8 GiB/s) |
| `page_write_4k_buffered` | Stamp + checksum + buffered `pwrite` of one page | ~3.1 µs |
| `page_read_4k_buffered` | Buffered `pread` + full header & checksum verify | ~2.7 µs |
| `page_write_sync_4k` | One page written **and made durable** (`write_page` + `sync`) | ~0.4–0.9 ms |
| `pool_fetch_hit` | `BufferPool::fetch` of a resident page (cache hit, no I/O) | ~37 ns |
| `pool_fetch_miss_evict` | `fetch` forcing a clock eviction + a read | ~3.1 µs |
| `alloc_extend` | `PageAllocator::allocate` extending the high-water mark | ~8.9 ns |
| `alloc_free_cycle` | `free` + `allocate` recycling a freed id | ~17 ns |

## Reading the numbers

- **The checksum is not the bottleneck.** CRC32C runs at ~2.8 GiB/s, so a 4 KiB
  page checksums in ~1.4 µs — a fraction of even the buffered page write it rides
  with, and negligible against a durable flush. A hardware-accelerated CRC would
  be faster still, but it would not move the page path; the software slice-by-8
  implementation is the right call until a profile says otherwise.

- **The buffer pool earns its keep on hits.** A cache hit is ~37 ns — about
  **eighty times** faster than the ~3.1 µs it costs to miss, evict a clock victim,
  and read the page back. That ratio is the entire reason the pool exists.

- **Allocation is free in practice.** `allocate` and `free` are pure in-memory
  operations: ~8.9 ns to extend the id space, ~17 ns to recycle a freed id. The
  on-disk free-list is written only at `sync`, so a burst of allocate/free churn
  costs nothing until the next checkpoint.

- **Durability is honestly sub-millisecond and variable.** `write_page` + `sync`
  is dominated by the platform flush, which depends on the drive, the filesystem,
  and background activity (antivirus, in particular, on Windows). The figure
  ranges accordingly; it is the cost of a guarantee, not of page-db.

<hr>
<br>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>

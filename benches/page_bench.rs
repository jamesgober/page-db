//! Criterion benchmarks for the page hot paths: checksumming, framing, and the
//! file read/write round-trip.
//!
//! Run with `cargo bench --bench page_bench`. The file benchmarks use buffered
//! I/O so the numbers reflect the framing and syscall cost without a disk-flush
//! barrier dominating every sample; `sync` is measured separately.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use page_db::{BufferPool, Lsn, PageAllocator, PageFileOptions, PageId, PageSize, crc32c};

fn bench_crc32c(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32c");
    for size in [512usize, 4096, 16384] {
        let data = vec![0xA5u8; size];
        group.throughput(Throughput::Bytes(size as u64));
        let _ = group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            b.iter(|| crc32c(black_box(data)));
        });
    }
    group.finish();
}

fn bench_stamp(c: &mut Criterion) {
    let file = PageFileOptions::new()
        .direct_io(false)
        .create(true)
        .open(temp_path("stamp"))
        .expect("open");
    let mut page = file.allocate_page();
    page.payload_mut().fill(0x5A);

    let _ = c.bench_function("page_write_4k_buffered", |b| {
        b.iter(|| {
            file.write_page(PageId::new(0), black_box(&mut page))
                .expect("write");
        });
    });
}

fn bench_read(c: &mut Criterion) {
    let file = PageFileOptions::new()
        .direct_io(false)
        .open(temp_path("read"))
        .expect("open");
    let mut page = file.allocate_page();
    page.set_lsn(Lsn::new(1));
    page.payload_mut().fill(0x33);
    file.write_page(PageId::new(0), &mut page).expect("write");
    file.sync().expect("sync");

    let _ = c.bench_function("page_read_4k_buffered", |b| {
        b.iter(|| {
            let p = file.read_page(black_box(PageId::new(0))).expect("read");
            black_box(p.lsn());
        });
    });
}

fn bench_write_sync(c: &mut Criterion) {
    let file = PageFileOptions::new()
        .direct_io(false)
        .open(temp_path("sync"))
        .expect("open");
    let mut page = file.allocate_page();
    page.payload_mut().fill(0x0F);

    let _ = c.bench_function("page_write_sync_4k", |b| {
        b.iter(|| {
            file.write_page(PageId::new(0), &mut page).expect("write");
            file.sync().expect("sync");
        });
    });
}

fn bench_pool_hit(c: &mut Criterion) {
    let file = PageFileOptions::new()
        .direct_io(false)
        .open(temp_path("pool-hit"))
        .expect("open");
    let pool = BufferPool::new(file, 16);
    {
        let guard = pool.new_page(PageId::new(0)).expect("new_page");
        guard.write().payload_mut().fill(0x11);
    }
    pool.flush_all().expect("flush");

    // The page is resident, so every fetch is a cache hit: no I/O, just the
    // lookup, pin, and read borrow.
    let _ = c.bench_function("pool_fetch_hit", |b| {
        b.iter(|| {
            let guard = pool.fetch(black_box(PageId::new(0))).expect("fetch");
            black_box(guard.read().payload()[0]);
        });
    });
}

fn bench_pool_miss(c: &mut Criterion) {
    let file = PageFileOptions::new()
        .direct_io(false)
        .open(temp_path("pool-miss"))
        .expect("open");
    // More pages than frames, so cycling through them forces an eviction and a
    // read on every fetch.
    let pages = 32u64;
    let pool = BufferPool::new(file, 4);
    for id in 0..pages {
        let guard = pool.new_page(PageId::new(id)).expect("new_page");
        let _ = guard;
    }
    pool.flush_all().expect("flush");

    let mut next = 0u64;
    let _ = c.bench_function("pool_fetch_miss_evict", |b| {
        b.iter(|| {
            let id = PageId::new(next % pages);
            next = next.wrapping_add(7); // stride to defeat the small cache
            let guard = pool.fetch(black_box(id)).expect("fetch");
            black_box(guard.read().payload()[0]);
        });
    });
}

fn bench_alloc(c: &mut Criterion) {
    let page_size = PageSize::new(4096).expect("valid");

    // Steady-state allocate/free of a single id: pop the free-list and push it
    // back, so the high-water mark and the free-list both stay warm.
    let alloc = PageAllocator::open(temp_path("alloc-free"), page_size).expect("open");
    let id = alloc.allocate().expect("allocate");
    let _ = c.bench_function("alloc_free_cycle", |b| {
        b.iter(|| {
            alloc.free(black_box(id)).expect("free");
            black_box(alloc.allocate().expect("allocate"));
        });
    });

    // Fresh allocation that extends the high-water mark (no free-list reuse).
    let alloc2 = PageAllocator::open(temp_path("alloc-extend"), page_size).expect("open");
    let _ = c.bench_function("alloc_extend", |b| {
        b.iter(|| {
            black_box(alloc2.allocate().expect("allocate"));
        });
    });
}

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("page-db-bench-{tag}-{}.pages", std::process::id()));
    path
}

criterion_group!(
    benches,
    bench_crc32c,
    bench_stamp,
    bench_read,
    bench_write_sync,
    bench_pool_hit,
    bench_pool_miss,
    bench_alloc
);
criterion_main!(benches);

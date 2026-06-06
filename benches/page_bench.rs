//! Criterion benchmarks for the page hot paths: checksumming, framing, and the
//! file read/write round-trip.
//!
//! Run with `cargo bench --bench page_bench`. The file benchmarks use buffered
//! I/O so the numbers reflect the framing and syscall cost without a disk-flush
//! barrier dominating every sample; `sync` is measured separately.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use page_db::{Lsn, PageFileOptions, PageId, crc32c};

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
    bench_write_sync
);
criterion_main!(benches);

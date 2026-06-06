//! Fuzz the page parse path: arbitrary bytes handed to `Page::from_bytes` and
//! `crc32c` must never panic or read out of bounds. When a page does parse, it
//! must round-trip through `to_checksummed_bytes` and back.

#![no_main]

use libfuzzer_sys::fuzz_target;
use page_db::{Page, PageSize};

fn drive(data: &[u8], size: usize) {
    let ps = match PageSize::new(size) {
        Ok(ps) => ps,
        Err(_) => return,
    };
    // Build a page-sized block from the fuzz input (truncated or zero-padded).
    let mut buf = vec![0u8; size];
    let n = data.len().min(size);
    buf[..n].copy_from_slice(&data[..n]);

    if let Ok(page) = Page::from_bytes(ps, &buf) {
        // A page that verified must survive a serialize / parse round-trip.
        let bytes = page.to_checksummed_bytes();
        let reparsed = Page::from_bytes(ps, &bytes).expect("round-trip must verify");
        assert_eq!(reparsed.id(), page.id());
        assert_eq!(reparsed.lsn(), page.lsn());
        assert_eq!(reparsed.payload(), page.payload());
    }
}

fuzz_target!(|data: &[u8]| {
    // The raw checksum must accept any input without panicking.
    let _ = page_db::crc32c(data);

    drive(data, 4096);
    drive(data, 8192);
});

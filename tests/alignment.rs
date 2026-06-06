//! Page-size and alignment edge cases: the boundaries of what `PageSize`
//! accepts, and that every accepted size round-trips through the file with
//! page-aligned offsets, including at high page ids.

use page_db::{
    Lsn, MAX_PAGE_SIZE, MIN_PAGE_SIZE, PAGE_HEADER_SIZE, Page, PageError, PageFileOptions, PageId,
    PageSize,
};

/// Every power of two in `MIN..=MAX` is accepted; everything else is rejected.
#[test]
fn page_size_accepts_exactly_the_valid_set() {
    // Powers of two in range, accepted.
    let mut size = MIN_PAGE_SIZE;
    while size <= MAX_PAGE_SIZE {
        let ps = PageSize::new(size).expect("power of two in range");
        assert_eq!(ps.get(), size);
        assert_eq!(ps.payload_len(), size - PAGE_HEADER_SIZE);
        size *= 2;
    }

    // Boundaries and non-powers-of-two, rejected.
    for bad in [
        0,
        1,
        MIN_PAGE_SIZE - 1,
        MIN_PAGE_SIZE + 1,
        2048,
        6144,
        MAX_PAGE_SIZE - 1,
        MAX_PAGE_SIZE + 1,
        MAX_PAGE_SIZE * 2,
    ] {
        assert!(
            matches!(PageSize::new(bad), Err(PageError::InvalidPageSize { size }) if size == bad),
            "size {bad} should be rejected"
        );
    }
}

/// Every valid page size round-trips in memory through the checksummed-bytes
/// framing, with the payload length the size implies.
#[test]
fn every_page_size_round_trips_in_memory() {
    let mut size = MIN_PAGE_SIZE;
    while size <= MAX_PAGE_SIZE {
        let ps = PageSize::new(size).expect("valid");
        let mut page = Page::new(ps);
        let last = page.payload().len() - 1;
        page.set_lsn(Lsn::new(size as u64));
        page.payload_mut()[0] = 0xE1;
        page.payload_mut()[last] = 0x1E;

        let bytes = page.to_checksummed_bytes();
        assert_eq!(bytes.len(), size);

        let loaded = Page::from_bytes(ps, &bytes).expect("verify");
        assert_eq!(loaded.lsn(), Lsn::new(size as u64));
        assert_eq!(loaded.payload()[0], 0xE1);
        assert_eq!(loaded.payload()[last], 0x1E);

        size *= 2;
    }
}

/// Writing at a non-trivial page id puts the page at a page-aligned offset and
/// reads back intact; the file length stays a whole number of pages. Kept to a
/// few MiB so the test does not allocate a large file on filesystems that do not
/// sparse by default.
#[test]
fn offset_arithmetic_round_trips_across_sizes() {
    for &size in &[MIN_PAGE_SIZE, 16384, 65536] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("off.pages");
        let ps = PageSize::new(size).expect("valid");
        let file = PageFileOptions::new()
            .page_size(ps)
            .direct_io(false)
            .open(&path)
            .expect("open");

        let id = PageId::new(64); // offset = 64 * size, aligned and non-trivial
        let mut page = file.allocate_page();
        page.payload_mut()[0] = 0x9;
        file.write_page(id, &mut page).expect("write");
        file.sync().expect("sync");

        let got = file.read_page(id).expect("read");
        assert_eq!(got.id(), id);
        assert_eq!(got.payload()[0], 0x9);

        // The file is exactly (id + 1) whole pages long.
        assert_eq!(file.page_count().expect("count"), 65);
    }
}

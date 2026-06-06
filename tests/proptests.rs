//! Property tests for the page-format and file invariants.
//!
//! These run buffered so they are deterministic and portable across the CI
//! filesystems; the Direct I/O path is covered by `tests/direct_io.rs`.

use std::path::Path;

use page_db::{Lsn, PageError, PageFile, PageFileOptions, PageId, PageSize};
use proptest::prelude::*;

const PAGE_SIZE: usize = 4096;
const PAYLOAD: usize = PAGE_SIZE - 32;

fn open(path: &Path) -> PageFile {
    PageFileOptions::new()
        .page_size(PageSize::new(PAGE_SIZE).expect("valid"))
        .direct_io(false)
        .open(path)
        .expect("open")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// A written page reads back byte-for-byte, with its lsn and slot id intact.
    #[test]
    fn write_read_roundtrip(
        slot in 0u64..256,
        lsn in any::<u64>(),
        payload in proptest::collection::vec(any::<u8>(), 0..=PAYLOAD),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = open(&dir.path().join("p.pages"));

        let mut page = file.allocate_page();
        page.set_lsn(Lsn::new(lsn));
        page.payload_mut()[..payload.len()].copy_from_slice(&payload);
        file.write_page(PageId::new(slot), &mut page).expect("write");

        let got = file.read_page(PageId::new(slot)).expect("read");
        prop_assert_eq!(got.id(), PageId::new(slot));
        prop_assert_eq!(got.lsn(), Lsn::new(lsn));
        prop_assert_eq!(&got.payload()[..payload.len()], &payload[..]);
    }

    /// Any single-byte corruption of a stored page is detected on read — it is
    /// never returned as a successful read of wrong data.
    #[test]
    fn single_byte_corruption_is_always_caught(
        offset in 0usize..PAGE_SIZE,
        xor in 1u8..=255,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.pages");
        {
            let file = open(&path);
            let mut page = file.allocate_page();
            page.payload_mut()[0] = 0xA5;
            file.write_page(PageId::new(0), &mut page).expect("write");
            file.sync().expect("sync");
        }

        // Corrupt one byte directly in the file.
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut raw = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .expect("reopen");
            raw.seek(SeekFrom::Start(offset as u64)).expect("seek");
            let mut b = [0u8; 1];
            raw.read_exact(&mut b).expect("read");
            b[0] ^= xor;
            raw.seek(SeekFrom::Start(offset as u64)).expect("seek");
            raw.write_all(&b).expect("write");
            raw.sync_all().expect("sync");
        }

        let file = open(&path);
        prop_assert!(file.read_page(PageId::new(0)).is_err());
    }

    /// Many pages written to distinct slots all read back correctly, with no
    /// cross-talk between slots.
    #[test]
    fn many_pages_no_cross_talk(
        markers in proptest::collection::vec(any::<u8>(), 1..32),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = open(&dir.path().join("p.pages"));

        for (slot, &marker) in markers.iter().enumerate() {
            let mut page = file.allocate_page();
            page.payload_mut()[0] = marker;
            page.set_lsn(Lsn::new(slot as u64));
            file.write_page(PageId::new(slot as u64), &mut page).expect("write");
        }

        for (slot, &marker) in markers.iter().enumerate() {
            let page = file.read_page(PageId::new(slot as u64)).expect("read");
            prop_assert_eq!(page.payload()[0], marker);
            prop_assert_eq!(page.lsn(), Lsn::new(slot as u64));
        }
    }

    /// Reading a slot past the end of the file is a short read, never a panic or
    /// a bogus page.
    #[test]
    fn read_beyond_end_is_short_read(slot in 1u64..1024) {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = open(&dir.path().join("p.pages"));
        let is_short = matches!(
            file.read_page(PageId::new(slot)),
            Err(PageError::ShortRead { .. })
        );
        prop_assert!(is_short);
    }
}

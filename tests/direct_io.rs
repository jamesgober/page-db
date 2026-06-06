//! Direct I/O round-trip against a real file.
//!
//! The unit tests run buffered for portability; this exercises the actual
//! cache-bypass path — `FILE_FLAG_NO_BUFFERING` on Windows, `O_DIRECT` on Linux,
//! `F_NOCACHE` on macOS. If the test filesystem rejects Direct I/O (some overlay
//! and network filesystems do), the test reports that and returns rather than
//! failing — there is nothing to verify where the feature is unavailable.

use page_db::{Lsn, PageFile, PageFileOptions, PageId, PageSize};

#[test]
fn direct_io_write_read_sync_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("direct.pages");

    let page_size = PageSize::new(4096).expect("valid");
    let file = match PageFileOptions::new()
        .page_size(page_size)
        .direct_io(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) => {
            eprintln!("skipping: filesystem does not support Direct I/O: {err}");
            return;
        }
    };

    // Write a handful of pages, each tagged distinctly, then make them durable.
    for id in 0..8u64 {
        let mut page = file.allocate_page();
        page.set_lsn(Lsn::new(id + 1));
        let marker = [id as u8; 16];
        page.payload_mut()[..16].copy_from_slice(&marker);
        file.write_page(PageId::new(id), &mut page).expect("write");
    }
    file.sync().expect("sync");

    assert_eq!(file.page_count().expect("count"), 8);

    // Reopen with a fresh handle to defeat any in-process caching, then verify.
    drop(file);
    let file = PageFile::open(&path, page_size).expect("reopen");
    for id in 0..8u64 {
        let page = file.read_page(PageId::new(id)).expect("read");
        assert_eq!(page.id(), PageId::new(id));
        assert_eq!(page.lsn(), Lsn::new(id + 1));
        assert_eq!(&page.payload()[..16], &[id as u8; 16]);
    }
}

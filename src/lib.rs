//! # page-db
//!
//! The paging substrate beneath B-tree and heap storage engines - fixed-size pages, CRC32 headers with LSN slots, an LRU buffer pool with dirty-page pinning, and cross-platform Direct I/O.
//!
//! Scaffold release (v0.1.0). The public surface is being designed across the
//! 0.x series and frozen at v1.0. See `docs/API.md` and `dev/ROADMAP.md`.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(1 + 1, 2);
    }
}

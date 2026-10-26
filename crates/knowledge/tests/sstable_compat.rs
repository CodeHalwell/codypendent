//! Persisted search indexes retain their SSTable format across the Zstd security backport.

use tantivy::directory::OwnedBytes;
use tantivy_sstable::{Dictionary, MonotonicU64SSTable};

#[test]
fn legacy_sstable_from_zstd_013_remains_readable() {
    let bytes = include_bytes!("data/legacy-zstd-013.sstable");
    assert_eq!(
        bytes[4], 1,
        "the legacy fixture must exercise compressed blocks"
    );
    let dictionary = Dictionary::<MonotonicU64SSTable>::from_bytes(OwnedBytes::new(bytes.to_vec()))
        .expect("open an index written before the compression security update");
    assert_eq!(dictionary.num_terms(), 4096);
    for ordinal in 0..4096_u64 {
        assert_eq!(
            dictionary
                .get(format!("legacy-entry-{ordinal:06}"))
                .expect("read legacy term"),
            Some(ordinal * 2),
        );
    }
    assert_eq!(
        dictionary.get("missing-entry").expect("read missing term"),
        None
    );
}

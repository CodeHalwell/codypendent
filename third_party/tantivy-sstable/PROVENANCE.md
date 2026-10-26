# Tantivy SSTable security backport

Upstream: https://github.com/quickwit-oss/tantivy

Crate: tantivy-sstable 0.3.0

Published crate SHA-256: `c69578242e8e9fc989119f522ba5b49a38ac20f576fc778035b96cc94f41f98e`

Upstream source revision: `2fb381be70c03d5f84dadcaf89ae2fcb4170c8c1`

The upstream Rust source is unchanged. The normalized Cargo manifest selects Zstd >=0.14.3 instead of 0.13, which removes RUSTSEC-2026-0391, RUSTSEC-2026-0398, RUSTSEC-2026-0403 and RUSTSEC-2026-0410 through patched Zstd, zstd-safe and zstd-sys dependencies. The existing Tantivy 0.22/SSTable 0.3 index format is retained. LICENSE is copied from the matching upstream Tantivy distribution.

This path override is declared in both Cargo workspaces. Remove the backport when a compatible upstream SSTable release uses patched Zstd.

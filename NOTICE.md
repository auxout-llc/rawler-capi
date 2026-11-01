# rawler-capi

rawler-capi is a C ABI wrapper around rawler, built as the dynamic library
`librawler_capi` (`.dylib` / `.so` / `.dll`).

- License: GNU Lesser General Public License v2.1 (`LICENSE`), the license of rawler.
- rawler: https://github.com/dnglab/dnglab (crate `rawler`), pinned to commit
  `1fcc4a5607591178d2b335845a7e5ee09f89a37f` (2026-10-30).
  Source: https://github.com/dnglab/dnglab/tree/1fcc4a5607591178d2b335845a7e5ee09f89a37f/rawler
- Source of this wrapper: https://github.com/auxout/rawler-capi . Each app release links to
  the tag of the exact commit its shipped binaries were built from.

You may replace the shipped `librawler_capi` with your own build of rawler-capi
(or of a modified rawler) as long as it keeps the C ABI (`rlc_abi_version()` = 2,
`include/rawler_capi.h`). Build: `cargo build --release` in this directory.

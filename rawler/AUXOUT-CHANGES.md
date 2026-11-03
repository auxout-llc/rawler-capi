# Changes to rawler

This directory is rawler (https://github.com/dnglab/dnglab, crate `rawler`) at commit
`1fcc4a5607591178d2b335845a7e5ee09f89a37f` (2026-10-30), LGPL-2.1 (`LICENSE`), with the
changes below. rawler-capi uses it through `[patch]` in `../Cargo.toml`.

Left out of the copy (not needed to build the library): `data/testdata/`, `tests/`, `fuzz/`.

| Date | File | Change |
|---|---|---|
| 2026-11-03 | `Cargo.toml` | The dnglab workspace lints (`[lints] workspace = true`) are inlined, as this crate is built outside that workspace |
| 2026-11-03 | `data/cameras/nikon/d810a.toml` | Added the Nikon D810A, with the layout and white levels of the D810. The color matrix is the D810's as a placeholder |

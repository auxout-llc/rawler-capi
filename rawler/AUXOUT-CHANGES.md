# Changes to rawler

This directory is rawler (https://github.com/dnglab/dnglab, crate `rawler`) at commit
`1fcc4a5607591178d2b335845a7e5ee09f89a37f` (2026-10-30), LGPL-2.1 (`LICENSE`), with the
changes below. rawler-capi uses it through `[patch]` in `../Cargo.toml`.

Left out of the copy (not needed to build the library): `data/testdata/`, `tests/`, `fuzz/`.

| Date | File | Change |
|---|---|---|
| 2026-11-03 | `Cargo.toml` | The dnglab workspace lints (`[lints] workspace = true`) are inlined, as this crate is built outside that workspace |
| 2026-11-03 | `data/cameras/nikon/d810a.toml` | Added the Nikon D810A, with the layout and white levels of the D810. The color matrix is the D810's as a placeholder |
| 2026-11-03 | `src/formats/tiff/entry.rs`, `src/formats/tiff/reader.rs` | A TIFF tag whose value would not fit in the rest of the file is refused (`TiffError::Overflow`) before its values are allocated (`EndianReader::remaining`). A bogus count made the reader allocate up to 32 GB |
| 2026-11-03 | `src/decoders/crw.rs`, `src/decoders/mos.rs`, `src/decoders/iiq.rs`, `src/decoders/mod.rs`, `src/formats/ciff/mod.rs` | The orientation is read from where these formats record it instead of an EXIF block: Canon CRW ImageInfo (0x1810) rotation, Leaf `ImgProf_rotation_angle`, Phase One makernote 0x100. Degrees map to EXIF as dcraw / LibRaw do (`exif_orientation_from_degrees`) |
| 2026-11-03 | `src/decoders/rw2.rs`, `src/decoders/arw.rs` | The crop for the aspect ratio chosen in the camera (1:1, 16:9, ...) is the crop area: Panasonic `CropLeft`..`CropBottom` when inside the active area (before, the active area replaced it), Sony `SonyCropTopLeft` / `SonyCropSize` ahead of DefaultCrop. A Panasonic crop that ends before it starts is ignored instead of underflowing |
| 2026-11-07 | `src/rawsource.rs`, `src/decoders/{ari,dcr,dcs,kdc,mod,mrw,nef,orf,qtk,raf,rw2,srw,unwrapped,x3f}.rs`, `src/formats/tiff/entry.rs` | Decoding the header reads the header. `RawSource::new` no longer advises `WillNeed` / `Sequential` (on macOS that fetches the whole file): `RawSource::advise_full_read` does, and rawler-capi calls it before decoding pixels. The padded subviews take `dummy` and give untouched zeros of the same length instead of copying the end of the file. MRW, X3F and picking a decoder by file size read the file through the mapping instead of a copy. An ExifIFD MakerNotes over 4 MB (Phase One's spans the image data) keeps its first 4 KB |

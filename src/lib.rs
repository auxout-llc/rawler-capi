// SPDX-License-Identifier: LGPL-2.1
//! rawler-capi: a plain C ABI over [rawler](https://github.com/dnglab/dnglab).
//!
//! This library only exposes what rawler itself does: open a RAW file, read the
//! generalized metadata, decode the sensor data (mosaic + levels + color
//! matrices + DNG opcode lists), apply DNG OpcodeList1 / LinearizationTable,
//! and run rawler's demosaic algorithms on a caller-provided buffer.
//! Product-specific processing lives in the callers.
//!
//! Conventions:
//! - Every function returns `RLC_OK` (0) or a negative `RLC_ERR_*` code, and
//!   writes a NUL-terminated message into `err` (may be NULL) on failure.
//! - Objects (`rlc_file`, `rlc_image`) are created and freed by this library.
//!   A handle must not be used from two threads at the same time; different
//!   handles are independent.
//! - Panics inside rawler are caught and returned as `RLC_ERR_PANIC`.
//! - `threads` parameters: 0 = rayon's global pool (RAYON_NUM_THREADS or the
//!   number of CPUs), N > 0 = a pool of N threads owned by this library.
#![allow(clippy::missing_safety_doc)]

use std::collections::HashMap;
use std::ffi::{CStr, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use rawler::cfa::PlaneColor;
use rawler::decoders::{Decoder, FormatHint, Orientation, RawDecodeParams};
use rawler::dng::opcodes::OpcodeStage;
use rawler::formats::tiff::{Rational, SRational};
use rawler::imgop::sensor::Demosaic;
use rawler::imgop::sensor::bayer::ppg::PPGDemosaic;
use rawler::imgop::sensor::xtrans::bilinear::XTransBilinearDemosaic;
use rawler::imgop::sensor::xtrans::markesteijn::XTransMarkesteijnDemosaic;
use rawler::imgop::{Dim2, Point, Rect};
use rawler::pixarray::ImageView;
use rawler::rawimage::RawPhotometricInterpretation;
use rawler::rawsource::RawSource;
use rawler::{CFA, RawImage, RawImageData, RawlerError};

/// Version of this C ABI. Bumped on every incompatible change of a struct or
/// function signature. Callers check it before using the library.
pub const RLC_ABI_VERSION: u32 = 2;

pub const RLC_OK: i32 = 0;
pub const RLC_ERR_INVALID_ARGUMENT: i32 = -1;
/// The file could not be opened or read.
pub const RLC_ERR_IO: i32 = -2;
/// rawler does not recognize the file format.
pub const RLC_ERR_FORMAT_UNKNOWN: i32 = -3;
/// The format is known but the camera / mode is not supported for decoding.
pub const RLC_ERR_CAMERA_UNSUPPORTED: i32 = -4;
/// Decoding failed (corrupt or truncated file, unsupported compression, ...).
pub const RLC_ERR_DECODE: i32 = -5;
/// rawler panicked; the operation was aborted.
pub const RLC_ERR_PANIC: i32 = -6;
/// The request does not apply to this data (e.g. PPG on an X-Trans CFA).
pub const RLC_ERR_UNSUPPORTED: i32 = -7;

pub const RLC_DATA_NONE: i32 = 0;
pub const RLC_DATA_U16: i32 = 1;
pub const RLC_DATA_F32: i32 = 2;

pub const RLC_PHOTOMETRIC_BLACK_IS_ZERO: i32 = 0;
pub const RLC_PHOTOMETRIC_CFA: i32 = 1;
pub const RLC_PHOTOMETRIC_LINEAR_RAW: i32 = 2;

// rawler's format hint (`rlc_metadata.format`).
pub const RLC_FORMAT_UNKNOWN: i32 = 0;
pub const RLC_FORMAT_CR2: i32 = 1;
pub const RLC_FORMAT_CR3: i32 = 2;
pub const RLC_FORMAT_CRW: i32 = 3;
pub const RLC_FORMAT_NEF: i32 = 4;
pub const RLC_FORMAT_ARW: i32 = 5;
pub const RLC_FORMAT_RAF: i32 = 6;
pub const RLC_FORMAT_RW2: i32 = 7;
pub const RLC_FORMAT_ARI: i32 = 8;
pub const RLC_FORMAT_DNG: i32 = 9;
pub const RLC_FORMAT_DCR: i32 = 10;
pub const RLC_FORMAT_DCS: i32 = 11;
pub const RLC_FORMAT_ERF: i32 = 12;
pub const RLC_FORMAT_IIQ: i32 = 13;
pub const RLC_FORMAT_KDC: i32 = 14;
pub const RLC_FORMAT_MEF: i32 = 15;
pub const RLC_FORMAT_MOS: i32 = 16;
pub const RLC_FORMAT_MRW: i32 = 17;
pub const RLC_FORMAT_NRW: i32 = 18;
pub const RLC_FORMAT_ORF: i32 = 19;
pub const RLC_FORMAT_PEF: i32 = 20;
pub const RLC_FORMAT_QTK: i32 = 21;
pub const RLC_FORMAT_SRW: i32 = 22;
pub const RLC_FORMAT_TFR: i32 = 23;
pub const RLC_FORMAT_X3F: i32 = 24;

/// Bayer: PPG. Input must be an RGB 2x2 CFA.
pub const RLC_DEMOSAIC_PPG: i32 = 1;
/// X-Trans: Markesteijn, 1 pass.
pub const RLC_DEMOSAIC_MARKESTEIJN_1: i32 = 2;
/// X-Trans: Markesteijn, 3 passes.
pub const RLC_DEMOSAIC_MARKESTEIJN_3: i32 = 3;
/// X-Trans: bilinear.
pub const RLC_DEMOSAIC_XTRANS_BILINEAR: i32 = 4;

/// Opaque: an opened RAW file (memory mapped) and its rawler decoder.
pub struct RlcFile {
  source: RawSource,
  decoder: Box<dyn Decoder>,
}

/// Opaque: a decoded RAW image (rawler's `RawImage`).
pub struct RlcImage {
  raw: RawImage,
  opcodes: [Option<Vec<u8>>; 3],
  is_dng: bool,
}

/// Generalized metadata (rawler's `RawMetadata` / `Exif`). Strings are UTF-8,
/// NUL-terminated, empty when unknown. Numbers are NaN when unknown.
#[repr(C)]
pub struct RlcMetadata {
  /// rawler's format hint (`RLC_FORMAT_*`).
  pub format: i32,
  /// Make and model, cleaned up by rawler's camera database ("Sony", "ILCE-7M5").
  pub make: [c_char; 64],
  pub model: [c_char; 64],
  pub lens_make: [c_char; 64],
  pub lens_model: [c_char; 128],
  /// EXIF date strings as stored ("YYYY:MM:DD HH:MM:SS").
  pub date_time_original: [c_char; 32],
  pub create_date: [c_char; 32],
  pub modify_date: [c_char; 32],
  pub offset_time: [c_char; 16],
  pub offset_time_original: [c_char; 16],
  /// EXIF orientation 1..8, 0 when absent.
  pub orientation: i32,
  /// -1 when absent.
  pub rating: i32,
  pub exposure_time: f64,
  pub fnumber: f64,
  pub aperture_value: f64,
  pub focal_length: f64,
  pub exposure_bias: f64,
  /// ISOSpeedRatings, else ISOSpeed, else RecommendedExposureIndex.
  pub iso: f64,
  /// LensSpecification: min/max focal length, min/max f-number.
  pub lens_spec: [f64; 4],
}

/// Description of a decoded image. Pointers are borrowed from the `rlc_image`
/// and valid until it is freed or modified (`rlc_image_apply_*`).
#[repr(C)]
pub struct RlcImageDesc {
  /// Make and model as encoded in the file (not cleaned up), UTF-8.
  pub make: [c_char; 64],
  pub model: [c_char; 64],
  /// Full sensor data size in pixels (before any crop).
  pub width: i32,
  pub height: i32,
  /// Components per pixel (1 for CFA / monochrome, 3 for LinearRaw RGB).
  pub cpp: i32,
  pub bps: i32,
  /// `RLC_DATA_*`. NONE for a header-only decode.
  pub data_type: i32,
  /// `width * height * cpp` samples, row-major.
  pub data: *const std::ffi::c_void,
  /// `RLC_PHOTOMETRIC_*`.
  pub photometric: i32,
  /// CFA repeat size (2x2 Bayer, 6x6 X-Trans), 0 when not CFA. The pattern is
  /// aligned to the origin of `data` (not to the active area).
  pub cfa_width: i32,
  pub cfa_height: i32,
  /// Colors row-major, rawler's CFAColor codes (`CFA::flat_pattern`): 0 R, 1 G,
  /// 2 B, 3 C, 4 M, 5 Y, 6 W (E), 7 Fuji green, 255 unknown.
  pub cfa: [u8; 36],
  /// 1 when the CFA uses exactly R, G and B.
  pub cfa_is_rgb: i32,
  /// Black / white level per 2x2 position (index (row % 2) * 2 + col % 2),
  /// relative to the origin of `data`. For LinearRaw: per component.
  pub black: [f32; 4],
  pub white: [f32; 4],
  /// As-shot white balance coefficients in R, G, B, E order as rawler reports
  /// them (NaN / 0 when unknown).
  pub wb_coeffs: [f32; 4],
  /// Active area / default crop as x, y, width, height relative to `data`.
  /// width = 0 when absent.
  pub active_area: [i32; 4],
  pub crop_area: [i32; 4],
  /// Orientation from the image data (DNG Orientation), EXIF value 1..8, 0 unknown.
  /// For non-DNG files rawler reports 1; the EXIF value is in `rlc_metadata`.
  pub orientation: i32,
  /// Fuji SuperCCD rotated sensor split point, 0 when not rotated.
  pub fuji_rotation_width: i32,
  /// DNG BaselineExposure (EV), NaN when absent.
  pub baseline_exposure: f32,
  /// 1 when the file is a DNG (rawler's DNG decoder).
  pub is_dng: i32,
  /// Color matrices (XYZ -> camera) of the as-shot camera profile, up to 3
  /// calibration slots. Illuminant: EXIF LightSource number (17 A, 21 D65, 0 none).
  /// `color_matrix_rows` is 3 or 4 (rows = camera planes), 0 for an empty slot.
  pub color_matrix_count: i32,
  pub illuminant: [i32; 3],
  pub color_matrix_rows: [i32; 3],
  pub color_matrix: [[f32; 12]; 3],
  /// Index (0..2) of the matrix rawler prefers for D65 calculations, -1 none.
  pub best_color_matrix: i32,
  /// DNG AnalogBalance (NaN when absent).
  pub analog_balance: [f32; 4],
  /// Entries of a not yet applied DNG LinearizationTable, 0 when none.
  pub linearization_len: i32,
  /// Byte lengths of the DNG opcode lists 1..3 (0 = none), see `rlc_image_opcodes`.
  pub opcode_list_len: [i32; 3],
  /// DNG CameraCalibration of each `color_matrix` slot (3x3, row major), when
  /// the file has one that applies to the as-shot profile (the
  /// CameraCalibrationSignature matches). 0 / zeros otherwise. The DNG
  /// XYZ -> camera matrix is AnalogBalance * CameraCalibration * ColorMatrix.
  /// (ABI 2)
  pub has_camera_calibration: [i32; 3],
  pub camera_calibration: [[f32; 9]; 3],
}

/// CFA pattern for `rlc_demosaic`, aligned to the origin of the mosaic buffer.
#[repr(C)]
pub struct RlcCfa {
  pub width: i32,
  pub height: i32,
  /// Colors row-major: 0 R, 1 G, 2 B.
  pub pattern: [u8; 36],
}

mod layout;

// The structs as cbindgen writes them into the header = the layout table.
#[cfg(target_pointer_width = "64")]
const _: () = {
  use std::mem::{offset_of, size_of};
  assert!(size_of::<RlcMetadata>() == layout::RLC_METADATA_SIZE);
  assert!(offset_of!(RlcMetadata, iso) == layout::RLC_METADATA_ISO);
  assert!(offset_of!(RlcMetadata, lens_spec) == layout::RLC_METADATA_LENS_SPEC);
  assert!(size_of::<RlcImageDesc>() == layout::RLC_IMAGE_DESC_SIZE);
  assert!(offset_of!(RlcImageDesc, data) == layout::RLC_IMAGE_DESC_DATA);
  assert!(offset_of!(RlcImageDesc, cfa) == layout::RLC_IMAGE_DESC_CFA);
  assert!(offset_of!(RlcImageDesc, black) == layout::RLC_IMAGE_DESC_BLACK);
  assert!(offset_of!(RlcImageDesc, color_matrix) == layout::RLC_IMAGE_DESC_COLOR_MATRIX);
  assert!(offset_of!(RlcImageDesc, analog_balance) == layout::RLC_IMAGE_DESC_ANALOG_BALANCE);
  assert!(offset_of!(RlcImageDesc, opcode_list_len) == layout::RLC_IMAGE_DESC_OPCODE_LIST_LEN);
  assert!(offset_of!(RlcImageDesc, has_camera_calibration) == layout::RLC_IMAGE_DESC_HAS_CAMERA_CALIBRATION);
  assert!(offset_of!(RlcImageDesc, camera_calibration) == layout::RLC_IMAGE_DESC_CAMERA_CALIBRATION);
  assert!(size_of::<RlcCfa>() == layout::RLC_CFA_SIZE);
  assert!(offset_of!(RlcCfa, pattern) == layout::RLC_CFA_PATTERN);
};

// ---------------------------------------------------------------------------
// helpers

/// The longest prefix of `s` that fits `cap - 1` bytes (room for the NUL) on
/// a UTF-8 boundary.
fn c_prefix(s: &str, cap: usize) -> &[u8] {
  let mut n = s.len().min(cap.saturating_sub(1));
  while n > 0 && !s.is_char_boundary(n) {
    n -= 1;
  }
  &s.as_bytes()[..n]
}

fn set_err(err: *mut c_char, len: i32, msg: &str) {
  if err.is_null() || len <= 0 {
    return;
  }
  let b = c_prefix(msg, len as usize);
  unsafe {
    std::ptr::copy_nonoverlapping(b.as_ptr() as *const c_char, err, b.len());
    *err.add(b.len()) = 0;
  }
}

fn copy_str<const N: usize>(dst: &mut [c_char; N], s: &str) {
  let b = c_prefix(s, N);
  for (d, v) in dst.iter_mut().zip(b) {
    *d = *v as c_char;
  }
  dst[b.len()] = 0;
}

fn guard(err: *mut c_char, len: i32, f: impl FnOnce() -> Result<(), (i32, String)>) -> i32 {
  match catch_unwind(AssertUnwindSafe(f)) {
    Ok(Ok(())) => RLC_OK,
    Ok(Err((code, msg))) => {
      set_err(err, len, &msg);
      code
    }
    Err(p) => {
      let msg = p
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".into());
      set_err(err, len, &format!("rawler panicked: {msg}"));
      RLC_ERR_PANIC
    }
  }
}

fn decode_err(e: RawlerError) -> (i32, String) {
  match e {
    RawlerError::Unsupported { .. } => (RLC_ERR_CAMERA_UNSUPPORTED, e.to_string()),
    RawlerError::DecoderFailed(_) => (RLC_ERR_DECODE, e.to_string()),
  }
}

fn pools() -> &'static Mutex<HashMap<usize, Arc<rayon::ThreadPool>>> {
  static POOLS: OnceLock<Mutex<HashMap<usize, Arc<rayon::ThreadPool>>>> = OnceLock::new();
  POOLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run `f` in the pool for `threads` (0 = global pool).
fn in_pool<R: Send>(threads: i32, f: impl FnOnce() -> R + Send) -> R {
  if threads <= 0 {
    return f();
  }
  let n = threads as usize;
  let pool = {
    let mut map = pools().lock().unwrap_or_else(|e| e.into_inner());
    match map.get(&n) {
      Some(p) => Some(p.clone()),
      None => match rayon::ThreadPoolBuilder::new().num_threads(n).build() {
        Ok(p) => {
          let p = Arc::new(p);
          map.insert(n, p.clone());
          Some(p)
        }
        Err(_) => None,
      },
    }
  };
  match pool {
    Some(p) => p.install(f),
    None => f(),
  }
}

struct AssertSend<T>(T);
unsafe impl<T> Send for AssertSend<T> {}

/// A rational as f64 (NaN when absent or the denominator is 0). Works for
/// rawler's unsigned and signed rationals.
fn rational<R: RationalValue>(r: Option<&R>) -> f64 {
  r.map(RationalValue::value).unwrap_or(f64::NAN)
}

trait RationalValue {
  fn value(&self) -> f64;
}
impl RationalValue for Rational {
  fn value(&self) -> f64 {
    if self.d == 0 { f64::NAN } else { self.n as f64 / self.d as f64 }
  }
}
impl RationalValue for SRational {
  fn value(&self) -> f64 {
    if self.d == 0 { f64::NAN } else { self.n as f64 / self.d as f64 }
  }
}

fn rect4(r: &Option<Rect>) -> [i32; 4] {
  match r {
    Some(r) => [r.p.x as i32, r.p.y as i32, r.d.w as i32, r.d.h as i32],
    None => [0; 4],
  }
}

fn orientation_value(o: Orientation) -> i32 {
  match o {
    Orientation::Unknown => 0,
    o => o.to_u16() as i32,
  }
}

fn format_code(h: FormatHint) -> i32 {
  use FormatHint::*;
  match h {
    Unknown => RLC_FORMAT_UNKNOWN,
    CR2 => RLC_FORMAT_CR2,
    CR3 => RLC_FORMAT_CR3,
    CRW => RLC_FORMAT_CRW,
    NEF => RLC_FORMAT_NEF,
    ARW => RLC_FORMAT_ARW,
    RAF => RLC_FORMAT_RAF,
    RW2 => RLC_FORMAT_RW2,
    ARI => RLC_FORMAT_ARI,
    DNG => RLC_FORMAT_DNG,
    DCR => RLC_FORMAT_DCR,
    DCS => RLC_FORMAT_DCS,
    ERF => RLC_FORMAT_ERF,
    IIQ => RLC_FORMAT_IIQ,
    KDC => RLC_FORMAT_KDC,
    MEF => RLC_FORMAT_MEF,
    MOS => RLC_FORMAT_MOS,
    MRW => RLC_FORMAT_MRW,
    NRW => RLC_FORMAT_NRW,
    ORF => RLC_FORMAT_ORF,
    PEF => RLC_FORMAT_PEF,
    QTK => RLC_FORMAT_QTK,
    SRW => RLC_FORMAT_SRW,
    TFR => RLC_FORMAT_TFR,
    X3F => RLC_FORMAT_X3F,
  }
}

fn path_arg(path: *const c_char) -> Result<String, (i32, String)> {
  if path.is_null() {
    return Err((RLC_ERR_INVALID_ARGUMENT, "null path".into()));
  }
  unsafe { CStr::from_ptr(path) }
    .to_str()
    .map(|s| s.to_string())
    .map_err(|_| (RLC_ERR_INVALID_ARGUMENT, "path is not UTF-8".into()))
}

// ---------------------------------------------------------------------------
// library

/// `RLC_ABI_VERSION` of this binary.
#[unsafe(no_mangle)]
pub extern "C" fn rlc_abi_version() -> u32 {
  RLC_ABI_VERSION
}

/// "rawler-capi <version> (rawler <commit>)", static string.
#[unsafe(no_mangle)]
pub extern "C" fn rlc_version() -> *const c_char {
  static V: &str = concat!("rawler-capi ", env!("CARGO_PKG_VERSION"), " (rawler 1fcc4a5607591178d2b335845a7e5ee09f89a37f)\0");
  V.as_ptr() as *const c_char
}

// ---------------------------------------------------------------------------
// file

/// Open `path` (UTF-8) and pick rawler's decoder. Does not decode pixels.
/// Errors: IO, FORMAT_UNKNOWN.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_open(path: *const c_char, out: *mut *mut RlcFile, err: *mut c_char, err_len: i32) -> i32 {
  guard(err, err_len, || {
    if out.is_null() {
      return Err((RLC_ERR_INVALID_ARGUMENT, "null out".into()));
    }
    unsafe { *out = std::ptr::null_mut() };
    let path = path_arg(path)?;
    let source = RawSource::new(Path::new(&path)).map_err(|e| (RLC_ERR_IO, format!("open {path}: {e}")))?;
    let decoder = rawler::get_decoder(&source).map_err(|e| match e {
      RawlerError::Unsupported { .. } => (RLC_ERR_FORMAT_UNKNOWN, e.to_string()),
      RawlerError::DecoderFailed(_) => (RLC_ERR_DECODE, e.to_string()),
    })?;
    let file = Box::new(RlcFile { source, decoder });
    unsafe { *out = Box::into_raw(file) };
    Ok(())
  })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_close(file: *mut RlcFile) {
  if !file.is_null() {
    drop(unsafe { Box::from_raw(file) });
  }
}

/// RLC_OK when rawler can decode the raw image of this camera / mode,
/// CAMERA_UNSUPPORTED otherwise. Metadata is available either way.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_check_supported(file: *const RlcFile, err: *mut c_char, err_len: i32) -> i32 {
  guard(err, err_len, || {
    let file = unsafe { file.as_ref() }.ok_or((RLC_ERR_INVALID_ARGUMENT, "null file".to_string()))?;
    file.decoder.check_supported().map_err(decode_err)
  })
}

/// Size of the mapped file in bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_file_size(file: *const RlcFile) -> u64 {
  unsafe { file.as_ref() }.map(|f| f.source.buf().len() as u64).unwrap_or(0)
}

/// Generalized metadata.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_read_metadata(file: *const RlcFile, out: *mut RlcMetadata, err: *mut c_char, err_len: i32) -> i32 {
  guard(err, err_len, || {
    let file = unsafe { file.as_ref() }.ok_or((RLC_ERR_INVALID_ARGUMENT, "null file".to_string()))?;
    let out = unsafe { out.as_mut() }.ok_or((RLC_ERR_INVALID_ARGUMENT, "null out".to_string()))?;
    let md = file.decoder.raw_metadata(&file.source, &RawDecodeParams::default()).map_err(decode_err)?;
    let e = &md.exif;
    let mut m = RlcMetadata {
      format: format_code(file.decoder.format_hint()),
      make: [0; 64],
      model: [0; 64],
      lens_make: [0; 64],
      lens_model: [0; 128],
      date_time_original: [0; 32],
      create_date: [0; 32],
      modify_date: [0; 32],
      offset_time: [0; 16],
      offset_time_original: [0; 16],
      orientation: e.orientation.map(|o| o as i32).unwrap_or(0),
      rating: md.rating.map(|r| r as i32).unwrap_or(-1),
      exposure_time: rational(e.exposure_time.as_ref()),
      fnumber: rational(e.fnumber.as_ref()),
      aperture_value: rational(e.aperture_value.as_ref()),
      focal_length: rational(e.focal_length.as_ref()),
      exposure_bias: rational(e.exposure_bias.as_ref()),
      iso: e
        .iso_speed_ratings
        .map(|v| v as f64)
        .or(e.iso_speed.map(|v| v as f64))
        .or(e.recommended_exposure_index.map(|v| v as f64))
        .unwrap_or(f64::NAN),
      lens_spec: [f64::NAN; 4],
    };
    copy_str(&mut m.make, &md.make);
    copy_str(&mut m.model, &md.model);
    copy_str(&mut m.lens_make, e.lens_make.as_deref().unwrap_or(""));
    copy_str(&mut m.lens_model, e.lens_model.as_deref().unwrap_or(""));
    copy_str(&mut m.date_time_original, e.date_time_original.as_deref().unwrap_or(""));
    copy_str(&mut m.create_date, e.create_date.as_deref().unwrap_or(""));
    copy_str(&mut m.modify_date, e.modify_date.as_deref().unwrap_or(""));
    copy_str(&mut m.offset_time, e.offset_time.as_deref().unwrap_or(""));
    copy_str(&mut m.offset_time_original, e.offset_time_original.as_deref().unwrap_or(""));
    if let Some(spec) = &e.lens_spec {
      for (d, r) in m.lens_spec.iter_mut().zip(spec.iter()) {
        *d = rational(Some(r));
      }
    }
    *out = m;
    Ok(())
  })
}

// ---------------------------------------------------------------------------
// image

/// Decode the raw image. `header_only` = 1 runs rawler's dummy decode (levels,
/// matrices, sizes, no pixel data; much faster). `threads`: see the module doc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_decode(
  file: *const RlcFile,
  header_only: i32,
  threads: i32,
  out: *mut *mut RlcImage,
  err: *mut c_char,
  err_len: i32,
) -> i32 {
  guard(err, err_len, || {
    let file = unsafe { file.as_ref() }.ok_or((RLC_ERR_INVALID_ARGUMENT, "null file".to_string()))?;
    if out.is_null() {
      return Err((RLC_ERR_INVALID_ARGUMENT, "null out".into()));
    }
    unsafe { *out = std::ptr::null_mut() };
    file.decoder.check_supported().map_err(decode_err)?;
    // The decoder is not Sync (it holds Rc'd IFDs). `install` runs the closure on a
    // pool thread while this thread blocks, so there is no concurrent access.
    let job = AssertSend(file);
    let raw = in_pool(threads, move || {
      let file = job;
      file.0.decoder.raw_image(&file.0.source, &RawDecodeParams::default(), header_only != 0)
    })
    .map_err(decode_err)?;
    let opcodes = [OpcodeStage::List1, OpcodeStage::List2, OpcodeStage::List3].map(|s| raw.opcode_list(s).and_then(|l| l.to_bytes().ok()));
        let is_dng = file.decoder.format_hint() == FormatHint::DNG;
    unsafe { *out = Box::into_raw(Box::new(RlcImage { raw, opcodes, is_dng })) };
    Ok(())
  })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_image_free(image: *mut RlcImage) {
  if !image.is_null() {
    drop(unsafe { Box::from_raw(image) });
  }
}

/// Describe the decoded image (see `rlc_image_desc`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_image_describe(image: *const RlcImage, out: *mut RlcImageDesc) -> i32 {
  let r = guard(std::ptr::null_mut(), 0, || {
    let img = unsafe { image.as_ref() }.ok_or((RLC_ERR_INVALID_ARGUMENT, String::new()))?;
    let out = unsafe { out.as_mut() }.ok_or((RLC_ERR_INVALID_ARGUMENT, String::new()))?;
    let raw = &img.raw;
    let (data_type, data) = match &raw.data {
      RawImageData::Integer(v) if !v.is_empty() => (RLC_DATA_U16, v.as_ptr() as *const std::ffi::c_void),
      RawImageData::Float(v) if !v.is_empty() => (RLC_DATA_F32, v.as_ptr() as *const std::ffi::c_void),
      _ => (RLC_DATA_NONE, std::ptr::null()),
    };
    let mut d = RlcImageDesc {
      make: [0; 64],
      model: [0; 64],
      width: raw.width as i32,
      height: raw.height as i32,
      cpp: raw.cpp as i32,
      bps: raw.bps as i32,
      data_type,
      data,
      photometric: RLC_PHOTOMETRIC_BLACK_IS_ZERO,
      cfa_width: 0,
      cfa_height: 0,
      cfa: [255; 36],
      cfa_is_rgb: 0,
      black: [0.0; 4],
      white: [0.0; 4],
      wb_coeffs: raw.wb_coeffs,
      active_area: rect4(&raw.active_area),
      crop_area: rect4(&raw.crop_area),
      orientation: orientation_value(raw.orientation),
      fuji_rotation_width: raw.fuji_rotation_width.unwrap_or(0) as i32,
      baseline_exposure: raw.baseline_exposure.unwrap_or(f32::NAN),
      is_dng: img.is_dng as i32,
      color_matrix_count: 0,
      illuminant: [0; 3],
      color_matrix_rows: [0; 3],
      color_matrix: [[0.0; 12]; 3],
      best_color_matrix: -1,
      analog_balance: [f32::NAN; 4],
      linearization_len: raw.linearization_table.as_ref().map(|t| t.len() as i32).unwrap_or(0),
      opcode_list_len: img.opcodes.each_ref().map(|o| o.as_ref().map(|b| b.len() as i32).unwrap_or(0)),
      has_camera_calibration: [0; 3],
      camera_calibration: [[0.0; 9]; 3],
    };
    copy_str(&mut d.make, raw.make.trim());
    copy_str(&mut d.model, raw.model.trim());
    match &raw.photometric {
      RawPhotometricInterpretation::Cfa(cfg) => {
        d.photometric = RLC_PHOTOMETRIC_CFA;
        d.cfa_width = cfg.cfa.width as i32;
        d.cfa_height = cfg.cfa.height as i32;
        if cfg.cfa.width * cfg.cfa.height <= 36 {
          let pattern = cfg.cfa.flat_pattern();
          d.cfa[..pattern.len()].copy_from_slice(&pattern);
        }
        d.cfa_is_rgb = cfg.cfa.is_rgb() as i32;
      }
      RawPhotometricInterpretation::LinearRaw => d.photometric = RLC_PHOTOMETRIC_LINEAR_RAW,
      RawPhotometricInterpretation::BlackIsZero => d.photometric = RLC_PHOTOMETRIC_BLACK_IS_ZERO,
    }
    if matches!(raw.photometric, RawPhotometricInterpretation::LinearRaw) {
      let b = raw.blacklevel.as_vec();
      let w = raw.whitelevel.as_vec();
      for i in 0..4 {
        d.black[i] = b.get(i).or(b.last()).copied().unwrap_or(0.0);
        d.white[i] = w.get(i).or(w.last()).copied().unwrap_or(0.0);
      }
    } else {
      d.black = raw.blacklevel.as_bayer_array();
      d.white = raw.whitelevel.as_bayer_array();
    }
    if let Some(ab) = &raw.analog_balance {
      for (dst, v) in d.analog_balance.iter_mut().zip(ab.iter()) {
        *dst = *v;
      }
    }
    if let Some(profile) = raw.as_shot_camera_profile() {
      for (slot, m) in profile.color_matrix.iter().enumerate() {
        if let Some(m) = m
          && (m.len() == 9 || m.len() == 12)
        {
          let i = d.color_matrix_count as usize;
          d.illuminant[i] = profile.calibration_illuminant[slot].map(|il| u16::from(il) as i32).unwrap_or(0);
          d.color_matrix_rows[i] = (m.len() / 3) as i32;
          d.color_matrix[i][..m.len()].copy_from_slice(m);
          if let Some(cc) = profile.calibration_slot(slot, Some(&raw.camera_calibration)).and_then(|s| s.camera_calibration)
            && cc.len() == 9
            && cc.iter().all(|v| v.is_finite())
          {
            d.has_camera_calibration[i] = 1;
            d.camera_calibration[i].copy_from_slice(cc);
          }
          d.color_matrix_count += 1;
        }
      }
      if let Some((best, bm)) = raw.best_color_matrix() {
        for i in 0..d.color_matrix_count as usize {
          let rows = d.color_matrix_rows[i] as usize;
          if d.illuminant[i] == u16::from(best) as i32 && d.color_matrix[i][..rows * 3] == bm[..] {
            d.best_color_matrix = i as i32;
            break;
          }
        }
      }
    }
    *out = d;
    Ok(())
  });
  r
}

/// Raw bytes of DNG OpcodeList `stage` (1..3) as stored in the DNG (big endian).
/// `*data` is borrowed from the image. Returns RLC_OK with `*len = 0` when absent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_image_opcodes(image: *const RlcImage, stage: i32, data: *mut *const u8, len: *mut usize) -> i32 {
  let Some(img) = (unsafe { image.as_ref() }) else {
    return RLC_ERR_INVALID_ARGUMENT;
  };
  if data.is_null() || len.is_null() || !(1..=3).contains(&stage) {
    return RLC_ERR_INVALID_ARGUMENT;
  }
  match &img.opcodes[stage as usize - 1] {
    Some(b) => unsafe {
      *data = b.as_ptr();
      *len = b.len();
    },
    None => unsafe {
      *data = std::ptr::null();
      *len = 0;
    },
  }
  RLC_OK
}

/// Apply DNG OpcodeList1 (bad pixels, TrimBounds, ...) to the integer sensor
/// data with rawler's implementation. May change the size / areas: describe again.
/// Unsupported required opcodes fail with DECODE (data may be partly processed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_image_apply_opcode_list1(image: *mut RlcImage, threads: i32, err: *mut c_char, err_len: i32) -> i32 {
  guard(err, err_len, || {
    let img = unsafe { image.as_mut() }.ok_or((RLC_ERR_INVALID_ARGUMENT, "null image".to_string()))?;
    img.opcodes[0] = None;
    let raw = &mut img.raw;
    in_pool(threads, || raw.apply_opcode_list(OpcodeStage::List1)).map_err(decode_err)
  })
}

/// Apply a pending DNG LinearizationTable to the integer data in place (rawler's
/// dithered lookup). No-op when there is none.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_image_apply_linearization(image: *mut RlcImage, err: *mut c_char, err_len: i32) -> i32 {
  guard(err, err_len, || {
    let img = unsafe { image.as_mut() }.ok_or((RLC_ERR_INVALID_ARGUMENT, "null image".to_string()))?;
    let raw = &mut img.raw;
    let row = raw.width * raw.cpp;
    if let Some(table) = raw.linearization_table.take()
      && let RawImageData::Integer(samples) = &mut raw.data
    {
      rawler::imgop::raw::apply_linearization_table(samples, row, &table);
    }
    Ok(())
  })
}

// ---------------------------------------------------------------------------
// demosaic

/// Demosaic a single-channel mosaic (`width * height` floats, row-major) into
/// `out_rgb` (`width * height * 3` floats). Values are not clipped. The CFA must
/// be aligned to the buffer origin. Rows/columns at the buffer border are
/// interpolated with a simpler method; callers that demosaic strips should pad.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rlc_demosaic(
  method: i32,
  mosaic: *const f32,
  width: i32,
  height: i32,
  cfa: *const RlcCfa,
  threads: i32,
  out_rgb: *mut f32,
  err: *mut c_char,
  err_len: i32,
) -> i32 {
  guard(err, err_len, || {
    let cfa = unsafe { cfa.as_ref() }.ok_or((RLC_ERR_INVALID_ARGUMENT, "null cfa".to_string()))?;
    if mosaic.is_null() || out_rgb.is_null() || width <= 0 || height <= 0 {
      return Err((RLC_ERR_INVALID_ARGUMENT, "bad buffer".into()));
    }
    let (cw, ch) = (cfa.width as usize, cfa.height as usize);
    let xtrans = match (cw, ch) {
      (2, 2) => false,
      (6, 6) => true,
      _ => return Err((RLC_ERR_UNSUPPORTED, format!("CFA {cw}x{ch} is not Bayer or X-Trans"))),
    };
    let mut name = String::with_capacity(cw * ch);
    for &c in &cfa.pattern[..cw * ch] {
      name.push(match c {
        0 => 'R',
        1 => 'G',
        2 => 'B',
        _ => return Err((RLC_ERR_UNSUPPORTED, "CFA is not RGB".into())),
      });
    }
    let pat = CFA::new(&name);
    if !pat.is_rgb() {
      return Err((RLC_ERR_UNSUPPORTED, "CFA is not RGB".into()));
    }
    let (w, h) = (width as usize, height as usize);
    let src = unsafe { std::slice::from_raw_parts(mosaic, w * h) };
    let dst = unsafe { std::slice::from_raw_parts_mut(out_rgb, w * h * 3) };
    let view = ImageView::new(src, w, h, 1);
    let roi = Rect::new(Point::new(0, 0), Dim2::new(w, h));
    let colors = PlaneColor::new("RGB");
    let algo: Box<dyn Demosaic<f32, 3> + Sync> = match (method, xtrans) {
      (RLC_DEMOSAIC_PPG, false) => Box::new(PPGDemosaic::new()),
      (RLC_DEMOSAIC_MARKESTEIJN_1, true) => Box::new(XTransMarkesteijnDemosaic::new_pass_1()),
      (RLC_DEMOSAIC_MARKESTEIJN_3, true) => Box::new(XTransMarkesteijnDemosaic::new_pass_3()),
      (RLC_DEMOSAIC_XTRANS_BILINEAR, true) => Box::new(XTransBilinearDemosaic::new()),
      _ => return Err((RLC_ERR_UNSUPPORTED, format!("demosaic method {method} does not apply to this CFA"))),
    };
    let rgb = in_pool(threads, || algo.demosaic(view, &pat, &colors, roi));
    if rgb.data.len() != w * h {
      return Err((RLC_ERR_DECODE, "demosaic returned an unexpected size".into()));
    }
    dst.copy_from_slice(rgb.data.as_flattened());
    Ok(())
  })
}

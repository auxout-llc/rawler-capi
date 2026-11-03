use crate::decoders::exif_orientation_from_degrees;
use crate::RawImage;
use crate::RawLoader;
use crate::RawlerError;
use crate::Result;
use crate::alloc_image_ok;
use crate::analyze::FormatDump;
use crate::decompressors::ljpeg::LjpegDecompressor;
use crate::decompressors::packed::decompress_16be;
use crate::decompressors::packed::decompress_16le;
use crate::exif::Exif;
use crate::formats::tiff::GenericTiffReader;
use crate::formats::tiff::reader::TiffReader;
use crate::pixarray::PixU16;
use crate::rawsource::RawSource;
use crate::tags::TiffCommonTag;

use super::Camera;
use super::CameraLookup;
use super::Decoder;
use super::FormatHint;
use super::RawDecodeParams;
use super::RawMetadata;
use super::ok_cfa_image;

#[derive(Debug, Clone)]
pub struct MosDecoder<'a> {
  #[allow(unused)]
  rawloader: &'a RawLoader,
  tiff: GenericTiffReader,
  camera: CameraLookup,
}

impl<'a> MosDecoder<'a> {
  pub fn new(_file: &RawSource, tiff: GenericTiffReader, rawloader: &'a RawLoader) -> Result<MosDecoder<'a>> {
    let make = Self::xmp_tag(&tiff, "Make")?;
    let model_full = Self::xmp_tag(&tiff, "Model")?;
    let model = model_full.split_terminator('(').next().expect("split always yields at least one element");
    let camera = rawloader.lookup_camera_with_everything(&make, model, "");

    Ok(MosDecoder { tiff, rawloader, camera })
  }
}

impl<'a> Decoder for MosDecoder<'a> {
  fn raw_image(&self, file: &RawSource, _params: &RawDecodeParams, dummy: bool) -> Result<RawImage> {
    let camera = self.camera.supported()?;
    let raw = self
      .tiff
      .find_first_ifd_with_tag(TiffCommonTag::TileOffsets)
      .ok_or_else(|| RawlerError::DecoderFailed(format!("Failed to find a IFD with TileOffsets tag")))?;
    let width = fetch_tiff_tag!(raw, TiffCommonTag::ImageWidth).force_usize(0);
    let height = fetch_tiff_tag!(raw, TiffCommonTag::ImageLength).force_usize(0);
    let offset = fetch_tiff_tag!(raw, TiffCommonTag::TileOffsets).force_usize(0);
    let compression = fetch_tiff_tag!(raw, TiffCommonTag::Compression).force_usize(0);
    let src = file.subview_until_eof(offset as u64)?;

    let image = match compression {
      1 => {
        if self.tiff.little_endian() {
          decompress_16le(src, width, height, dummy)?
        } else {
          decompress_16be(src, width, height, dummy)?
        }
      }
      7 | 99 => self.decode_compressed(camera, src, width, height, dummy)?,
      x => return Err(RawlerError::unsupported(camera, format!("MOS: unsupported compression {}", x))),
    };

    let cpp = 1;
    ok_cfa_image(camera.clone(), cpp, self.get_wb()?, image, dummy)
  }

  fn check_supported(&self) -> Result<()> {
    self.camera.supported().map(|_| ())
  }

  fn format_dump(&self) -> FormatDump {
    todo!()
  }

  fn raw_metadata(&self, _file: &RawSource, _params: &RawDecodeParams) -> Result<RawMetadata> {
    let mut exif = Exif::new(self.tiff.root_ifd())?;
    // AUXOUT: Leaf records the turn in its own metadata (ImgProf_rotation_angle), not in TIFF.
    if let Some(orientation) = self.leaf_rotation().and_then(exif_orientation_from_degrees) {
      exif.orientation = Some(orientation);
    }
    let mdata = RawMetadata::new(self.camera.metadata(), exif);
    Ok(mdata)
  }

  fn format_hint(&self) -> FormatHint {
    FormatHint::MOS
  }
}

impl<'a> MosDecoder<'a> {
  fn get_wb(&self) -> Result<[f32; 4]> {
    let meta = fetch_tiff_tag!(self.tiff, TiffCommonTag::LeafMetadata).get_data();
    let mut pos = 0;
    // We need at least 16+45+10 bytes for the NeutObj_neutrals section itself
    while pos + 70 < meta.len() {
      if meta[pos..pos + 16] == b"NeutObj_neutrals"[..] {
        let data = &meta[pos + 44..];
        if let Some(endpos) = data.iter().position(|&x| x == 0) {
          let nums = String::from_utf8_lossy(&data[0..endpos])
            .split_terminator('\n')
            .map(|x| x.parse::<f32>().unwrap_or(f32::NAN))
            .collect::<Vec<f32>>();
          if nums.len() == 4 {
            return Ok([nums[0] / nums[1], nums[0] / nums[2], nums[0] / nums[3], f32::NAN]);
          }
        }
        break;
      }
      pos += 1;
    }
    Ok([f32::NAN, f32::NAN, f32::NAN, f32::NAN])
  }

  /// AUXOUT: ImgProf_rotation_angle from the Leaf metadata (PKTS entries: 40-byte name,
  /// big-endian u32 size, ASCII value).
  fn leaf_rotation(&self) -> Option<i32> {
    let meta = self.tiff.get_entry(TiffCommonTag::LeafMetadata)?.get_data();
    leaf_entry_value(meta, b"ImgProf_rotation_angle")?.trim_end_matches('\0').trim().parse().ok()
  }

  fn xmp_tag(tiff: &GenericTiffReader, tag: &str) -> Result<String> {
    let xmp_bytes = fetch_tiff_tag!(tiff, TiffCommonTag::Xmp).get_data();
    let xmp = String::from_utf8_lossy(xmp_bytes);
    let error = format!("MOS: Couldn't find XMP tag {}", tag);
    let start = xmp.find(&format!("<tiff:{}>", tag)).ok_or_else(|| error.clone())?;
    let end = xmp.find(&format!("</tiff:{}>", tag)).ok_or(error)?;

    Ok(xmp[start + tag.len() + 7..end].to_string())
  }

  pub fn decode_compressed(&self, cam: &Camera, src: &[u8], width: usize, height: usize, dummy: bool) -> Result<PixU16> {
    let interlaced = cam.find_hint("interlaced");
    Self::do_decode(src, interlaced, width, height, dummy)
  }

  pub(crate) fn do_decode(src: &[u8], interlaced: bool, width: usize, height: usize, dummy: bool) -> Result<PixU16> {
    if dummy {
      return Ok(PixU16::new_uninit(width, height));
    }

    let decompressor = LjpegDecompressor::new_full(src, true, true)?;
    let ljpegout = decompressor.decode_leaf(width, height)?;
    if interlaced {
      let mut out = alloc_image_ok!(width, height, dummy);
      for (row, line) in ljpegout.pixels().chunks_exact(width).enumerate() {
        let orow = if row & 1 == 1 { height - 1 - row / 2 } else { row / 2 };
        out[orow * width..(orow + 1) * width].copy_from_slice(line);
      }
      Ok(out)
    } else {
      Ok(ljpegout)
    }
  }
}

/// AUXOUT: value of one PKTS entry in the Leaf metadata, found by name.
fn leaf_entry_value(meta: &[u8], name: &[u8]) -> Option<String> {
  let pos = meta.windows(name.len()).position(|w| w == name)?;
  let size = u32::from_be_bytes(meta.get(pos + 40..pos + 44)?.try_into().ok()?) as usize;
  let value = meta.get(pos + 44..pos.checked_add(44)?.checked_add(size)?)?;
  Some(String::from_utf8_lossy(value).into_owned())
}

#[cfg(test)]
mod auxout_tests {
  use super::*;

  fn pkts(name: &str, value: &[u8]) -> Vec<u8> {
    let mut out = b"PKTS\0\0\0\x01".to_vec();
    let mut n = name.as_bytes().to_vec();
    n.resize(40, 0);
    out.extend_from_slice(&n);
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
    out
  }

  #[test]
  fn reads_rotation_angle_entry() {
    let mut meta = pkts("ImgProf_other", b"1\0");
    meta.extend(pkts("ImgProf_rotation_angle", b"90\0"));
    assert_eq!(leaf_entry_value(&meta, b"ImgProf_rotation_angle").as_deref(), Some("90\0"));
  }

  #[test]
  fn truncated_entry_is_none() {
    let mut meta = pkts("ImgProf_rotation_angle", b"90\0");
    meta.truncate(meta.len() - 2);
    assert_eq!(leaf_entry_value(&meta, b"ImgProf_rotation_angle"), None);
  }
}

// SPDX-License-Identifier: MIT
// Copyright 2021 Daniel Vogelbacher <daniel@chaospixel.com>

use std::io::{Read, Seek};

use log::debug;
use serde::{Deserialize, Serialize};

use crate::{
  bits::Endian,
  formats::tiff::{Rational, SRational, TiffAscii, TiffError, Value, apply_corr, reader::ReadByteOrder},
  tags::DngTag,
};

use super::{ReadBudget, Result, reader::EndianReader};

const TYPE_BYTE: u16 = 1;
const TYPE_ASCII: u16 = 2;
const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_RATIONAL: u16 = 5;
const TYPE_SBYTE: u16 = 6;
const TYPE_UNDEFINED: u16 = 7;
const TYPE_SSHORT: u16 = 8;
const TYPE_SLONG: u16 = 9;
const TYPE_SRATIONAL: u16 = 10;
const TYPE_FLOAT: u16 = 11;
const TYPE_DOUBLE: u16 = 12;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
  pub tag: u16,
  pub value: Value,
  // Embedded value for writer, offset for reader
  // This is only None when building an IFD for writing.
  pub embedded: Option<u32>, // TODO: rename it
}

impl std::ops::Deref for Entry {
  type Target = Value;

  fn deref(&self) -> &Self::Target {
    &self.value
  }
}

pub struct RawEntry<'a> {
  pub entry: &'a Entry,
  pub endian: Endian,
  pub data: Vec<u8>,
}

impl<'a> RawEntry<'a> {
  pub fn get_force_u32(&self, idx: usize) -> u32 {
    self.endian.read_u32(&self.data, idx * 4)
  }

  pub fn get_force_u16(&self, idx: usize) -> u16 {
    self.endian.read_u16(&self.data, idx * 2)
  }
}

// 0-1-2-3-4-5-6-7-8-9-10-11-12-13
const DATASHIFTS: [u8; 14] = [0, 0, 0, 1, 2, 3, 0, 0, 1, 2, 3, 2, 3, 2];

/// ExifIFD MakerNotes (auxout, see [`held_elements`]).
const MAKER_NOTE_TAG: u16 = 0x927c;
/// The raw file embedded in a DNG (auxout, see [`held_elements`]).
const ORIGINAL_RAW_FILE_DATA_TAG: u16 = DngTag::OriginalRawFileData as u16;
/// The second embedded JPEG of an RW2 (auxout, see [`held_elements`]). Up to 8.6 MB seen.
const PANASONIC_JPG_FROM_RAW2_TAG: u16 = 0x0127;
/// Above any maker note read as tags; the smallest Phase One file seen spans 6 MB.
const LARGE_MAKER_NOTE_BYTES: usize = 4 * 1024 * 1024;
const HELD_HEAD_BYTES: usize = 4096;
/// auxout: what an entry costs besides its values (the `Entry` and its place in the IFD's map).
/// Charged so that repeating tags without values also runs out of [`ReadBudget`].
pub(crate) const ENTRY_FIXED_BYTES: u64 = 64;

/// auxout: elements to read of a byte-sized value. Phase One's maker note spans the image data
/// (nearly the whole file), and its decoder reads it from the file. No decoder reads an embedded
/// raw file, as large as the original, or the JPEG of an RW2. Keep the start, for the name that
/// opens a maker note.
fn held_elements(tag: u16, count: usize) -> usize {
  match tag {
    MAKER_NOTE_TAG if count > LARGE_MAKER_NOTE_BYTES => HELD_HEAD_BYTES,
    ORIGINAL_RAW_FILE_DATA_TAG | PANASONIC_JPG_FROM_RAW2_TAG => count.min(HELD_HEAD_BYTES),
    _ => count,
  }
}

impl Entry {
  pub fn value_type(&self) -> u16 {
    self.value.value_type()
  }

  pub fn count(&self) -> u32 {
    self.value.count() as u32
  }

  /// Returns the offset
  /// It is already corrected by `corr` but needs to be summed
  /// with `base` offset.
  pub fn offset(&self) -> Option<usize> {
    self.embedded.map(|v| v as usize)
  }

  pub fn parse<R: Read + Seek>(reader: &mut EndianReader<R>, base: u32, corr: i32, tag: u16) -> Result<Entry> {
    Self::parse_budgeted(reader, base, corr, tag, &mut ReadBudget::new())
  }

  /// auxout: [`Entry::parse`], charging what the entry will hold to `budget` before allocating.
  pub(crate) fn parse_budgeted<R: Read + Seek>(reader: &mut EndianReader<R>, base: u32, corr: i32, tag: u16, budget: &mut ReadBudget) -> Result<Entry> {
    let pos = reader.position()? - 2; // TODO -2 because tag is already read

    let typ = reader.read_u16()?;
    let count = reader.read_u32()?;

    // If we don't know the type assume byte data (undefined)
    let compat_typ = if typ == 0 || typ > 12 { 7 } else { typ };

    let bytesize: usize = (count as usize) << DATASHIFTS[compat_typ as usize];
    let offset: u32 = if bytesize <= 4 {
      reader.position()? - base
    } else {
      apply_corr(reader.read_u32()?, corr)
    };

    debug!(
      "Tag: {:#x}, Typ: {:#x}, count: {}, offset: {}, base: {}, corr: {}",
      tag, typ, count, offset, base, corr
    );

    if offset == u32::MAX || base.checked_add(offset).is_none() {
      // We hit an invalid offset, ignoring this tag
      // This happens for Olympus E-P2 images in ImageProc IFD for example.
      return Err(TiffError::Overflow(format!("Offset {} is invalid for tag 0x{:X}", offset, tag)));
    }

    reader.goto(base + offset)?;
    // auxout: a count that does not fit in the file is a broken tag. Refuse it before
    // allocating: the values are read into a Vec of `count` elements, and a bogus count
    // (up to 4G elements) aborts processes whose allocator crashes on failure (Electron).
    let remaining = reader.remaining()?;
    if bytesize as u64 > remaining {
      return Err(TiffError::Overflow(format!(
        "Tag 0x{:X} claims {} bytes but only {} remain in the file",
        tag, bytesize, remaining
      )));
    }
    let held_bytes = match typ {
      TYPE_BYTE | TYPE_UNDEFINED => held_elements(tag, count as usize),
      _ => bytesize,
    };
    budget.charge(ENTRY_FIXED_BYTES + held_bytes as u64)?;
    let entry = match typ {
      TYPE_BYTE => {
        let mut v = vec![0; held_elements(tag, count as usize)];
        reader.read_u8_into(&mut v)?;
        Entry {
          tag,
          value: Value::Byte(v),
          embedded: Some(offset),
        }
      }
      TYPE_ASCII => {
        let mut v = vec![0; count as usize];
        reader.read_u8_into(&mut v)?;
        Entry {
          tag,
          value: Value::Ascii(TiffAscii::new_from_raw(&v)),
          embedded: Some(offset),
        }
      }
      TYPE_SHORT => {
        let mut v = vec![0; count as usize];
        reader.read_u16_into(&mut v)?;
        Entry {
          tag,
          value: Value::Short(v),
          embedded: Some(offset),
        }
      }
      TYPE_LONG => {
        let mut v = vec![0; count as usize];
        reader.read_u32_into(&mut v)?;
        Entry {
          tag,
          value: Value::Long(v),
          embedded: Some(offset),
        }
      }
      TYPE_RATIONAL => {
        let mut tmp = vec![0; count as usize * 2]; // Rational is 2x u32
        reader.read_u32_into(&mut tmp)?;

        let mut v = Vec::with_capacity(count as usize);
        for chunk in tmp.chunks_exact(2) {
          v.push(Rational::new(chunk[0], chunk[1]));
        }
        Entry {
          tag,
          value: Value::Rational(v),
          embedded: Some(offset),
        }
      }
      TYPE_SBYTE => {
        let mut v = vec![0; count as usize];
        reader.read_i8_into(&mut v)?;
        Entry {
          tag,
          value: Value::SByte(v),
          embedded: Some(offset),
        }
      }
      TYPE_UNDEFINED => {
        let mut v = vec![0; held_elements(tag, count as usize)];
        reader.read_u8_into(&mut v)?;
        Entry {
          tag,
          value: Value::Undefined(v),
          embedded: Some(offset),
        }
      }
      TYPE_SSHORT => {
        let mut v = vec![0; count as usize];
        reader.read_i16_into(&mut v)?;
        Entry {
          tag,
          value: Value::SShort(v),
          embedded: Some(offset),
        }
      }
      TYPE_SLONG => {
        let mut v = vec![0; count as usize];
        reader.read_i32_into(&mut v)?;
        Entry {
          tag,
          value: Value::SLong(v),
          embedded: Some(offset),
        }
      }
      TYPE_SRATIONAL => {
        let mut tmp = vec![0; count as usize * 2]; // SRational is 2x i32
        reader.read_i32_into(&mut tmp)?;

        let mut v = Vec::with_capacity(count as usize);
        for chunk in tmp.chunks_exact(2) {
          v.push(SRational::new(chunk[0], chunk[1]));
        }
        Entry {
          tag,
          value: Value::SRational(v),
          embedded: Some(offset),
        }
      }
      TYPE_FLOAT => {
        let mut v = vec![0.0; count as usize];
        reader.read_f32_into(&mut v)?;
        Entry {
          tag,
          value: Value::Float(v),
          embedded: Some(offset),
        }
      }
      TYPE_DOUBLE => {
        let mut v = vec![0.0; count as usize];
        reader.read_f64_into(&mut v)?;
        Entry {
          tag,
          value: Value::Double(v),
          embedded: Some(offset),
        }
      }
      x => {
        let mut v = vec![0; count as usize];
        reader.read_u8_into(&mut v)?;
        Entry {
          tag,
          value: Value::Unknown(x, v),
          embedded: Some(offset),
        }
      }
    };
    reader.goto(pos + 12)?; // Size of IFD entry
    Ok(entry)
  }

  pub fn type_name(&self) -> String {
    self.value.value_type_name()
  }
}

// auxout: a tag whose count does not fit in the rest of the file is refused before its
// values are allocated (see `Entry::parse`).
#[cfg(test)]
mod auxout_tests {
  use std::io::Cursor;

  use super::super::{TiffError, reader::EndianReader};
  use crate::bits::Endian;
  use super::*;

  /// One IFD entry after the tag number: type, count and a value offset of 16.
  fn entry_bytes(typ: u16, count: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&typ.to_le_bytes());
    b.extend_from_slice(&count.to_le_bytes());
    b.extend_from_slice(&16u32.to_le_bytes());
    b.resize(32, 0); // 16 bytes of values at offset 16
    b
  }

  fn parse(typ: u16, count: u32) -> Result<Entry> {
    let mut data = vec![0u8, 0u8]; // the tag number is read by the caller
    data.extend(entry_bytes(typ, count));
    let mut cursor = Cursor::new(data);
    cursor.set_position(2);
    let mut reader = EndianReader::new(&mut cursor, Endian::Little);
    Entry::parse(&mut reader, 0, 0, 0x9999)
  }

  #[test]
  fn refuses_a_count_larger_than_the_file() {
    // 0xFFFF_FFFF doubles would be 32 GB
    assert!(matches!(parse(TYPE_DOUBLE, u32::MAX), Err(TiffError::Overflow(_))));
    // 5 longs = 20 bytes, but only 16 bytes follow the offset
    assert!(matches!(parse(TYPE_LONG, 5), Err(TiffError::Overflow(_))));
  }

  #[test]
  fn reads_a_count_that_fits() {
    assert!(parse(TYPE_LONG, 4).is_ok());
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::bits::Endian;
  use std::io::Cursor;

  fn parse_undefined(tag: u16, count: usize) -> Entry {
    parse_typed(TYPE_UNDEFINED, tag, count)
  }

  fn parse_typed(typ: u16, tag: u16, count: usize) -> Entry {
    let mut file = vec![0u8; 12 + count];
    file[..2].copy_from_slice(&tag.to_le_bytes());
    file[2..4].copy_from_slice(&typ.to_le_bytes());
    file[4..8].copy_from_slice(&(count as u32).to_le_bytes());
    file[8..12].copy_from_slice(&12u32.to_le_bytes());
    file[12..21].copy_from_slice(b"IIIICwaR\0");
    let mut cursor = Cursor::new(file);
    let mut reader = EndianReader::new(&mut cursor, Endian::Little);
    reader.goto(2).unwrap();
    Entry::parse(&mut reader, 0, 0, tag).unwrap()
  }

  #[test]
  fn a_maker_note_spanning_the_image_keeps_only_its_start() {
    let entry = parse_undefined(MAKER_NOTE_TAG, LARGE_MAKER_NOTE_BYTES + 1);
    assert_eq!(entry.offset(), Some(12));
    let Value::Undefined(held) = &entry.value else { panic!("not undefined") };
    assert_eq!(held.len(), HELD_HEAD_BYTES);
    assert_eq!(&held[..8], b"IIIICwaR");
  }

  #[test]
  fn a_maker_note_of_the_limit_is_read_whole() {
    let entry = parse_undefined(MAKER_NOTE_TAG, LARGE_MAKER_NOTE_BYTES);
    let Value::Undefined(held) = &entry.value else { panic!("not undefined") };
    assert_eq!(held.len(), LARGE_MAKER_NOTE_BYTES);
  }

  #[test]
  fn an_embedded_raw_file_keeps_only_its_start() {
    let tag = ORIGINAL_RAW_FILE_DATA_TAG;
    let Value::Undefined(held) = &parse_undefined(tag, 1024 * 1024).value else { panic!("not undefined") };
    assert_eq!(held.len(), HELD_HEAD_BYTES);
    let entry = parse_typed(TYPE_BYTE, tag, 1024 * 1024);
    let Value::Byte(held) = &entry.value else { panic!("not bytes") };
    assert_eq!(held.len(), HELD_HEAD_BYTES);
    assert_eq!(entry.offset(), Some(12));
  }

  #[test]
  fn an_embedded_value_shorter_than_the_head_is_read_whole() {
    let entry = parse_typed(TYPE_BYTE, ORIGINAL_RAW_FILE_DATA_TAG, 100);
    let Value::Byte(held) = &entry.value else { panic!("not bytes") };
    assert_eq!(held.len(), 100);
  }

  #[test]
  fn the_second_jpeg_of_an_rw2_keeps_only_its_start() {
    let entry = parse_undefined(PANASONIC_JPG_FROM_RAW2_TAG, 1024 * 1024);
    let Value::Undefined(held) = &entry.value else { panic!("not undefined") };
    assert_eq!(held.len(), HELD_HEAD_BYTES);
  }

  #[test]
  fn another_tag_of_that_size_is_read_whole() {
    let entry = parse_undefined(0x9286, LARGE_MAKER_NOTE_BYTES + 1);
    let Value::Undefined(held) = &entry.value else { panic!("not undefined") };
    assert_eq!(held.len(), LARGE_MAKER_NOTE_BYTES + 1);
  }
}


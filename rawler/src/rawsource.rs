//! Image source like file or buffer

use std::{
  fmt::Debug,
  fs::File,
  io::Cursor,
  ops::Deref,
  path::{Path, PathBuf},
  sync::Arc,
};

use md5::Digest;
use memmap2::MmapOptions;

use crate::buffer::PaddedBuf;

/// Zero bytes after the data of a padded view, for the decoders that read ahead.
const PADDING: usize = 16;

pub struct RawSource {
  path: PathBuf,
  inner: RawSourceImpl,
}

enum RawSourceImpl {
  Memmap(memmap2::Mmap),
  Memory(Arc<Vec<u8>>),
}

impl RawSource {
  pub fn new(path: &Path) -> std::io::Result<Self> {
    let file = File::open(path)?;
    let mmap = unsafe { MmapOptions::new().map(&file)? };
    Ok(Self {
      path: path.canonicalize().unwrap_or_else(|_| path.to_owned()),
      inner: RawSourceImpl::Memmap(mmap),
    })
  }

  /// Tell the OS the whole file is about to be read. Separate from [`Self::new`]: the
  /// advice makes macOS fetch the whole file, which a read of the header must not do on a NAS.
  pub fn advise_full_read(&self) {
    #[cfg(unix)]
    if let RawSourceImpl::Memmap(mmap) = &self.inner {
      // Only an optimization: reading works without it.
      let _ = mmap.advise(memmap2::Advice::WillNeed);
      let _ = mmap.advise(memmap2::Advice::Sequential);
    }
  }

  pub fn new_from_shared_vec(buf: Arc<Vec<u8>>) -> Self {
    Self {
      path: PathBuf::default(),
      inner: RawSourceImpl::Memory(buf),
    }
  }

  pub fn with_path(self, path: impl AsRef<Path>) -> Self {
    Self {
      path: path.as_ref().to_owned(),
      inner: self.inner,
    }
  }

  pub fn new_from_slice(buf: &[u8]) -> Self {
    Self::new_from_shared_vec(Arc::new(Vec::from(buf)))
  }

  /// Calculate digest for file
  pub fn digest(&self) -> Digest {
    md5::compute(self.buf())
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  pub fn buf(&self) -> &[u8] {
    self.deref()
  }

  pub fn subview(&self, offset: u64, size: u64) -> std::io::Result<&[u8]> {
    self.buf().get(offset as usize..(offset + size) as usize).ok_or(std::io::Error::new(
      std::io::ErrorKind::UnexpectedEof,
      format!("subview(): Offset {}+{} is behind EOF", offset, size),
    ))
  }

  /// `dummy`: only the header is decoded. The decoders then look at the length and not at the
  /// bytes, so the end of the file gets zeros of that length instead of a copy, which on a NAS
  /// fetches the bytes. The zeros are not touched: they cost no memory either.
  pub fn subview_padded(&self, offset: u64, size: u64, dummy: bool) -> std::io::Result<PaddedBuf<'_>> {
    if offset + size <= self.len() as u64 {
      if offset + size + PADDING as u64 <= self.len() as u64 {
        self.subview(offset, size + PADDING as u64).map(|buf| PaddedBuf::new_ref(buf, size as usize))
      } else {
        self.padded_copy(self.subview(offset, size)?, dummy)
      }
    } else {
      Err(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!("subview_padded(): Offset {}+{} is behind EOF", offset, size),
      ))
    }
  }

  pub fn subview_until_eof(&self, offset: u64) -> std::io::Result<&[u8]> {
    self.buf().get(offset as usize..).ok_or(std::io::Error::new(
      std::io::ErrorKind::UnexpectedEof,
      format!("subview_until_eof(): Offset {} is behind EOF", offset),
    ))
  }

  /// `dummy`: see [`Self::subview_padded`].
  pub fn subview_until_eof_padded(&self, offset: u64, dummy: bool) -> std::io::Result<PaddedBuf<'_>> {
    if offset < self.len() as u64 {
      self.padded_copy(self.subview_until_eof(offset)?, dummy)
    } else {
      Err(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!("subview_until_eof_padded(): Offset {} is behind EOF", offset),
      ))
    }
  }

  fn padded_copy(&self, src: &[u8], dummy: bool) -> std::io::Result<PaddedBuf<'_>> {
    let mut buf = vec![0; src.len() + PADDING];
    if !dummy {
      buf[..src.len()].copy_from_slice(src);
    }
    Ok(PaddedBuf::new_owned(buf, src.len()))
  }

  pub fn reader(&self) -> Cursor<&[u8]> {
    Cursor::new(self.buf())
  }

  pub fn as_vec(&self) -> std::io::Result<Vec<u8>> {
    Ok(self.buf().to_vec())
  }

  pub fn stream_len(&mut self) -> u64 {
    self.buf().len() as u64
  }
}

impl Deref for RawSource {
  type Target = [u8];

  fn deref(&self) -> &Self::Target {
    match &self.inner {
      RawSourceImpl::Memmap(mmap) => mmap.deref(),
      RawSourceImpl::Memory(mem) => mem.deref(),
    }
  }
}

impl Debug for RawSource {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("RawSource").field("path", &self.path).finish()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_header_decode_sees_the_lengths_of_a_pixel_decode() {
    let source = RawSource::new_from_slice(&[7u8; 100]);
    for (header, pixels) in [
      (source.subview_until_eof_padded(40, true).unwrap(), source.subview_until_eof_padded(40, false).unwrap()),
      (source.subview_padded(90, 10, true).unwrap(), source.subview_padded(90, 10, false).unwrap()),
      (source.subview_padded(10, 10, true).unwrap(), source.subview_padded(10, 10, false).unwrap()),
    ] {
      assert_eq!(header.size(), pixels.size());
      assert_eq!(header.len(), pixels.len());
    }
  }

  #[test]
  fn a_header_decode_does_not_copy_the_end_of_the_file() {
    let source = RawSource::new_from_slice(&[7u8; 100]);
    assert!(source.subview_until_eof_padded(40, true).unwrap().iter().all(|&b| b == 0));
    assert!(source.subview_padded(90, 10, true).unwrap().iter().all(|&b| b == 0));
  }

  #[test]
  fn a_pixel_decode_still_gets_the_zero_padding_at_the_end_of_the_file() {
    let source = RawSource::new_from_slice(&[7u8; 100]);
    let view = source.subview_until_eof_padded(90, false).unwrap();
    assert_eq!(view.len(), 26);
    assert_eq!(&view[10..], &[0u8; 16]);
  }

  #[test]
  fn both_kinds_of_view_refuse_what_is_past_the_end() {
    let source = RawSource::new_from_slice(&[7u8; 100]);
    assert!(source.subview_padded(90, 11, true).is_err());
    assert!(source.subview_until_eof_padded(100, true).is_err());
  }
}

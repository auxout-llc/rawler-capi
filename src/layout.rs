// SPDX-License-Identifier: LGPL-2.1
// Sizes and field offsets (bytes, 64-bit targets) of the C structs in
// `include/rawler_capi.h`, as a C compiler lays them out. rawler-capi checks
// its own structs against them at compile time; users that mirror the structs
// by hand (raw_core's `rlc.rs` includes this file) check their mirror the
// same way, so neither side can drift from the header silently.
// Update together with the header (and RLC_ABI_VERSION).

pub(crate) const RLC_METADATA_SIZE: usize = 544;
pub(crate) const RLC_METADATA_ISO: usize = 504;
pub(crate) const RLC_METADATA_LENS_SPEC: usize = 512;

pub(crate) const RLC_IMAGE_DESC_SIZE: usize = 640;
pub(crate) const RLC_IMAGE_DESC_DATA: usize = 152;
pub(crate) const RLC_IMAGE_DESC_CFA: usize = 172;
pub(crate) const RLC_IMAGE_DESC_BLACK: usize = 212;
pub(crate) const RLC_IMAGE_DESC_COLOR_MATRIX: usize = 336;
pub(crate) const RLC_IMAGE_DESC_ANALOG_BALANCE: usize = 484;
pub(crate) const RLC_IMAGE_DESC_OPCODE_LIST_LEN: usize = 504;
pub(crate) const RLC_IMAGE_DESC_HAS_CAMERA_CALIBRATION: usize = 516;
pub(crate) const RLC_IMAGE_DESC_CAMERA_CALIBRATION: usize = 528;

pub(crate) const RLC_CFA_SIZE: usize = 44;
pub(crate) const RLC_CFA_PATTERN: usize = 8;

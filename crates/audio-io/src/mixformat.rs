//! Mix-format validation.
//!
//! Both the capture and render paths reinterpret the WASAPI endpoint buffer
//! as `f32` without conversion. That is only sound if the engine actually
//! negotiated 32-bit IEEE float. WASAPI shared-mode mix formats are float
//! in practice on every mainstream driver, but "in practice" is not a bounds
//! check: a 16-bit endpoint would make the capture path read 2x the buffer
//! length and the render path *write* 2x the buffer length, which is a heap
//! buffer overflow.
//!
//! This module centralises the check so both paths fail loudly at init
//! instead of corrupting memory at the first tick.

#![cfg(windows)]

use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE};

use crate::error::{AudioError, Result};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` = 00000003-0000-0010-8000-00aa00389b71.
/// Spelled out rather than imported so this doesn't break when windows-rs
/// shuffles which module re-exports the constant.
const SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Fields we care about, copied out of the packed struct.
#[derive(Debug, Clone, Copy)]
pub struct MixFormat {
    pub sample_rate: u32,
    pub channels: usize,
    pub bits_per_sample: u16,
    pub format_tag: u16,
    pub block_align: u32,
}

/// Read the interesting fields out of a `GetMixFormat` result and verify the
/// buffer really is 32-bit IEEE float.
///
/// # Safety
/// `ptr` must be a valid `WAVEFORMATEX` from `IAudioClient::GetMixFormat`.
/// When `format_tag` is `WAVE_FORMAT_EXTENSIBLE` the allocation is a full
/// `WAVEFORMATEXTENSIBLE` (cbSize >= 22), which is what lets us read
/// `SubFormat`.
pub unsafe fn read_and_validate(ptr: *const WAVEFORMATEX, which: &'static str) -> Result<MixFormat> {
    let fmt = MixFormat {
        sample_rate: (*ptr).nSamplesPerSec,
        channels: (*ptr).nChannels as usize,
        bits_per_sample: (*ptr).wBitsPerSample,
        format_tag: (*ptr).wFormatTag,
        block_align: (*ptr).nBlockAlign as u32,
    };

    if fmt.channels == 0 {
        return Err(AudioError::UnsupportedFormat(format!(
            "{which} endpoint reports 0 channels"
        )));
    }

    if fmt.bits_per_sample != 32 {
        return Err(AudioError::UnsupportedFormat(format!(
            "{which} endpoint mix format is {}-bit; NoiseGate only handles 32-bit float. \
             Set the device to a 32-bit float format in Sound Control Panel → \
             Properties → Advanced.",
            fmt.bits_per_sample
        )));
    }

    let is_float = match fmt.format_tag {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE => {
            // cbSize must cover the extensible tail before we may read it.
            // Copy out of the packed struct: taking a reference to a field
            // (including via format!) would be an unaligned reference.
            let cb_size = (*ptr).cbSize;
            if cb_size < 22 {
                return Err(AudioError::UnsupportedFormat(format!(
                    "{which} endpoint claims WAVE_FORMAT_EXTENSIBLE but cbSize={cb_size} (<22)"
                )));
            }
            let ext = ptr as *const WAVEFORMATEXTENSIBLE;
            let sub_format = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
            sub_format == SUBTYPE_IEEE_FLOAT
        }
        _ => false,
    };

    if !is_float {
        return Err(AudioError::UnsupportedFormat(format!(
            "{which} endpoint is not IEEE float (wFormatTag=0x{:04X}); \
             NoiseGate only handles 32-bit float.",
            fmt.format_tag
        )));
    }

    // Belt and braces: the f32 reinterpretation assumes the frame stride is
    // exactly channels * 4 bytes. If a driver reports something else, every
    // index we compute is wrong.
    let expected_align = (fmt.channels * 4) as u32;
    if fmt.block_align != expected_align {
        return Err(AudioError::UnsupportedFormat(format!(
            "{which} endpoint nBlockAlign={} but {} channels of f32 implies {}",
            fmt.block_align, fmt.channels, expected_align
        )));
    }

    Ok(fmt)
}

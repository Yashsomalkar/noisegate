use crate::error::{AudioError, Result};

/// Pipeline-internal audio format. We always operate on mono f32 @ 48 kHz.
/// Resampling/channel-mixing happens in the WASAPI wrappers when the device's
/// mix format differs.
#[derive(Debug, Clone, Copy)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl StreamFormat {
    pub const PIPELINE: Self = Self {
        sample_rate: 48_000,
        channels: 1,
    };
}

/// The fields of a device's WASAPI mix format that the audio loops care
/// about, snapshotted out of the (packed, possibly extensible) `WAVEFORMATEX`
/// the engine hands back from `GetMixFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// Bytes per frame (all channels). For 32-bit float this is `channels * 4`.
    pub block_align: u16,
    /// True when the sample type is IEEE float — either `WAVE_FORMAT_IEEE_FLOAT`
    /// or `WAVE_FORMAT_EXTENSIBLE` carrying the IEEE-float subformat GUID.
    pub is_float: bool,
}

impl MixFormat {
    /// Both audio loops reinterpret the engine's raw byte buffer as `[f32]`.
    /// That is only sound when the device really is mixing 32-bit float: on a
    /// 16-bit device we would read (capture) or write (render) twice the bytes
    /// the engine allocated, walking off the end of the shared buffer. The
    /// mix format is *usually* float32 in shared mode, but nothing in WASAPI
    /// guarantees it, so refuse the device instead of trusting it.
    pub fn validate(&self) -> Result<()> {
        if self.channels == 0 {
            return Err(AudioError::UnsupportedFormat(
                "device reports 0 channels".into(),
            ));
        }
        if !self.is_float || self.bits_per_sample != 32 {
            return Err(AudioError::UnsupportedFormat(format!(
                "device mixes {} {}-bit samples; NoiseGate needs 32-bit IEEE float",
                if self.is_float { "float" } else { "integer PCM" },
                self.bits_per_sample
            )));
        }
        let expected = u32::from(self.channels) * 4;
        if u32::from(self.block_align) != expected {
            return Err(AudioError::UnsupportedFormat(format!(
                "nBlockAlign is {} but {} channels of 32-bit float need {expected}",
                self.block_align, self.channels
            )));
        }
        Ok(())
    }
}

/// Read the mix format out of a `GetMixFormat` pointer.
///
/// # Safety
/// `ptr` must be a valid `WAVEFORMATEX` from the audio engine. When
/// `wFormatTag` is `WAVE_FORMAT_EXTENSIBLE` the engine guarantees the
/// extensible tail is present, but we check `cbSize` before reading it
/// anyway rather than take that on faith.
#[cfg(windows)]
pub unsafe fn read_mix_format(
    ptr: *const windows::Win32::Media::Audio::WAVEFORMATEX,
) -> MixFormat {
    use windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE;
    use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
    use windows::Win32::Media::Multimedia::{
        KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT,
    };

    let format_tag = (*ptr).wFormatTag as u32;
    let cb_size = (*ptr).cbSize;

    let is_float = if format_tag == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if format_tag == WAVE_FORMAT_EXTENSIBLE && cb_size >= 22 {
        // Packed struct: read the GUID out unaligned rather than borrowing it.
        let ext = ptr as *const WAVEFORMATEXTENSIBLE;
        std::ptr::addr_of!((*ext).SubFormat).read_unaligned() == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        false
    };

    MixFormat {
        sample_rate: (*ptr).nSamplesPerSec,
        channels: (*ptr).nChannels,
        bits_per_sample: (*ptr).wBitsPerSample,
        block_align: (*ptr).nBlockAlign,
        is_float,
    }
}

#[cfg(test)]
mod tests {
    use super::MixFormat;

    fn float32(channels: u16) -> MixFormat {
        MixFormat {
            sample_rate: 48_000,
            channels,
            bits_per_sample: 32,
            block_align: channels * 4,
            is_float: true,
        }
    }

    #[test]
    fn accepts_float32_mix_formats() {
        assert!(float32(1).validate().is_ok());
        assert!(float32(2).validate().is_ok());
        assert!(float32(8).validate().is_ok());
    }

    #[test]
    fn rejects_integer_pcm() {
        // The dangerous case: 16-bit PCM would make us read/write 2x the
        // bytes the engine allocated.
        let f = MixFormat {
            bits_per_sample: 16,
            block_align: 4,
            is_float: false,
            ..float32(2)
        };
        assert!(f.validate().is_err());
    }

    #[test]
    fn rejects_float64_and_mismatched_block_align() {
        let wide = MixFormat {
            bits_per_sample: 64,
            block_align: 16,
            ..float32(2)
        };
        assert!(wide.validate().is_err());

        let lying = MixFormat {
            block_align: 6,
            ..float32(2)
        };
        assert!(lying.validate().is_err());
    }

    #[test]
    fn rejects_zero_channels() {
        let f = MixFormat {
            channels: 0,
            block_align: 0,
            ..float32(1)
        };
        assert!(f.validate().is_err());
    }
}

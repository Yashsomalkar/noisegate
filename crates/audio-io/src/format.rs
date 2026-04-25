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

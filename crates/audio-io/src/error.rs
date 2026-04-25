use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("VB-Cable input device is not installed (looked for friendly name containing 'CABLE Input')")]
    VbCableMissing,

    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),

    #[error("WASAPI call failed: {context}: {source}")]
    Wasapi {
        context: &'static str,
        #[source]
        source: anyhow::Error,
    },

    #[error("audio thread panicked or exited unexpectedly")]
    ThreadDied,

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[cfg(windows)]
impl AudioError {
    pub(crate) fn wasapi(context: &'static str, e: windows::core::Error) -> Self {
        Self::Wasapi {
            context,
            source: anyhow::anyhow!("HRESULT 0x{:08X}: {}", e.code().0 as u32, e.message()),
        }
    }
}

pub type Result<T> = std::result::Result<T, AudioError>;

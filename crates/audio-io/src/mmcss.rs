//! MMCSS (Multimedia Class Scheduler Service) — promotes audio threads to
//! "Pro Audio" priority. Without this, ordinary thread scheduling produces
//! sporadic 2-10 ms stalls under CPU load and audible glitches.

#![cfg(windows)]

use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};

pub struct ProAudio(HANDLE);

impl ProAudio {
    pub fn set_for_current_thread() -> Option<Self> {
        unsafe {
            let mut task_index: u32 = 0;
            match AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) {
                Ok(h) if !h.is_invalid() => Some(Self(h)),
                _ => {
                    tracing::warn!(
                        "AvSetMmThreadCharacteristicsW failed; running at normal priority"
                    );
                    None
                }
            }
        }
    }
}

impl Drop for ProAudio {
    fn drop(&mut self) {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.0);
        }
    }
}

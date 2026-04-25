use crate::error::{AudioError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceDirection {
    Capture,
    Render,
}

#[derive(Debug, Clone)]
pub struct Device {
    /// Stable WASAPI endpoint ID (e.g. "{0.0.1.00000000}.{guid}"). Persisted
    /// in config so the user's chosen device survives reboots and unrelated
    /// device plug events.
    pub id: String,
    pub friendly_name: String,
    pub direction: DeviceDirection,
    pub is_default: bool,
}

impl Device {
    /// Heuristic: is this the VB-Cable virtual input device?
    /// VB-Audio names its endpoints "CABLE Input (VB-Audio Virtual Cable)"
    /// for what we render INTO, and "CABLE Output" for what other apps
    /// capture FROM. We render into "CABLE Input".
    pub fn is_vb_cable_input(&self) -> bool {
        self.direction == DeviceDirection::Render
            && self.friendly_name.contains("CABLE Input")
    }
}

#[derive(Debug, Default)]
pub struct DeviceList {
    pub capture: Vec<Device>,
    pub render: Vec<Device>,
}

impl DeviceList {
    pub fn enumerate() -> Result<Self> {
        #[cfg(windows)]
        {
            crate::wasapi_capture::enumerate_all()
        }
        #[cfg(not(windows))]
        {
            Err(AudioError::Other(anyhow::anyhow!(
                "device enumeration is only supported on Windows"
            )))
        }
    }

    pub fn find_vb_cable_input(&self) -> Result<&Device> {
        self.render
            .iter()
            .find(|d| d.is_vb_cable_input())
            .ok_or(AudioError::VbCableMissing)
    }

    pub fn default_capture(&self) -> Option<&Device> {
        self.capture.iter().find(|d| d.is_default)
    }
}

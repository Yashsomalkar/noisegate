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

    /// If this render endpoint is one half of a known virtual cable, return
    /// the cable. Anything we render into it shows up as a microphone that
    /// Discord/Zoom/Teams/OBS can select.
    pub fn virtual_cable(&self) -> Option<&'static VirtualCable> {
        if self.direction != DeviceDirection::Render {
            return None;
        }
        let name = self.friendly_name.to_lowercase();
        VIRTUAL_CABLES
            .iter()
            .find(|c| c.render_match.iter().any(|m| name.contains(m)))
    }
}

/// A virtual audio cable: a render endpoint whose output is looped back to a
/// capture endpoint by the driver.
#[derive(Debug)]
pub struct VirtualCable {
    pub label: &'static str,
    /// Lower-case substrings that identify the render side.
    render_match: &'static [&'static str],
    /// What the user should pick as their mic in other apps.
    pub mic_name: &'static str,
}

impl VirtualCable {
    /// True if `friendly_name` belongs to either side of this cable.
    pub fn matches_name(&self, friendly_name: &str) -> bool {
        let name = friendly_name.to_lowercase();
        self.render_match.iter().any(|m| name.contains(m))
            || name.contains(&self.mic_name.to_lowercase())
    }
}

/// In order of preference. VB-Cable first (purpose-built, and what the rest
/// of the app was written around); the others are drivers people often
/// already have installed.
pub static VIRTUAL_CABLES: &[VirtualCable] = &[
    VirtualCable {
        label: "VB-Cable",
        render_match: &["cable input"],
        mic_name: "CABLE Output (VB-Audio Virtual Cable)",
    },
    VirtualCable {
        label: "VoiceMeeter",
        render_match: &["voicemeeter input", "voicemeeter vaio"],
        mic_name: "VoiceMeeter Output (or Out B1)",
    },
    VirtualCable {
        label: "Steam Streaming Microphone",
        // The render side is named "Speakers (Steam Streaming Microphone)".
        // Not to be confused with "Steam Streaming Speakers", which is a
        // separate device with no mic attached.
        render_match: &["steam streaming microphone"],
        mic_name: "Microphone (Steam Streaming Microphone)",
    },
];

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

    /// Best available virtual cable render endpoint, per `VIRTUAL_CABLES`
    /// preference order.
    pub fn find_virtual_cable(&self) -> Option<(&Device, &'static VirtualCable)> {
        VIRTUAL_CABLES.iter().find_map(|cable| {
            self.render
                .iter()
                .find(|d| d.virtual_cable().map(|c| std::ptr::eq(c, cable)).unwrap_or(false))
                .map(|d| (d, cable))
        })
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

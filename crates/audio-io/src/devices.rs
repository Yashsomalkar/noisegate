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
    ///
    /// We require the vendor half of the name too: this heuristic decides
    /// where the microphone ends up, and "CABLE Input" on its own is a name
    /// any virtual audio device could claim.
    pub fn is_vb_cable_input(&self) -> bool {
        self.direction == DeviceDirection::Render
            && self.friendly_name.contains("CABLE Input")
            && self.friendly_name.contains("VB-Audio")
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
        let mut matches = self.render.iter().filter(|d| d.is_vb_cable_input());
        let first = matches.next().ok_or(AudioError::VbCableMissing)?;
        if matches.next().is_some() {
            // Don't guess which cable gets the microphone.
            return Err(AudioError::AmbiguousDevice(
                "multiple VB-Cable input endpoints found; set output_device_id in config.toml"
                    .into(),
            ));
        }
        Ok(first)
    }

    pub fn default_capture(&self) -> Option<&Device> {
        self.capture.iter().find(|d| d.is_default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(name: &str) -> Device {
        Device {
            id: format!("{{0.0.0.00000000}}.{name}"),
            friendly_name: name.into(),
            direction: DeviceDirection::Render,
            is_default: false,
        }
    }

    #[test]
    fn matches_the_real_vb_cable_endpoint() {
        assert!(render("CABLE Input (VB-Audio Virtual Cable)").is_vb_cable_input());
    }

    #[test]
    fn ignores_lookalikes_and_capture_endpoints() {
        // Name-squatting: anything can call itself "CABLE Input".
        assert!(!render("CABLE Input (Totally Not A Recorder)").is_vb_cable_input());
        // The side other apps capture from, not the side we render into.
        assert!(!render("CABLE Output (VB-Audio Virtual Cable)").is_vb_cable_input());

        let mut capture = render("CABLE Input (VB-Audio Virtual Cable)");
        capture.direction = DeviceDirection::Capture;
        assert!(!capture.is_vb_cable_input());
    }

    #[test]
    fn refuses_to_guess_between_two_cables() {
        let list = DeviceList {
            capture: vec![],
            render: vec![
                render("CABLE Input (VB-Audio Virtual Cable)"),
                render("CABLE Input (VB-Audio Virtual Cable) 2"),
            ],
        };
        assert!(list.find_vb_cable_input().is_err());
    }

    #[test]
    fn reports_missing_cable() {
        let list = DeviceList {
            capture: vec![],
            render: vec![render("Speakers (Realtek Audio)")],
        };
        assert!(matches!(
            list.find_vb_cable_input(),
            Err(AudioError::VbCableMissing)
        ));
    }
}

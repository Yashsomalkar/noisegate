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

/// Virtual audio cables we know how to drive.
///
/// A cable is two endpoints: a render one we push cleaned audio into, and a
/// capture one that other apps select as their microphone. Each entry lists
/// the substrings that must *all* appear in the friendly name — matching on
/// one word alone would let any device claim to be a cable, and this decides
/// where the microphone ends up.
const KNOWN_CABLES: &[Cable] = &[
    Cable {
        product: "VB-Cable",
        input: &["cable input", "vb-audio"],
        output: &["cable output", "vb-audio"],
    },
    Cable {
        product: "VoiceMeeter",
        input: &["voicemeeter", "input"],
        output: &["voicemeeter", "output"],
    },
    Cable {
        product: "Virtual Audio Cable",
        input: &["virtual audio cable", "line "],
        output: &["virtual audio cable", "line "],
    },
];

struct Cable {
    product: &'static str,
    /// Render side — where we write.
    input: &'static [&'static str],
    /// Capture side — where other apps read.
    output: &'static [&'static str],
}

/// Every product NoiseGate can route through, for error messages.
pub fn known_cable_products() -> Vec<&'static str> {
    KNOWN_CABLES.iter().map(|c| c.product).collect()
}

fn matches_all(name: &str, needles: &[&str]) -> bool {
    let lower = name.to_ascii_lowercase();
    needles.iter().all(|n| lower.contains(n))
}

impl Device {
    /// The cable whose **input** side this is — the endpoint we render cleaned
    /// audio into. VB-Audio names it "CABLE Input (VB-Audio Virtual Cable)".
    pub fn virtual_cable_input(&self) -> Option<&'static str> {
        if self.direction != DeviceDirection::Render {
            return None;
        }
        KNOWN_CABLES
            .iter()
            .find(|c| matches_all(&self.friendly_name, c.input))
            .map(|c| c.product)
    }

    /// The cable whose **output** side this is — the endpoint other apps pick
    /// as their microphone.
    ///
    /// Worth detecting because Windows tends to make a freshly installed cable
    /// the default capture device. Recording from it while rendering into the
    /// same cable feeds the thing into itself.
    pub fn virtual_cable_output(&self) -> Option<&'static str> {
        if self.direction != DeviceDirection::Capture {
            return None;
        }
        KNOWN_CABLES
            .iter()
            .find(|c| matches_all(&self.friendly_name, c.output))
            .map(|c| c.product)
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

    /// The one virtual cable input to render into, if exactly one is present.
    pub fn find_virtual_cable_input(&self) -> Result<&Device> {
        let mut matches = self
            .render
            .iter()
            .filter(|d| d.virtual_cable_input().is_some());
        let first = matches.next().ok_or(AudioError::VirtualCableMissing)?;
        if matches.next().is_some() {
            // Don't guess which cable gets the microphone.
            return Err(AudioError::AmbiguousDevice(
                "several virtual cables are installed; set output_device_id in config.toml to \
                 the one you want"
                    .into(),
            ));
        }
        Ok(first)
    }

    pub fn default_capture(&self) -> Option<&Device> {
        self.capture.iter().find(|d| d.is_default)
    }

    pub fn capture_by_id(&self, id: &str) -> Option<&Device> {
        self.capture.iter().find(|d| d.id == id)
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

    fn capture(name: &str) -> Device {
        Device {
            direction: DeviceDirection::Capture,
            ..render(name)
        }
    }

    #[test]
    fn matches_the_real_vb_cable_endpoint() {
        assert_eq!(
            render("CABLE Input (VB-Audio Virtual Cable)").virtual_cable_input(),
            Some("VB-Cable")
        );
    }

    #[test]
    fn ignores_lookalikes_and_the_wrong_direction() {
        // Name-squatting: anything can call itself "CABLE Input".
        assert_eq!(
            render("CABLE Input (Totally Not A Recorder)").virtual_cable_input(),
            None
        );
        // The side other apps capture from, not the side we render into.
        assert_eq!(
            render("CABLE Output (VB-Audio Virtual Cable)").virtual_cable_input(),
            None
        );
        // Right name, wrong direction.
        assert_eq!(
            capture("CABLE Input (VB-Audio Virtual Cable)").virtual_cable_input(),
            None
        );
    }

    /// The loop guard depends on this: installing a cable tends to make its
    /// output the default capture device, and recording from that while
    /// rendering into the same cable feeds it into itself.
    #[test]
    fn recognises_the_capture_side_of_a_cable() {
        assert_eq!(
            capture("CABLE Output (VB-Audio Virtual Cable)").virtual_cable_output(),
            Some("VB-Cable")
        );
        assert_eq!(
            capture("Microphone (fifine Microphone)").virtual_cable_output(),
            None
        );
        // Render side is not a capture side.
        assert_eq!(
            render("CABLE Output (VB-Audio Virtual Cable)").virtual_cable_output(),
            None
        );
    }

    /// VB-Cable also installs "CABLE In 16ch", which must not be confused for
    /// the endpoint we render into.
    #[test]
    fn ignores_the_multichannel_sibling_endpoint() {
        assert_eq!(
            render("CABLE In 16ch (VB-Audio Virtual Cable)").virtual_cable_input(),
            None
        );
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
        assert!(list.find_virtual_cable_input().is_err());
    }

    #[test]
    fn reports_missing_cable() {
        let list = DeviceList {
            capture: vec![],
            render: vec![render("Speakers (Realtek Audio)")],
        };
        assert!(matches!(
            list.find_virtual_cable_input(),
            Err(AudioError::VirtualCableMissing)
        ));
    }
}

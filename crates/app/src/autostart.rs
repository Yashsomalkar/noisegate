//! Start-at-login, via the per-user `Run` key.
//!
//! `HKCU\...\CurrentVersion\Run` rather than the machine-wide `HKLM` one or a
//! scheduled task: it needs no elevation, it's trivially inspectable by the
//! user, and uninstalling is deleting one value. NoiseGate holds a microphone
//! open — it should not be quietly installing itself for every account on the
//! machine.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS,
    REG_SZ,
};

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE_NAME: PCWSTR = w!("NoiseGate");

/// The command Windows will run at login. Quoted, because `C:\Program Files\…`
/// would otherwise be parsed as several arguments.
fn command_string(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().map_err(Into::into)
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn open_run_key(access: REG_SAM_FLAGS) -> Result<Key> {
    let mut key = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            access,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        bail!("opening the Run key failed: error {}", status.0);
    }
    Ok(Key(key))
}

/// Is NoiseGate currently registered to start at login?
pub fn is_enabled() -> bool {
    let Ok(key) = open_run_key(KEY_QUERY_VALUE) else {
        return false;
    };
    // We only care whether the value exists, not what it holds — a stale path
    // from a moved binary still means "the user asked for autostart", and
    // `set(true)` will rewrite it.
    unsafe { RegQueryValueExW(key.0, VALUE_NAME, None, None, None, None) == ERROR_SUCCESS }
}

/// Register or unregister start-at-login for the current user.
pub fn set(enabled: bool) -> Result<()> {
    let key = open_run_key(KEY_SET_VALUE)?;
    let status = if enabled {
        let value = command_string(&exe_path()?);
        // REG_SZ wants NUL-terminated UTF-16, handed over as raw bytes.
        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = unsafe {
            std::slice::from_raw_parts(wide.as_ptr() as *const u8, std::mem::size_of_val(&wide[..]))
        };
        unsafe { RegSetValueExW(key.0, VALUE_NAME, 0, REG_SZ, Some(bytes)) }
    } else {
        let status = unsafe { RegDeleteValueW(key.0, VALUE_NAME) };
        // Deleting something that was never there is a success as far as the
        // caller is concerned.
        if status != ERROR_SUCCESS && !is_enabled() {
            ERROR_SUCCESS
        } else {
            status
        }
    };
    if status != ERROR_SUCCESS {
        bail!("writing the Run key failed: error {}", status.0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_quoted_for_paths_with_spaces() {
        let cmd = command_string(Path::new(r"C:\Program Files\NoiseGate\noisegate.exe"));
        assert_eq!(cmd, "\"C:\\Program Files\\NoiseGate\\noisegate.exe\"");
        assert!(cmd.starts_with('"') && cmd.ends_with('"'));
    }

    /// Round-trip against the real registry. It's the user's own HKCU Run key,
    /// so this is safe, but restore whatever was there before.
    #[test]
    fn enabling_and_disabling_round_trips() {
        let original = is_enabled();

        set(true).expect("enable");
        assert!(is_enabled(), "should be registered after set(true)");

        set(false).expect("disable");
        assert!(!is_enabled(), "should be gone after set(false)");

        // Disabling twice must not error.
        set(false).expect("disable again");

        if original {
            set(true).expect("restore");
        }
    }
}

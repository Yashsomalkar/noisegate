//! "Start with Windows": a value under HKCU\...\CurrentVersion\Run. Per user,
//! no admin rights, and visible/removable in Task Manager → Startup apps.

#![cfg(windows)]

use anyhow::{anyhow, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("NoiseGate");

/// Whether NoiseGate is registered to start at login.
pub fn is_enabled() -> bool {
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, RRF_RT_REG_SZ, None, None, None)
            == ERROR_SUCCESS
    }
}

/// Register (the currently running exe) or unregister start at login.
pub fn set(enabled: bool) -> Result<()> {
    unsafe {
        if enabled {
            let exe = std::env::current_exe()?;
            let cmd = format!("\"{}\"", exe.display());
            let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let r = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE,
                REG_SZ.0,
                Some(wide.as_ptr().cast()),
                (wide.len() * 2) as u32,
            );
            if r != ERROR_SUCCESS {
                return Err(anyhow!("writing the Run registry value failed: {r:?}"));
            }
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE);
            if r != ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
                return Err(anyhow!("removing the Run registry value failed: {r:?}"));
            }
        }
    }
    Ok(())
}

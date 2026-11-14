//! Dark tray menu when Windows apps are set to dark mode.
//!
//! Win32 popup menus are light unless the process opts in through uxtheme's
//! `SetPreferredAppMode` (exported by ordinal 135 since Windows 10 1903, used
//! by Explorer, Notepad++ and others). We follow the user's "app mode"
//! setting and re-check it periodically, so switching themes applies live.

#![cfg(windows)]

use std::sync::atomic::{AtomicI8, Ordering};

use windows::core::{w, PCSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

/// uxtheme `PreferredAppMode` values.
const FORCE_DARK: i32 = 2;
const FORCE_LIGHT: i32 = 3;

/// -1 = not applied yet, else 0/1 = light/dark last applied.
static APPLIED: AtomicI8 = AtomicI8::new(-1);

/// Windows' "Choose your app mode": true when apps should be dark.
fn apps_use_dark() -> bool {
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    r == ERROR_SUCCESS && value == 0
}

/// Make menus match the current app mode. Cheap when nothing changed.
pub fn sync_menu_theme() {
    let dark = apps_use_dark();
    if APPLIED.swap(dark as i8, Ordering::Relaxed) == dark as i8 {
        return;
    }
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        // Exported by ordinal only: pass the ordinal as the name pointer.
        if let Some(f) = GetProcAddress(lib, PCSTR(135 as *const u8)) {
            let set_preferred_app_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_preferred_app_mode(if dark { FORCE_DARK } else { FORCE_LIGHT });
        }
        if let Some(f) = GetProcAddress(lib, PCSTR(136 as *const u8)) {
            let flush_menu_themes: extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }
    }
}

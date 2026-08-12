//! Console attachment for a GUI-subsystem binary.
//!
//! NoiseGate is built as `windows_subsystem = "windows"` so double-clicking it
//! goes straight to the tray without a console window hanging around behind
//! it. The cost is that the CLI modes (`--list-devices`, `--denoise`, …) would
//! print into the void: a GUI-subsystem process starts with no console and
//! invalid standard handles.
//!
//! [`attach_to_parent`] borrows the console of whatever launched us — the
//! PowerShell window you typed into — so command-line output still works.
//! Nothing is created when there's no parent console (a double-click), which
//! is exactly the "no console flash" behaviour we want.

use windows::core::w;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
    STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

/// Attach to the parent process's console, if it has one.
///
/// Returns true when stdio is usable afterwards. Must be called before any
/// output: Rust caches the standard handles the first time you print, so
/// redirecting them afterwards is too late.
pub fn attach_to_parent() -> bool {
    unsafe {
        // Already redirected — `noisegate --list-devices > out.txt`, or a
        // pipe. Those handles are valid and are exactly where the user wants
        // the output; pointing them at CONOUT$ would throw the redirection
        // away and print into a console they aren't looking at.
        if is_usable(STD_OUTPUT_HANDLE) {
            return true;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            // No parent console — launched from Explorer or the tray.
            return false;
        }
        // Attaching doesn't fix up our standard handles; a GUI-subsystem
        // process started with none. Reopen the console device for whichever
        // of them is still unset.
        let out = open(w!("CONOUT$"), true);
        if out == INVALID_HANDLE_VALUE {
            return false;
        }
        for handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            if !is_usable(handle) {
                let _ = SetStdHandle(handle, out);
            }
        }
        if !is_usable(STD_INPUT_HANDLE) {
            let inp = open(w!("CONIN$"), false);
            if inp != INVALID_HANDLE_VALUE {
                let _ = SetStdHandle(STD_INPUT_HANDLE, inp);
            }
        }
        true
    }
}

/// Is this standard handle already pointing at something? An unset one comes
/// back as null or `INVALID_HANDLE_VALUE`.
unsafe fn is_usable(which: STD_HANDLE) -> bool {
    match GetStdHandle(which) {
        Ok(h) => !h.is_invalid() && !h.0.is_null(),
        Err(_) => false,
    }
}

unsafe fn open(name: windows::core::PCWSTR, write: bool) -> HANDLE {
    let access = if write {
        FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0
    } else {
        FILE_GENERIC_READ.0
    };
    CreateFileW(
        name,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        None,
    )
    .unwrap_or(INVALID_HANDLE_VALUE)
}

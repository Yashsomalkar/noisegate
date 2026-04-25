//! Terminal banner + green log theme. Pre-alpha hacker-grade chrome.
//!
//! On Windows the console doesn't process ANSI escape sequences unless
//! `ENABLE_VIRTUAL_TERMINAL_PROCESSING` is set; modern Windows Terminal /
//! PowerShell do that automatically, legacy conhost may not. We toggle it
//! defensively at startup.

const GREEN: &str = "\x1b[32m";
const BRIGHT_GREEN: &str = "\x1b[1;92m";
const DIM_GREEN: &str = "\x1b[2;32m";
const RESET: &str = "\x1b[0m";

/// "ANSI Shadow" figlet font, hand-edited to fit 80 cols.
const ASCII_ART: &str = r#"
███╗   ██╗ ██████╗ ██╗███████╗███████╗ ██████╗  █████╗ ████████╗███████╗
████╗  ██║██╔═══██╗██║██╔════╝██╔════╝██╔════╝ ██╔══██╗╚══██╔══╝██╔════╝
██╔██╗ ██║██║   ██║██║███████╗█████╗  ██║  ███╗███████║   ██║   █████╗
██║╚██╗██║██║   ██║██║╚════██║██╔══╝  ██║   ██║██╔══██║   ██║   ██╔══╝
██║ ╚████║╚██████╔╝██║███████║███████╗╚██████╔╝██║  ██║   ██║   ███████╗
╚═╝  ╚═══╝ ╚═════╝ ╚═╝╚══════╝╚══════╝ ╚═════╝ ╚═╝  ╚═╝   ╚═╝   ╚══════╝
"#;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn print() {
    enable_vt_processing();

    // Banner in bright green, tagline in normal green, divider dim.
    print!("{BRIGHT_GREEN}{ASCII_ART}{RESET}");
    println!(
        "{GREEN}    real-time mic noise cancellation  •  windows  •  rust{RESET}"
    );
    println!(
        "{DIM_GREEN}    v{VERSION}  •  https://github.com/Yashsomalkar/noisegate{RESET}"
    );
    println!(
        "{DIM_GREEN}    ────────────────────────────────────────────────────────────────────────{RESET}"
    );
}

#[cfg(windows)]
fn enable_vt_processing() {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };
    unsafe {
        let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) else { return };
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(h, &mut mode).is_ok() {
            let _ = SetConsoleMode(h, CONSOLE_MODE(mode.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0));
        }
    }
}

#[cfg(not(windows))]
fn enable_vt_processing() {}

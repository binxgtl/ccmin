//! Minimal ANSI styling. No dependencies; colour is disabled when the
//! `NO_COLOR` environment variable is set or `--no-color` was passed.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn init(force_off: bool) {
    #[allow(unused_mut)]
    let mut on = !force_off && std::env::var_os("NO_COLOR").is_none();
    // On Windows this doubles as an isatty check: GetConsoleMode fails when
    // stdout is a pipe or a file, and escape codes would just be litter.
    #[cfg(windows)]
    if on {
        on = enable_vt();
    }
    ENABLED.store(on, Ordering::Relaxed);
}

/// Carriage return plus erase-to-end-of-line, or nothing when styling is off.
pub fn clear_line() -> &'static str {
    if on() {
        "\r\x1b[K"
    } else {
        "\n"
    }
}

fn on() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// True when we are writing to a real terminal, so in-place progress updates
/// make sense. When output is piped they would just pile up as junk.
pub fn interactive() -> bool {
    on()
}

macro_rules! style {
    ($name:ident, $code:expr) => {
        pub fn $name(s: &str) -> String {
            if on() {
                format!("\x1b[{}m{}\x1b[0m", $code, s)
            } else {
                s.to_string()
            }
        }
    };
}

style!(bold, "1");
style!(dim, "2");
style!(red, "31");
style!(green, "32");
style!(yellow, "33");
style!(cyan, "36");

pub fn rule() -> String {
    dim(&"-".repeat(58))
}

/// Windows 10+ consoles support ANSI sequences but only once
/// ENABLE_VIRTUAL_TERMINAL_PROCESSING is set on the output handle.
/// Returns false when stdout is not a console at all.
#[cfg(windows)]
fn enable_vt() -> bool {
    // Declared locally so we stay dependency-free (no `winapi`/`windows` crate).
    type Handle = *mut core::ffi::c_void;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> Handle;
        fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: Handle, mode: u32) -> i32;
    }

    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h as isize == -1 {
            return false;
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(h, &mut mode) == 0 {
            // Not a real console (piped or redirected).
            return false;
        }
        SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        true
    }
}

//! Keeps spawned console programs from opening a console window.
//!
//! This lives outside `engine` because the probe binary includes the modules it
//! needs on its own, and a helper that reaches back into the app's module graph
//! cannot be shared that way.

use std::process::Command;

/// Dictate is a GUI binary and owns no console, so Windows hands every console
/// program it starts a new console window — with curl that is a black rectangle
/// flashing on screen for as long as the request runs. CREATE_NO_WINDOW is the
/// documented way to ask for no console at all. Everywhere else this is a no-op.
pub fn hide_console(command: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = command;
    }
}

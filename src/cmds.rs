//! Timer verbs wired to the systemd wake/hold/ping units.
//!
//! Plan: "Sleep lock, wake, ping" — `wake-arm` arms the wake timer's quota
//! window, `hold` extends the user hold timer (15 m), `ping-window` opens a
//! ping quota window (lateness guard >10 min). Registered as hidden
//! subcommands so they are reachable by the units but not part of the
//! everyday CLI surface.

/// `wake-arm`: arm the wake timer's quota window.
pub fn wake_arm() -> anyhow::Result<()> {
    eprintln!("not implemented yet: wake-arm");
    Ok(())
}

/// `hold`: extend the user hold timer by 15 minutes.
pub fn hold() -> anyhow::Result<()> {
    eprintln!("not implemented yet: hold");
    Ok(())
}

/// `ping-window`: open a ping quota window via `claude -p`.
pub fn ping_window() -> anyhow::Result<()> {
    eprintln!("not implemented yet: ping-window");
    Ok(())
}

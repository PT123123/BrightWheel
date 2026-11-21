//! Per-user autostart registration that survives version upgrades.
//!
//! The upstream `brightwheel::autostart::is_enabled()` compares the registry
//! value against the *current* executable path byte-for-byte. Because this app
//! is deployed into a versioned folder (`C:\workshop\BrightWheel-<ver>`), every
//! upgrade changes the path and the old entry is wrongly reported as
//! "disabled", while Windows still launches the stale exe. We own the entry
//! here: presence means enabled, and the stored path is refreshed on startup
//! so upgrades keep pointing at the newest binary.

use anyhow::{Context, Result};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "BrightWheel";

fn run_key(write: bool) -> Result<RegKey> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let flags = if write { KEY_READ | KEY_WRITE } else { KEY_READ };
    hkcu.open_subkey_with_flags(RUN_KEY, flags)
        .context("open Run key")
}

fn current_command() -> Result<String> {
    let exe = std::env::current_exe().context("resolve current exe")?;
    Ok(format!("\"{}\"", exe.display()))
}

/// Presence-based check: a stored command for BrightWheel means enabled,
/// regardless of whether the path is from an older version folder.
pub fn is_enabled() -> bool {
    run_key(false)
        .and_then(|key| key.get_value::<String, _>(VALUE_NAME).context("read value"))
        .map(|command: String| command.to_ascii_lowercase().contains("brightwheel"))
        .unwrap_or(false)
}

/// The raw command currently stored, for display in Settings.
pub fn registered_command() -> Option<String> {
    run_key(false)
        .ok()
        .and_then(|key| key.get_value::<String, _>(VALUE_NAME).ok())
}

/// Whether the stored command still points at the exe currently running.
pub fn is_current() -> bool {
    let Ok(current) = current_command() else {
        return false;
    };
    match registered_command() {
        Some(stored) => stored.eq_ignore_ascii_case(&current),
        None => false,
    }
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let key = run_key(true)?;
    if enabled {
        key.set_value(VALUE_NAME, &current_command()?)
            .context("write autostart value")?;
    } else {
        // Deleting a missing value is harmless on Windows.
        let _ = key.delete_value(VALUE_NAME);
    }
    Ok(())
}

/// On startup, if autostart is enabled but points at an older version folder,
/// rewrite it to the exe currently running so upgrades stay seamless.
pub fn reconcile() -> Result<()> {
    if is_enabled() && !is_current() {
        set_enabled(true)?;
    }
    Ok(())
}

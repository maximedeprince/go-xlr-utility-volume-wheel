use std::io;
use std::path::Path;

use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APP_NAME: &str = "GoXLR Volume Wheel";

pub fn is_enabled() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(run_key) = hkcu.open_subkey(RUN_KEY) else {
        return false;
    };
    match run_key.get_value::<String, _>(APP_NAME) {
        Ok(value) => paths_equivalent(&value, &exe),
        Err(_) => false,
    }
}

pub fn enable() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run_key, _) = hkcu.create_subkey(RUN_KEY)?;
    // Quote the path so it survives spaces in user folders.
    let value = format!("\"{}\"", exe.to_string_lossy());
    run_key.set_value(APP_NAME, &value)
}

pub fn disable() -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run_key = match hkcu.open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE) {
        Ok(k) => k,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    match run_key.delete_value(APP_NAME) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn paths_equivalent(stored: &str, current: &Path) -> bool {
    let stored = stored.trim().trim_matches('"');
    let current = current.to_string_lossy();
    stored.eq_ignore_ascii_case(current.as_ref())
}

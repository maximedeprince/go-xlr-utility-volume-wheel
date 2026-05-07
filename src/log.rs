use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::SystemInformation::GetLocalTime;

static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub fn init() {
    let Some(path) = log_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_LOG_BYTES {
        let _ = std::fs::write(&path, b"");
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = LOG_FILE.set(Mutex::new(file));
    }
    info("--- launch ---");
}

pub fn log_path() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(local)
            .join("GoXLR Volume Wheel")
            .join("app.log"),
    )
}

pub fn error(msg: &str) {
    write("ERROR", msg);
}

pub fn info(msg: &str) {
    write("INFO", msg);
}

fn write(level: &str, msg: &str) {
    let Some(file) = LOG_FILE.get() else { return };
    let Ok(mut f) = file.lock() else { return };
    let _ = writeln!(f, "{} [{}] {}", timestamp(), level, msg);
}

fn timestamp() -> String {
    let st = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

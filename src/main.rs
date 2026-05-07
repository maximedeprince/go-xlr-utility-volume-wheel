#![windows_subsystem = "windows"]

mod autostart;
mod goxlr;
mod hook;
mod log;
mod tray;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;

const DEFAULT_CHANNEL: &str = "Game";

fn main() {
    log::init();

    let active_channel = Arc::new(RwLock::new(DEFAULT_CHANNEL.to_string()));
    let connected = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::unbounded_channel::<hook::VolumeEvent>();

    // Low-level keyboard hook — owns its own Win32 message pump.
    {
        let connected = connected.clone();
        std::thread::Builder::new()
            .name("ll-keyboard-hook".into())
            .spawn(move || {
                hook::run_hook(tx, connected);
            })
            .expect("spawn hook thread");
    }

    // GoXLR WebSocket client — dedicated single-threaded tokio runtime.
    {
        let active_channel = active_channel.clone();
        let connected = connected.clone();
        std::thread::Builder::new()
            .name("goxlr-client".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(goxlr::run_client(rx, active_channel, connected));
            })
            .expect("spawn goxlr thread");
    }

    // Tray icon must run on the process main thread on Windows.
    tray::run(active_channel, connected);
}

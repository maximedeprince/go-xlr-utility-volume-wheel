#![windows_subsystem = "windows"]

mod autostart;
mod config;
mod goxlr;
mod hook;
mod log;
mod osd;
mod settings;
mod tray;

use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;

fn main() {
    log::init();
    let cfg = Arc::new(RwLock::new(config::load()));
    osd::start();

    // Active channel is shared with goxlr / tray / settings as the
    // canonical ALL_CHANNELS index. AtomicUsize keeps the hot read path
    // (per volume event in goxlr.rs) lock-free.
    let initial_idx = cfg
        .read()
        .map(|c| config::channel_index(&c.default_channel))
        .unwrap_or(0);
    let active_channel = Arc::new(AtomicUsize::new(initial_idx));
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

    // Tray icon must run on the process main thread on Windows. Also
    // owns the cycle-channel hotkey since WM_HOTKEY is delivered to the
    // thread that called RegisterHotKey.
    tray::run(active_channel, connected, cfg);
}

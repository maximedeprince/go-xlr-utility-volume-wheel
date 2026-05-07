use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, KillTimer, SetTimer, TranslateMessage, MSG, WM_TIMER,
};

/// Channels supported by the GoXLR Utility daemon.
const CHANNELS: &[&str] = &[
    "Mic",
    "LineIn",
    "Console",
    "System",
    "Game",
    "Chat",
    "Sample",
    "Music",
    "Headphones",
    "MicMonitor",
    "LineOut",
];

/// Tray brand colour when the daemon is connected.
const COLOR_CONNECTED: [u8; 4] = [0xFB, 0x9C, 0x33, 0xFF];
/// Muted grey when the daemon is unreachable, so the user notices at a
/// glance without us having to flash anything.
const COLOR_DISCONNECTED: [u8; 4] = [0x80, 0x80, 0x80, 0xFF];

const STATUS_TIMER_ID: usize = 1;
const STATUS_INTERVAL_MS: u32 = 1_000;

pub fn run(active_channel: Arc<RwLock<String>>, connected: Arc<AtomicBool>) {
    let menu = Menu::new();

    let header = MenuItem::new("GoXLR Volume Wheel — channel", false, None);
    menu.append(&header).expect("append header");
    menu.append(&PredefinedMenuItem::separator())
        .expect("append separator");

    let initial = active_channel
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut by_id: HashMap<MenuId, &'static str> = HashMap::new();
    let mut items: HashMap<&'static str, CheckMenuItem> = HashMap::new();

    for &channel in CHANNELS {
        let item = CheckMenuItem::new(channel, true, channel == initial.as_str(), None);
        menu.append(&item).expect("append channel item");
        by_id.insert(item.id().clone(), channel);
        items.insert(channel, item);
    }

    menu.append(&PredefinedMenuItem::separator())
        .expect("append separator");
    let status_item = MenuItem::new("GoXLR Utility: connecting…", false, None);
    menu.append(&status_item).expect("append status");
    let reinstall_item = MenuItem::new("Reinstall keyboard hook", true, None);
    menu.append(&reinstall_item).expect("append reinstall");
    let reinstall_id = reinstall_item.id().clone();
    let log_item = MenuItem::new("Open log folder", true, None);
    menu.append(&log_item).expect("append log");
    let log_id = log_item.id().clone();

    menu.append(&PredefinedMenuItem::separator())
        .expect("append separator");
    let autostart_item = CheckMenuItem::new(
        "Start with Windows",
        true,
        crate::autostart::is_enabled(),
        None,
    );
    menu.append(&autostart_item).expect("append autostart");
    let autostart_id = autostart_item.id().clone();

    menu.append(&PredefinedMenuItem::separator())
        .expect("append separator");
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&quit).expect("append quit");
    let quit_id = quit.id().clone();

    let icon_connected =
        Icon::from_rgba(build_icon(&COLOR_CONNECTED), 32, 32).expect("build connected icon");
    let icon_disconnected =
        Icon::from_rgba(build_icon(&COLOR_DISCONNECTED), 32, 32).expect("build disconnected icon");
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon_disconnected.clone())
        .with_tooltip(format!("GoXLR Volume Wheel — {} (connecting…)", initial))
        .build()
        .expect("build tray");

    let menu_rx = MenuEvent::receiver();

    let mut last_connected: Option<bool> = None;
    let apply_status = |is_connected: bool, channel: &str| {
        let (status_text, tooltip, icon) = if is_connected {
            (
                "GoXLR Utility: connected".to_string(),
                format!("GoXLR Volume Wheel — {}", channel),
                icon_connected.clone(),
            )
        } else {
            (
                "GoXLR Utility: offline — is the daemon running?".to_string(),
                format!("GoXLR Volume Wheel — {} (offline)", channel),
                icon_disconnected.clone(),
            )
        };
        status_item.set_text(status_text);
        let _ = tray.set_icon(Some(icon));
        let _ = tray.set_tooltip(Some(tooltip));
    };

    unsafe {
        let _ = SetTimer(HWND::default(), STATUS_TIMER_ID, STATUS_INTERVAL_MS, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            if msg.message == WM_TIMER && msg.wParam.0 == STATUS_TIMER_ID {
                let now = connected.load(Ordering::Acquire);
                if last_connected != Some(now) {
                    let channel = active_channel
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    apply_status(now, &channel);
                    last_connected = Some(now);
                }
            }

            while let Ok(event) = menu_rx.try_recv() {
                if event.id == quit_id {
                    std::process::exit(0);
                }
                if event.id == reinstall_id {
                    crate::hook::request_rehook();
                    continue;
                }
                if event.id == log_id {
                    if let Some(path) = crate::log::log_path() {
                        if let Some(parent) = path.parent() {
                            let _ = std::process::Command::new("explorer.exe")
                                .arg(parent)
                                .spawn();
                        }
                    }
                    continue;
                }
                if event.id == autostart_id {
                    let want = autostart_item.is_checked();
                    let result = if want {
                        crate::autostart::enable()
                    } else {
                        crate::autostart::disable()
                    };
                    if result.is_err() {
                        // Roll back the visual toggle if the registry call failed.
                        autostart_item.set_checked(!want);
                    }
                    continue;
                }
                if let Some(&name) = by_id.get(&event.id) {
                    *active_channel.write().unwrap_or_else(|e| e.into_inner()) = name.to_string();
                    for (other, item) in &items {
                        item.set_checked(*other == name);
                    }
                    apply_status(connected.load(Ordering::Acquire), name);
                }
            }
        }

        let _ = KillTimer(HWND::default(), STATUS_TIMER_ID);
    }
}

/// 32×32 RGBA speaker icon, drawn programmatically (no asset shipped).
fn build_icon(color: &[u8; 4]) -> Vec<u8> {
    const N: i32 = 32;
    let mut rgba = vec![0u8; (N * N * 4) as usize];

    // Speaker box
    for y in 12..21 {
        for x in 4..11 {
            put(&mut rgba, N, x, y, color);
        }
    }
    // Trapezoidal cone
    for x in 11..18 {
        let h = x - 11;
        for y in (10 - h)..(22 + h) {
            put(&mut rgba, N, x, y, color);
        }
    }
    // Sound waves
    for y in 13..=19 {
        put(&mut rgba, N, 20, y, color);
    }
    for y in 11..=21 {
        put(&mut rgba, N, 23, y, color);
    }
    for y in 9..=23 {
        put(&mut rgba, N, 26, y, color);
    }

    rgba
}

fn put(rgba: &mut [u8], n: i32, x: i32, y: i32, color: &[u8; 4]) {
    if (0..n).contains(&x) && (0..n).contains(&y) {
        let i = ((y * n + x) * 4) as usize;
        rgba[i..i + 4].copy_from_slice(color);
    }
}

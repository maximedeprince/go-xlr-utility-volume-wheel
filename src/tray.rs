use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG,
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

pub fn run(active_channel: Arc<RwLock<String>>) {
    let menu = Menu::new();

    let header = MenuItem::new("GoXLR Volume Wheel — channel", false, None);
    menu.append(&header).expect("append header");
    menu.append(&PredefinedMenuItem::separator())
        .expect("append separator");

    let initial = active_channel.read().unwrap().clone();
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
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&quit).expect("append quit");
    let quit_id = quit.id().clone();

    let icon = Icon::from_rgba(build_icon(), 32, 32).expect("build icon");
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip(format!("GoXLR Volume Wheel — {}", initial))
        .build()
        .expect("build tray");

    let menu_rx = MenuEvent::receiver();

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            while let Ok(event) = menu_rx.try_recv() {
                if event.id == quit_id {
                    std::process::exit(0);
                }
                if let Some(&name) = by_id.get(&event.id) {
                    *active_channel.write().unwrap() = name.to_string();
                    for (other, item) in &items {
                        item.set_checked(*other == name);
                    }
                    let _ = tray.set_tooltip(Some(format!("GoXLR Volume Wheel — {}", name)));
                }
            }
        }
    }
}

/// 32×32 RGBA speaker icon, drawn programmatically (no asset shipped).
fn build_icon() -> Vec<u8> {
    const N: i32 = 32;
    let mut rgba = vec![0u8; (N * N * 4) as usize];
    let color: [u8; 4] = [0xFB, 0x9C, 0x33, 0xFF];

    // Speaker box
    for y in 12..21 {
        for x in 4..11 {
            put(&mut rgba, N, x, y, &color);
        }
    }
    // Trapezoidal cone
    for x in 11..18 {
        let h = x - 11;
        for y in (10 - h)..(22 + h) {
            put(&mut rgba, N, x, y, &color);
        }
    }
    // Sound waves
    for y in 13..=19 {
        put(&mut rgba, N, 20, y, &color);
    }
    for y in 11..=21 {
        put(&mut rgba, N, 23, y, &color);
    }
    for y in 9..=23 {
        put(&mut rgba, N, 26, y, &color);
    }

    rgba
}

fn put(rgba: &mut [u8], n: i32, x: i32, y: i32, color: &[u8; 4]) {
    if (0..n).contains(&x) && (0..n).contains(&y) {
        let i = ((y * n + x) * 4) as usize;
        rgba[i..i + 4].copy_from_slice(color);
    }
}

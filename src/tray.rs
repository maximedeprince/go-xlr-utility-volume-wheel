use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, KillTimer, PostThreadMessageW, SetTimer, TranslateMessage, MSG,
    WM_APP, WM_HOTKEY, WM_TIMER,
};

use crate::config::{self, Config, ALL_CHANNELS};

/// Posted by the settings window while it's recording a new hotkey, so
/// the tray thread temporarily drops its global registration and the
/// recorder can actually see the keypress.
const WM_HOTKEY_PAUSE: u32 = WM_APP + 5;
/// Posted after recording ends (commit or cancel). The tray re-reads
/// the current spec from config and re-registers — this is also how
/// the hotkey is live-updated after a successful record, with no
/// restart required.
const WM_HOTKEY_RESUME: u32 = WM_APP + 6;

/// Tray thread id, captured once `run()` starts. Lets cross-thread
/// helpers (`pause_hotkey` / `resume_hotkey`) post work back without
/// needing a window handle.
static TRAY_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// Temporarily releases the global cycle hotkey so the settings window
/// can capture it during recording. Safe to call from any thread.
pub fn pause_hotkey() {
    let tid = TRAY_THREAD_ID.load(Ordering::Acquire);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_HOTKEY_PAUSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Asks the tray thread to re-register the cycle hotkey using whatever
/// spec is currently in config. Doubles as the live-update path after a
/// successful record.
pub fn resume_hotkey() {
    let tid = TRAY_THREAD_ID.load(Ordering::Acquire);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_HOTKEY_RESUME, WPARAM(0), LPARAM(0));
        }
    }
}

/// Tray brand colour when the daemon is connected.
const COLOR_CONNECTED: [u8; 4] = [0xFB, 0x9C, 0x33, 0xFF];
/// Muted grey when the daemon is unreachable, so the user notices at a
/// glance without us having to flash anything.
const COLOR_DISCONNECTED: [u8; 4] = [0x80, 0x80, 0x80, 0xFF];

const STATUS_TIMER_ID: usize = 1;
const STATUS_INTERVAL_MS: u32 = 1_000;
const CYCLE_HOTKEY_ID: i32 = 0xC0DE;

/// Bundle of menu state that has to be replaced together when visibility
/// changes — the channel items live in the menu, so swapping any of them
/// means swapping the whole menu and rebuilding the lookup maps.
struct MenuBundle {
    by_id: HashMap<MenuId, &'static str>,
    items: HashMap<&'static str, CheckMenuItem>,
    settings_id: MenuId,
    quit_id: MenuId,
    status_item: MenuItem,
}

pub fn run(
    active_channel: Arc<AtomicUsize>,
    connected: Arc<AtomicBool>,
    config: Arc<RwLock<Config>>,
) {
    let icon_connected =
        Icon::from_rgba(build_icon(&COLOR_CONNECTED), 32, 32).expect("build connected icon");
    let icon_disconnected =
        Icon::from_rgba(build_icon(&COLOR_DISCONNECTED), 32, 32).expect("build disconnected icon");

    let initial_idx = active_channel.load(Ordering::Acquire);
    let initial = config::channel_name(initial_idx);
    let mut visible_snapshot = visible_channels(&config);
    let (menu, mut bundle) = build_menu(&visible_snapshot, initial);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon_disconnected.clone())
        .with_tooltip(format!("GoXLR Volume Wheel — {} (connecting…)", initial))
        .build()
        .expect("build tray");

    // Left-click should open the Settings window, not the menu.
    // Right-click keeps the default behaviour (context menu).
    tray.set_show_menu_on_left_click(false);

    let menu_rx = MenuEvent::receiver();
    let tray_rx = TrayIconEvent::receiver();

    let mut last_connected: Option<bool> = None;
    let mut last_idx = initial_idx;
    apply_status(
        &bundle,
        &tray,
        &icon_connected,
        &icon_disconnected,
        false,
        initial,
    );

    TRAY_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Release);

    let cycle_spec = config.read().ok().and_then(|c| c.cycle_hotkey.clone());
    let mut cycle_registered = unsafe { register_cycle_hotkey(cycle_spec.as_deref()) };

    unsafe {
        let _ = SetTimer(HWND::default(), STATUS_TIMER_ID, STATUS_INTERVAL_MS, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            if msg.message == WM_TIMER && msg.wParam.0 == STATUS_TIMER_ID {
                // Re-sync from shared state every second so the menu
                // tracks edits made by the cycle hotkey *or* by the
                // Settings window without us having to invent a
                // cross-thread signal.
                let current_idx = active_channel.load(Ordering::Acquire);
                let current_name = config::channel_name(current_idx);
                let now = connected.load(Ordering::Acquire);
                let current_visible = visible_channels(&config);

                if current_visible != visible_snapshot {
                    let (new_menu, new_bundle) = build_menu(&current_visible, current_name);
                    tray.set_menu(Some(Box::new(new_menu)));
                    bundle = new_bundle;
                    visible_snapshot = current_visible;
                    apply_status(
                        &bundle,
                        &tray,
                        &icon_connected,
                        &icon_disconnected,
                        now,
                        current_name,
                    );
                    last_idx = current_idx;
                    last_connected = Some(now);
                } else if last_idx != current_idx {
                    for (other, item) in &bundle.items {
                        item.set_checked(*other == current_name);
                    }
                    apply_status(
                        &bundle,
                        &tray,
                        &icon_connected,
                        &icon_disconnected,
                        now,
                        current_name,
                    );
                    last_idx = current_idx;
                    last_connected = Some(now);
                } else if last_connected != Some(now) {
                    apply_status(
                        &bundle,
                        &tray,
                        &icon_connected,
                        &icon_disconnected,
                        now,
                        current_name,
                    );
                    last_connected = Some(now);
                }
            }

            if msg.message == WM_HOTKEY_PAUSE && cycle_registered {
                let _ = UnregisterHotKey(HWND::default(), CYCLE_HOTKEY_ID);
                cycle_registered = false;
            }

            if msg.message == WM_HOTKEY_RESUME {
                if cycle_registered {
                    let _ = UnregisterHotKey(HWND::default(), CYCLE_HOTKEY_ID);
                }
                let spec = config.read().ok().and_then(|c| c.cycle_hotkey.clone());
                cycle_registered = register_cycle_hotkey(spec.as_deref());
            }

            if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == CYCLE_HOTKEY_ID {
                if let Some(next) = cycle_next(&active_channel, &config) {
                    let next_name = config::channel_name(next);
                    apply_channel_choice(next_name, &active_channel, &config, &bundle.items);
                    apply_status(
                        &bundle,
                        &tray,
                        &icon_connected,
                        &icon_disconnected,
                        connected.load(Ordering::Acquire),
                        next_name,
                    );
                    crate::osd::show_channel_switch(next_name);
                    last_idx = next;
                }
            }

            while let Ok(event) = tray_rx.try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    crate::settings::open(config.clone(), active_channel.clone());
                } else if let TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } = event
                {
                    crate::settings::open(config.clone(), active_channel.clone());
                }
            }

            while let Ok(event) = menu_rx.try_recv() {
                if event.id == bundle.quit_id {
                    if cycle_registered {
                        let _ = UnregisterHotKey(HWND::default(), CYCLE_HOTKEY_ID);
                    }
                    std::process::exit(0);
                }
                if event.id == bundle.settings_id {
                    crate::settings::open(config.clone(), active_channel.clone());
                    continue;
                }
                if let Some(&name) = bundle.by_id.get(&event.id) {
                    apply_channel_choice(name, &active_channel, &config, &bundle.items);
                    apply_status(
                        &bundle,
                        &tray,
                        &icon_connected,
                        &icon_disconnected,
                        connected.load(Ordering::Acquire),
                        name,
                    );
                    last_idx = config::channel_index(name);
                }
            }
        }

        if cycle_registered {
            let _ = UnregisterHotKey(HWND::default(), CYCLE_HOTKEY_ID);
        }
        let _ = KillTimer(HWND::default(), STATUS_TIMER_ID);
    }
}

fn visible_channels(config: &Arc<RwLock<Config>>) -> Vec<&'static str> {
    let snapshot = config
        .read()
        .map(|c| c.visible_channels.clone())
        .unwrap_or_default();
    ALL_CHANNELS
        .iter()
        .copied()
        .filter(|c| snapshot.iter().any(|v| v == *c))
        .collect()
}

fn build_menu(visible: &[&'static str], active: &str) -> (Menu, MenuBundle) {
    let menu = Menu::new();

    let header = MenuItem::new("GoXLR Volume Wheel — channel", false, None);
    menu.append(&header).expect("append header");
    menu.append(&PredefinedMenuItem::separator())
        .expect("append separator");

    let mut by_id: HashMap<MenuId, &'static str> = HashMap::new();
    let mut items: HashMap<&'static str, CheckMenuItem> = HashMap::new();
    for &channel in visible {
        let item = CheckMenuItem::new(channel, true, channel == active, None);
        menu.append(&item).expect("append channel item");
        by_id.insert(item.id().clone(), channel);
        items.insert(channel, item);
    }

    menu.append(&PredefinedMenuItem::separator())
        .expect("append separator");
    let status_item = MenuItem::new("GoXLR Utility: connecting…", false, None);
    menu.append(&status_item).expect("append status");
    let settings_item = MenuItem::new("Settings…", true, None);
    menu.append(&settings_item).expect("append settings");
    let settings_id = settings_item.id().clone();

    menu.append(&PredefinedMenuItem::separator())
        .expect("append separator");
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&quit).expect("append quit");
    let quit_id = quit.id().clone();

    (
        menu,
        MenuBundle {
            by_id,
            items,
            settings_id,
            quit_id,
            status_item,
        },
    )
}

fn apply_status(
    bundle: &MenuBundle,
    tray: &TrayIcon,
    icon_connected: &Icon,
    icon_disconnected: &Icon,
    is_connected: bool,
    channel: &str,
) {
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
    bundle.status_item.set_text(status_text);
    let _ = tray.set_icon(Some(icon));
    let _ = tray.set_tooltip(Some(tooltip));
}

/// Sets the active channel and syncs the tray menu check-marks. The
/// startup `default_channel` is *not* touched here — that's an explicit
/// star click in the settings window, intentionally separate from the
/// transient "which fader am I controlling right now" selection.
fn apply_channel_choice(
    name: &str,
    active_channel: &Arc<AtomicUsize>,
    _config: &Arc<RwLock<Config>>,
    items: &HashMap<&'static str, CheckMenuItem>,
) {
    active_channel.store(config::channel_index(name), Ordering::Release);
    for (other, item) in items {
        item.set_checked(*other == name);
    }
}

/// Computes the next visible channel after the current active one,
/// wrapping at the end. Returns `None` if no channel is visible.
fn cycle_next(active_channel: &Arc<AtomicUsize>, config: &Arc<RwLock<Config>>) -> Option<usize> {
    let visible = config
        .read()
        .ok()
        .map(|c| c.visible_channels.clone())
        .unwrap_or_default();
    if visible.is_empty() {
        return None;
    }
    let current_name = config::channel_name(active_channel.load(Ordering::Acquire));
    let pos = visible
        .iter()
        .position(|c| c == current_name)
        .map(|i| (i + 1) % visible.len())
        .unwrap_or(0);
    Some(config::channel_index(&visible[pos]))
}

unsafe fn register_cycle_hotkey(spec: Option<&str>) -> bool {
    let Some(spec) = spec else { return false };
    let Some((mods, vk)) = parse_hotkey(spec) else {
        crate::log::error(&format!("cycle_hotkey malformed: {}", spec));
        return false;
    };
    if RegisterHotKey(HWND::default(), CYCLE_HOTKEY_ID, mods, vk).is_ok() {
        crate::log::info(&format!("cycle hotkey registered: {}", spec));
        true
    } else {
        crate::log::error(&format!("cycle hotkey already in use: {}", spec));
        false
    }
}

/// `"ctrl+shift+alt+v"` → (modifiers, virtual-key). Supports
/// `ctrl`/`control`, `shift`, `alt`, `win`/`super`, plus a single
/// alphanumeric or `f1`–`f24` for the key.
fn parse_hotkey(spec: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = MOD_NOREPEAT;
    let mut vk: Option<u32> = None;
    for part in spec.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "shift" => mods |= MOD_SHIFT,
            "alt" => mods |= MOD_ALT,
            "win" | "super" => mods |= MOD_WIN,
            key => {
                if key.len() == 1 {
                    let c = key.chars().next()?;
                    if c.is_ascii_alphanumeric() {
                        vk = Some(c.to_ascii_uppercase() as u32);
                    } else {
                        return None;
                    }
                } else if let Some(n_str) = key.strip_prefix('f') {
                    let n: u32 = n_str.parse().ok()?;
                    if !(1..=24).contains(&n) {
                        return None;
                    }
                    // VK_F1 = 0x70 … VK_F24 = 0x87.
                    vk = Some(0x70 + n - 1);
                } else {
                    return None;
                }
            }
        }
    }
    vk.map(|v| (mods, v))
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

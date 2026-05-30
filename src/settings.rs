//! Custom-painted settings window. Matches the OSD aesthetic (dark canvas,
//! GoXLR-orange accent) and double-buffers every frame so hovering doesn't
//! flicker. Native controls are deliberately avoided because Windows's
//! theming routinely overrides any styling on built-in widgets — owning
//! the paint pipeline is shorter than fighting it.

use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, Ellipse, EndPaint, FillRect,
    GetMonitorInfoW, InvalidateRect, MonitorFromWindow, RoundRect, SelectObject, SetBkMode,
    SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DT_CENTER, DT_LEFT,
    DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, FW_NORMAL, FW_SEMIBOLD, HDC, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, OUT_OUTLINE_PRECIS, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_CONTROL, VK_ESCAPE,
    VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect,
    GetMessageW, KillTimer, LoadCursorW, PostMessageW, RegisterClassExW, SetCursor,
    SetForegroundWindow, SetTimer, ShowWindow, TranslateMessage, HCURSOR, IDC_ARROW, IDC_HAND, MSG,
    SW_RESTORE, SW_SHOWNORMAL, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE,
    WM_PAINT, WM_SETCURSOR, WM_TIMER, WM_USER, WNDCLASSEXW, WS_CAPTION, WS_EX_APPWINDOW,
    WS_OVERLAPPED, WS_SYSMENU, WS_VISIBLE,
};

use crate::config::{self, Config, ALL_CHANNELS};

/// Hardcoded because windows-rs 0.58 doesn't export it as a constant.
const WM_MOUSELEAVE: u32 = 0x02A3;

const WIN_TITLE: PCWSTR = w!("GoXLR Volume Wheel — Settings");
const WND_CLASS: PCWSTR = w!("GoXLRVolumeWheelSettings");

const CLIENT_W: i32 = 460;
const CLIENT_H: i32 = 780;

// COLORREF is BBGGRR. The palette is intentionally small — one accent,
// three text levels, three surfaces — so anywhere a colour is reused
// the user recognises the intent.
const BG: u32 = 0x00141414;
const CARD: u32 = 0x001E1E1E;
const CARD_HOVER: u32 = 0x00282828;
/// Tint applied to the row that is *currently being controlled* by the
/// volume wheel. A warmer, lifted surface so the eye locks onto it.
const CARD_ACTIVE: u32 = 0x002A3245;
const ACCENT: u32 = 0x00339CFB; // GoXLR orange #FB9C33 → BGR
const ACCENT_HOVER: u32 = 0x0046ADFB;
const ACCENT_SOFT: u32 = 0x001D5F95;
const TEXT: u32 = 0x00F5F5F5;
const TEXT_DIM: u32 = 0x00B0B0B0;
const TEXT_FAINT: u32 = 0x00707070;
const BORDER: u32 = 0x002C2C2C;
const DIVIDER: u32 = 0x00262626;

const PADDING: i32 = 22;
const SECTION_GAP: i32 = 20;
const HEADER_H: i32 = 16;
const ROW_H: i32 = 38;
const CARD_RADIUS: i32 = 10;
const PILL_RADIUS: i32 = 8;
const HELP_BTN: i32 = 26;
const ICON_BTN: i32 = 30;
const SWITCH_W: i32 = 46;
const SWITCH_H: i32 = 24;

const WM_BRING_FRONT: u32 = WM_USER + 1;
const ACTIVE_POLL_TIMER_ID: usize = 1;
const ACTIVE_POLL_MS: u32 = 400;

static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum HelpLang {
    En,
    Fr,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Select this channel as the one the volume wheel currently
    /// controls. Transient — never written to disk.
    SetActive(usize),
    /// Mark this channel as the startup default. Persisted to
    /// `config.json`; does not affect the active channel until the next
    /// app launch.
    SetFavorite(usize),
    /// Toggle the channel's presence in the cycle hotkey rotation and
    /// the tray right-click menu.
    ToggleVisible(usize),
    RecordHotkey,
    ToggleAutostart,
    ReinstallHook,
    OpenLogFolder,
    OpenConfigFolder,
    OpenHelp,
    CloseHelp,
    SetHelpLang(HelpLang),
}

struct HitRegion {
    rect: RECT,
    action: Action,
}

struct Shared {
    config: Arc<RwLock<Config>>,
    active_channel: Arc<AtomicUsize>,
}

struct WindowState {
    shared: Shared,
    regions: Vec<HitRegion>,
    hovered: Option<usize>,
    recording: bool,
    autostart: bool,
    mouse_tracking: bool,
    show_help: bool,
    help_lang: HelpLang,
    /// Last `active_channel` snapshot the paint pass saw. Compared on
    /// the polling timer so we repaint when the tray or the cycle
    /// hotkey changes the active channel out from under us.
    last_seen_active: usize,
}

struct Fonts {
    header: HFONT,
    body: HFONT,
    strong: HFONT,
    name: HFONT,
    name_strong: HFONT,
    /// Segoe MDL2 Assets — system icon font shipped on every Windows 10
    /// / 11 build. Codepoints we use:
    ///   \u{E734} FavoriteStar     \u{E735} FavoriteStarFill
    ///   \u{E7B3} RedEye           \u{ED1A} Hide
    /// All four are clean vector glyphs, far nicer than anything we
    /// could draw with GDI primitives at icon-size.
    icon: HFONT,
    /// Larger variant of the icon font for the help-screen cards.
    icon_large: HFONT,
    title: HFONT,
}

impl Fonts {
    unsafe fn make() -> Self {
        Self {
            header: make_font(-11, FW_SEMIBOLD.0 as i32, w!("Segoe UI")),
            body: make_font(-13, FW_NORMAL.0 as i32, w!("Segoe UI")),
            strong: make_font(-13, FW_SEMIBOLD.0 as i32, w!("Segoe UI")),
            name: make_font(-14, FW_NORMAL.0 as i32, w!("Segoe UI")),
            name_strong: make_font(-15, FW_SEMIBOLD.0 as i32, w!("Segoe UI")),
            icon: make_font(-15, FW_NORMAL.0 as i32, w!("Segoe MDL2 Assets")),
            icon_large: make_font(-22, FW_NORMAL.0 as i32, w!("Segoe MDL2 Assets")),
            title: make_font(-24, FW_SEMIBOLD.0 as i32, w!("Segoe UI")),
        }
    }
    unsafe fn delete(&self) {
        let _ = DeleteObject(self.header);
        let _ = DeleteObject(self.body);
        let _ = DeleteObject(self.strong);
        let _ = DeleteObject(self.name);
        let _ = DeleteObject(self.name_strong);
        let _ = DeleteObject(self.icon);
        let _ = DeleteObject(self.icon_large);
        let _ = DeleteObject(self.title);
    }
}

static SHARED: OnceLock<Mutex<Option<Shared>>> = OnceLock::new();

pub fn open(config: Arc<RwLock<Config>>, active_channel: Arc<AtomicUsize>) {
    let existing = SETTINGS_HWND.load(Ordering::Acquire);
    if existing != 0 {
        unsafe {
            let hwnd = HWND(existing as *mut _);
            let _ = PostMessageW(hwnd, WM_BRING_FRONT, WPARAM(0), LPARAM(0));
        }
        return;
    }

    let shared = Shared {
        config,
        active_channel,
    };
    let slot = SHARED.get_or_init(|| Mutex::new(None));
    if let Ok(mut s) = slot.lock() {
        *s = Some(shared);
    }

    std::thread::Builder::new()
        .name("settings".into())
        .spawn(run)
        .expect("spawn settings thread");
}

fn run() {
    let shared = match SHARED
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|mut g| g.take())
    {
        Some(s) => s,
        None => return,
    };

    unsafe {
        let hinstance = GetModuleHandleW(None).expect("GetModuleHandleW failed");
        let cursor: HCURSOR = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: WND_CLASS,
            hCursor: cursor,
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE;
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: CLIENT_W,
            bottom: CLIENT_H,
        };
        let _ = AdjustWindowRectEx(&mut rect, style, false, WS_EX_APPWINDOW);
        let win_w = rect.right - rect.left;
        let win_h = rect.bottom - rect.top;

        let (x, y) = center_on_active_monitor(win_w, win_h);
        let initial_active = shared.active_channel.load(Ordering::Acquire);
        let state = Box::new(WindowState {
            shared,
            regions: Vec::new(),
            hovered: None,
            recording: false,
            autostart: crate::autostart::is_enabled(),
            mouse_tracking: false,
            show_help: false,
            help_lang: HelpLang::En,
            last_seen_active: initial_active,
        });
        let state_ptr = Box::into_raw(state);

        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            WND_CLASS,
            WIN_TITLE,
            style,
            x,
            y,
            win_w,
            win_h,
            None,
            None,
            hinstance,
            Some(state_ptr as *mut _),
        )
        .expect("CreateWindowExW failed");

        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(20), // DWMWA_USE_IMMERSIVE_DARK_MODE
            &dark as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        );

        SETTINGS_HWND.store(hwnd.0 as isize, Ordering::Release);
        SetTimer(hwnd, ACTIVE_POLL_TIMER_ID, ACTIVE_POLL_MS, None);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        SETTINGS_HWND.store(0, Ordering::Release);
    }
}

unsafe fn center_on_active_monitor(win_w: i32, win_h: i32) -> (i32, i32) {
    let hmon = MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        let r = mi.rcWork;
        let x = r.left + (r.right - r.left - win_w) / 2;
        let y = r.top + (r.bottom - r.top - win_h) / 2;
        (x, y)
    } else {
        (200, 200)
    }
}

// ------------------------------------------------------------- wndproc

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, PostQuitMessage, SetWindowLongPtrW, CREATESTRUCTW, GWLP_USERDATA,
        WM_CREATE,
    };

    if msg == WM_CREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        return LRESULT(0);
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
    if state_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state = &mut *state_ptr;

    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd, state);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if !state.mouse_tracking {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                state.mouse_tracking = true;
            }
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let new_hover = state.regions.iter().position(|r| in_rect(&r.rect, x, y));
            if new_hover != state.hovered {
                state.hovered = new_hover;
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            state.mouse_tracking = false;
            if state.hovered.is_some() {
                state.hovered = None;
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if let Some(action) = state
                .regions
                .iter()
                .find(|r| in_rect(&r.rect, x, y))
                .map(|r| r.action)
            {
                handle_action(hwnd, state, action);
            } else if state.recording {
                stop_recording(state);
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let cursor_name = if state.hovered.is_some() {
                IDC_HAND
            } else {
                IDC_ARROW
            };
            if let Ok(c) = LoadCursorW(None, cursor_name) {
                SetCursor(c);
            }
            LRESULT(1)
        }
        WM_KEYDOWN if state.recording => {
            let vk = wparam.0 as u32;
            if vk == VK_ESCAPE.0 as u32 {
                stop_recording(state);
                let _ = InvalidateRect(hwnd, None, false);
            } else if !is_modifier(vk) {
                if let Some(spec) = build_hotkey_spec(vk) {
                    if let Ok(mut cfg) = state.shared.config.write() {
                        cfg.cycle_hotkey = Some(spec);
                        let snapshot = cfg.clone();
                        drop(cfg);
                        config::save(&snapshot);
                    }
                }
                stop_recording(state);
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == ACTIVE_POLL_TIMER_ID => {
            // The tray and cycle hotkey mutate `active_channel` on
            // another thread. Poll cheaply so the highlighted row
            // doesn't go stale while the settings window is open.
            let current = state.shared.active_channel.load(Ordering::Acquire);
            if current != state.last_seen_active {
                state.last_seen_active = current;
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_BRING_FRONT => {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            if state.recording {
                crate::tray::resume_hotkey();
            }
            let _ = KillTimer(hwnd, ACTIVE_POLL_TIMER_ID);
            let _ = Box::from_raw(state_ptr);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn in_rect(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn stop_recording(state: &mut WindowState) {
    if state.recording {
        state.recording = false;
        crate::tray::resume_hotkey();
    }
}

// -------------------------------------------------------------- actions

unsafe fn handle_action(hwnd: HWND, state: &mut WindowState, action: Action) {
    match action {
        Action::SetActive(i) => {
            // Transient: change what's controlled now without touching
            // the persisted startup channel. The starred channel only
            // changes when the user explicitly clicks the star.
            state.shared.active_channel.store(i, Ordering::Release);
            state.last_seen_active = i;
            let _ = InvalidateRect(hwnd, None, false);
        }
        Action::SetFavorite(i) => {
            let name = ALL_CHANNELS[i];
            if let Ok(mut cfg) = state.shared.config.write() {
                // The startup channel must be visible — otherwise the
                // app would launch into a fader the user already chose
                // to hide.
                if !cfg.visible_channels.iter().any(|c| c == name) {
                    insert_visible_canonical(&mut cfg.visible_channels, name);
                }
                cfg.default_channel = name.to_string();
                let snapshot = cfg.clone();
                drop(cfg);
                config::save(&snapshot);
            }
            let _ = InvalidateRect(hwnd, None, false);
        }
        Action::ToggleVisible(i) => {
            let name = ALL_CHANNELS[i];
            if let Ok(mut cfg) = state.shared.config.write() {
                let was_visible = cfg.visible_channels.iter().any(|c| c == name);
                if was_visible {
                    if cfg.visible_channels.len() <= 1 {
                        drop(cfg);
                        let _ = InvalidateRect(hwnd, None, false);
                        return;
                    }
                    cfg.visible_channels.retain(|c| c != name);
                    // The startup channel must remain visible — if the
                    // user hid it, pick the first remaining as the new
                    // default.
                    if cfg.default_channel == name {
                        if let Some(first) = cfg.visible_channels.first().cloned() {
                            cfg.default_channel = first;
                        }
                    }
                } else {
                    insert_visible_canonical(&mut cfg.visible_channels, name);
                }
                let snapshot = cfg.clone();
                drop(cfg);
                config::save(&snapshot);
            }
            let _ = InvalidateRect(hwnd, None, false);
        }
        Action::RecordHotkey => {
            if state.recording {
                stop_recording(state);
            } else {
                state.recording = true;
                crate::tray::pause_hotkey();
            }
            let _ = SetFocus(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
        }
        Action::ToggleAutostart => {
            let want = !state.autostart;
            let result = if want {
                crate::autostart::enable()
            } else {
                crate::autostart::disable()
            };
            if result.is_ok() {
                state.autostart = want;
            }
            let _ = InvalidateRect(hwnd, None, false);
        }
        Action::ReinstallHook => {
            crate::hook::request_rehook();
        }
        Action::OpenLogFolder => {
            if let Some(path) = crate::log::log_path() {
                if let Some(parent) = path.parent() {
                    let _ = std::process::Command::new("explorer.exe")
                        .arg(parent)
                        .spawn();
                }
            }
        }
        Action::OpenConfigFolder => {
            if let Some(path) = config::path() {
                if let Some(parent) = path.parent() {
                    let _ = std::process::Command::new("explorer.exe")
                        .arg(parent)
                        .spawn();
                }
            }
        }
        Action::OpenHelp => {
            state.show_help = true;
            state.hovered = None;
            let _ = InvalidateRect(hwnd, None, false);
        }
        Action::CloseHelp => {
            state.show_help = false;
            state.hovered = None;
            let _ = InvalidateRect(hwnd, None, false);
        }
        Action::SetHelpLang(lang) => {
            state.help_lang = lang;
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn insert_visible_canonical(visible: &mut Vec<String>, name: &str) {
    let pos = ALL_CHANNELS.iter().position(|c| *c == name).unwrap_or(0);
    let insert_at = visible
        .iter()
        .position(|c| ALL_CHANNELS.iter().position(|x| x == c).unwrap_or(0) > pos)
        .unwrap_or(visible.len());
    visible.insert(insert_at, name.to_string());
}

// ---------------------------------------------------------------- paint

unsafe fn paint(hwnd: HWND, state: &mut WindowState) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let w = client.right - client.left;
    let h = client.bottom - client.top;

    let mem_dc = CreateCompatibleDC(hdc);
    let mem_bmp = CreateCompatibleBitmap(hdc, w, h);
    let old_bmp = SelectObject(mem_dc, mem_bmp);

    let bg = CreateSolidBrush(COLORREF(BG));
    FillRect(mem_dc, &client, bg);
    let _ = DeleteObject(bg);

    SetBkMode(mem_dc, TRANSPARENT);

    state.regions.clear();
    let fonts = Fonts::make();
    if state.show_help {
        paint_help(mem_dc, &fonts, state, &client);
    } else {
        paint_settings(mem_dc, &fonts, state, &client);
    }
    fonts.delete();

    let _ = BitBlt(hdc, 0, 0, w, h, mem_dc, 0, 0, SRCCOPY);

    SelectObject(mem_dc, old_bmp);
    let _ = DeleteObject(mem_bmp);
    let _ = DeleteDC(mem_dc);
    let _ = EndPaint(hwnd, &ps);
}

// --------------------------------------------------------- settings view

unsafe fn paint_settings(hdc: HDC, fonts: &Fonts, state: &mut WindowState, client: &RECT) {
    let snapshot = read_snapshot(state);
    let mut y = PADDING;
    y = paint_channels_section(hdc, fonts, state, client, y, &snapshot);
    y = paint_hotkey_section(hdc, fonts, state, client, y, &snapshot.hotkey);
    y = paint_autostart_section(hdc, fonts, state, client, y);
    paint_action_buttons(hdc, fonts, state, client, y);
}

struct Snapshot {
    default_name: String,
    visible: Vec<String>,
    hotkey: String,
    active_idx: usize,
}

fn read_snapshot(state: &WindowState) -> Snapshot {
    let active_idx = state.shared.active_channel.load(Ordering::Acquire);
    let guard = state
        .shared
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner());
    Snapshot {
        default_name: guard.default_channel.clone(),
        visible: guard.visible_channels.clone(),
        hotkey: guard.cycle_hotkey.clone().unwrap_or_default(),
        active_idx,
    }
}

// ----- Channels section -------------------------------------------------

unsafe fn paint_channels_section(
    hdc: HDC,
    fonts: &Fonts,
    state: &mut WindowState,
    client: &RECT,
    y_start: i32,
    snapshot: &Snapshot,
) -> i32 {
    let mut y = y_start;
    draw_header(hdc, fonts.header, "CHANNELS", PADDING, y);

    let help_rect = RECT {
        left: client.right - PADDING - HELP_BTN,
        top: y - 4,
        right: client.right - PADDING,
        bottom: y - 4 + HELP_BTN,
    };
    let hov_idx = state.regions.len();
    let hovered = state.hovered == Some(hov_idx);
    draw_help_button(hdc, fonts.strong, &help_rect, hovered);
    state.regions.push(HitRegion {
        rect: help_rect,
        action: Action::OpenHelp,
    });
    y += HEADER_H + 10;

    let panel_rect = RECT {
        left: PADDING,
        top: y,
        right: client.right - PADDING,
        bottom: y + ROW_H * ALL_CHANNELS.len() as i32,
    };
    fill_round(hdc, &panel_rect, CARD, CARD, CARD_RADIUS);

    for (i, &channel) in ALL_CHANNELS.iter().enumerate() {
        let row = RECT {
            left: PADDING,
            top: y,
            right: client.right - PADDING,
            bottom: y + ROW_H,
        };
        let visible = snapshot.visible.iter().any(|c| c == channel);
        let is_default = channel == snapshot.default_name.as_str();
        let is_active = i == snapshot.active_idx;
        paint_channel_row(
            hdc,
            fonts,
            &mut state.regions,
            state.hovered,
            &row,
            i,
            channel,
            visible,
            is_default,
            is_active,
        );
        if i + 1 < ALL_CHANNELS.len() {
            let divider = RECT {
                left: row.left + 14,
                top: row.bottom - 1,
                right: row.right - 14,
                bottom: row.bottom,
            };
            let brush = CreateSolidBrush(COLORREF(DIVIDER));
            FillRect(hdc, &divider, brush);
            let _ = DeleteObject(brush);
        }
        y += ROW_H;
    }

    y
}

#[allow(clippy::too_many_arguments)]
unsafe fn paint_channel_row(
    hdc: HDC,
    fonts: &Fonts,
    regions: &mut Vec<HitRegion>,
    hovered: Option<usize>,
    row: &RECT,
    index: usize,
    name: &str,
    visible: bool,
    is_default: bool,
    is_active: bool,
) {
    // Heavy visual treatment is reserved for the *active* channel — the
    // one the wheel is moving right now. The favourite gets just the
    // filled star marker. They can coincide; when they do, both are
    // applied.
    if is_active {
        let bg = CreateSolidBrush(COLORREF(CARD_ACTIVE));
        FillRect(hdc, row, bg);
        let _ = DeleteObject(bg);

        let stripe = RECT {
            left: row.left,
            top: row.top + 6,
            right: row.left + 4,
            bottom: row.bottom - 6,
        };
        let brush = CreateSolidBrush(COLORREF(ACCENT));
        FillRect(hdc, &stripe, brush);
        let _ = DeleteObject(brush);
    }

    // Star — startup-channel marker. Click anywhere inside the icon
    // rect to mark this channel as the one the app launches into.
    let star_top = row.top + (ROW_H - ICON_BTN) / 2;
    let star_left = row.left + 14;
    let star_rect = RECT {
        left: star_left,
        top: star_top,
        right: star_left + ICON_BTN,
        bottom: star_top + ICON_BTN,
    };
    let star_idx = regions.len();
    let star_hov = hovered == Some(star_idx);
    draw_star(hdc, fonts.icon, &star_rect, is_default, visible, star_hov);
    regions.push(HitRegion {
        rect: star_rect,
        action: Action::SetFavorite(index),
    });

    // Eye — visibility toggle on the right. Open eye = visible, eye
    // with slash = hidden.
    let eye_top = row.top + (ROW_H - ICON_BTN) / 2;
    let eye_left = row.right - ICON_BTN - 14;
    let eye_rect = RECT {
        left: eye_left,
        top: eye_top,
        right: eye_left + ICON_BTN,
        bottom: eye_top + ICON_BTN,
    };
    let eye_idx = regions.len();
    let eye_hov = hovered == Some(eye_idx);
    draw_eye(hdc, fonts.icon, &eye_rect, visible, eye_hov);
    regions.push(HitRegion {
        rect: eye_rect,
        action: Action::ToggleVisible(index),
    });

    // Channel name spans the middle of the row and is itself the
    // "make this channel active" click target. Three-tier emphasis:
    //   active  → strong-weight, full-bright
    //   visible → regular,        dim
    //   hidden  → regular,        faint
    let name_hit = RECT {
        left: star_rect.right + 6,
        top: row.top + 2,
        right: eye_rect.left - 6,
        bottom: row.bottom - 2,
    };
    let name_idx = regions.len();
    let name_hov = hovered == Some(name_idx);
    let (text_color, name_font) = if is_active {
        (TEXT, fonts.name_strong)
    } else if visible {
        let c = if name_hov { TEXT } else { TEXT_DIM };
        (c, fonts.name)
    } else {
        let c = if name_hov { TEXT_DIM } else { TEXT_FAINT };
        (c, fonts.name)
    };
    let old_font = SelectObject(hdc, name_font);
    SetTextColor(hdc, COLORREF(text_color));
    let mut text: Vec<u16> = name.encode_utf16().collect();
    let mut name_draw = RECT {
        left: star_rect.right + 12,
        top: row.top,
        right: eye_rect.left - 12,
        bottom: row.bottom,
    };
    DrawTextW(
        hdc,
        &mut text,
        &mut name_draw,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(hdc, old_font);
    regions.push(HitRegion {
        rect: name_hit,
        action: Action::SetActive(index),
    });
}

// ----- Hotkey section ---------------------------------------------------

unsafe fn paint_hotkey_section(
    hdc: HDC,
    fonts: &Fonts,
    state: &mut WindowState,
    client: &RECT,
    y_start: i32,
    hotkey_spec: &str,
) -> i32 {
    let mut y = y_start + SECTION_GAP;
    draw_header(hdc, fonts.header, "CYCLE HOTKEY", PADDING, y);
    y += HEADER_H + 10;

    let box_h = 38;
    let hotkey_box = RECT {
        left: PADDING,
        top: y,
        right: client.right - PADDING,
        bottom: y + box_h,
    };
    let display = if state.recording {
        "Press a combination… (Esc to cancel)"
    } else if hotkey_spec.is_empty() {
        "Click to set a keyboard shortcut"
    } else {
        hotkey_spec
    };
    let idx = state.regions.len();
    let hovered = state.hovered == Some(idx);
    draw_hotkey_box(
        hdc,
        fonts.body,
        &hotkey_box,
        display,
        state.recording,
        hovered,
    );
    state.regions.push(HitRegion {
        rect: hotkey_box,
        action: Action::RecordHotkey,
    });
    y += box_h + 8;
    let hint = if state.recording {
        "Modifier + letter or F-key. Esc to cancel."
    } else {
        "Cycles the active channel to the next visible one — applied instantly."
    };
    draw_hint(hdc, fonts.body, hint, PADDING, y);
    y + 18
}

// ----- Autostart section ------------------------------------------------

unsafe fn paint_autostart_section(
    hdc: HDC,
    fonts: &Fonts,
    state: &mut WindowState,
    client: &RECT,
    y_start: i32,
) -> i32 {
    let row_h = 38;
    let y = y_start + SECTION_GAP - 2;
    let rect = RECT {
        left: PADDING,
        top: y,
        right: client.right - PADDING,
        bottom: y + row_h,
    };
    let idx = state.regions.len();
    let hovered = state.hovered == Some(idx);
    draw_switch_row(
        hdc,
        fonts.body,
        &rect,
        "Start with Windows",
        state.autostart,
        hovered,
    );
    state.regions.push(HitRegion {
        rect,
        action: Action::ToggleAutostart,
    });
    y + row_h
}

// ----- Action buttons ---------------------------------------------------

unsafe fn paint_action_buttons(
    hdc: HDC,
    fonts: &Fonts,
    state: &mut WindowState,
    client: &RECT,
    y_start: i32,
) {
    let btn_h = 38;
    let mut y = y_start + SECTION_GAP;

    let reinstall = RECT {
        left: PADDING,
        top: y,
        right: client.right - PADDING,
        bottom: y + btn_h,
    };
    let idx = state.regions.len();
    let hovered = state.hovered == Some(idx);
    draw_pill_button(
        hdc,
        fonts.strong,
        &reinstall,
        "Reinstall keyboard hook",
        hovered,
        false,
    );
    state.regions.push(HitRegion {
        rect: reinstall,
        action: Action::ReinstallHook,
    });
    y += btn_h + 8;

    let btn_w = (client.right - PADDING * 2 - 10) / 2;
    let logs = RECT {
        left: PADDING,
        top: y,
        right: PADDING + btn_w,
        bottom: y + btn_h,
    };
    let idx = state.regions.len();
    let hovered = state.hovered == Some(idx);
    draw_pill_button(hdc, fonts.strong, &logs, "Open log folder", hovered, false);
    state.regions.push(HitRegion {
        rect: logs,
        action: Action::OpenLogFolder,
    });

    let cfg_btn = RECT {
        left: logs.right + 10,
        top: y,
        right: logs.right + 10 + btn_w,
        bottom: y + btn_h,
    };
    let idx = state.regions.len();
    let hovered = state.hovered == Some(idx);
    draw_pill_button(
        hdc,
        fonts.strong,
        &cfg_btn,
        "Open config folder",
        hovered,
        false,
    );
    state.regions.push(HitRegion {
        rect: cfg_btn,
        action: Action::OpenConfigFolder,
    });
}

// ------------------------------------------------------------- primitives

unsafe fn draw_star(
    hdc: HDC,
    font: HFONT,
    rect: &RECT,
    is_favorite: bool,
    visible: bool,
    hovered: bool,
) {
    if hovered && !is_favorite {
        fill_round(hdc, rect, CARD_HOVER, BORDER, PILL_RADIUS);
    }
    // Segoe MDL2 Assets — FavoriteStarFill (E735) / FavoriteStar (E734).
    let (glyph, color) = if is_favorite {
        ("\u{E735}", ACCENT)
    } else if visible {
        ("\u{E734}", if hovered { TEXT } else { TEXT_DIM })
    } else {
        ("\u{E734}", TEXT_FAINT)
    };
    draw_icon_glyph(hdc, font, rect, glyph, color);
}

/// Eye icon. Same Segoe MDL2 Assets font as the star so every row
/// glyph shares one vector pipeline — no more hand-rolled ellipses
/// sitting next to real icons.
unsafe fn draw_eye(hdc: HDC, font: HFONT, rect: &RECT, visible: bool, hovered: bool) {
    if hovered {
        fill_round(hdc, rect, CARD_HOVER, BORDER, PILL_RADIUS);
    }
    // RedEye (E7B3) for visible, Hide (ED1A — eye with a slash) for
    // hidden. Both ship in every Win10/11 build.
    let (glyph, color) = if visible {
        ("\u{E7B3}", if hovered { TEXT } else { TEXT_DIM })
    } else {
        ("\u{ED1A}", if hovered { TEXT_DIM } else { TEXT_FAINT })
    };
    draw_icon_glyph(hdc, font, rect, glyph, color);
}

unsafe fn draw_icon_glyph(hdc: HDC, font: HFONT, rect: &RECT, glyph: &str, color: u32) {
    let old = SelectObject(hdc, font);
    SetTextColor(hdc, COLORREF(color));
    let mut t: Vec<u16> = glyph.encode_utf16().collect();
    let mut r = *rect;
    DrawTextW(hdc, &mut t, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old);
}

unsafe fn draw_help_button(hdc: HDC, font: HFONT, rect: &RECT, hovered: bool) {
    let border = if hovered { ACCENT } else { BORDER };
    let fill = if hovered { CARD_HOVER } else { BG };
    fill_round(hdc, rect, fill, border, HELP_BTN / 2);
    let old = SelectObject(hdc, font);
    SetTextColor(hdc, COLORREF(if hovered { ACCENT } else { TEXT_DIM }));
    let mut t: Vec<u16> = "?".encode_utf16().collect();
    let mut r = *rect;
    DrawTextW(hdc, &mut t, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old);
}

unsafe fn draw_header(hdc: HDC, font: HFONT, label: &str, x: i32, y: i32) {
    let old = SelectObject(hdc, font);
    SetTextColor(hdc, COLORREF(TEXT_DIM));
    let mut text: Vec<u16> = label.encode_utf16().collect();
    let mut r = RECT {
        left: x,
        top: y,
        right: x + 400,
        bottom: y + HEADER_H,
    };
    DrawTextW(hdc, &mut text, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old);
}

unsafe fn draw_hint(hdc: HDC, font: HFONT, label: &str, x: i32, y: i32) {
    let old = SelectObject(hdc, font);
    SetTextColor(hdc, COLORREF(TEXT_FAINT));
    let mut text: Vec<u16> = label.encode_utf16().collect();
    let mut r = RECT {
        left: x,
        top: y,
        right: x + 460,
        bottom: y + 18,
    };
    DrawTextW(hdc, &mut text, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old);
}

unsafe fn draw_hotkey_box(
    hdc: HDC,
    font: HFONT,
    rect: &RECT,
    text: &str,
    recording: bool,
    hovered: bool,
) {
    let fill = if recording { CARD_HOVER } else { CARD };
    let border = if recording {
        ACCENT
    } else if hovered {
        ACCENT_SOFT
    } else {
        BORDER
    };
    fill_round(hdc, rect, fill, border, PILL_RADIUS);

    let old_font = SelectObject(hdc, font);
    let placeholder = !recording && text.starts_with("Click");
    SetTextColor(
        hdc,
        COLORREF(if recording {
            ACCENT
        } else if placeholder {
            TEXT_FAINT
        } else {
            TEXT
        }),
    );
    let mut t: Vec<u16> = text.encode_utf16().collect();
    let mut r = RECT {
        left: rect.left + 16,
        top: rect.top,
        right: rect.right - 16,
        bottom: rect.bottom,
    };
    DrawTextW(hdc, &mut t, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old_font);
}

unsafe fn draw_pill_button(
    hdc: HDC,
    font: HFONT,
    rect: &RECT,
    label: &str,
    hovered: bool,
    accent: bool,
) {
    let fill = if accent {
        if hovered {
            ACCENT_HOVER
        } else {
            ACCENT_SOFT
        }
    } else if hovered {
        CARD_HOVER
    } else {
        CARD
    };
    let border = if accent {
        fill
    } else if hovered {
        ACCENT_SOFT
    } else {
        BORDER
    };
    fill_round(hdc, rect, fill, border, PILL_RADIUS);
    let old = SelectObject(hdc, font);
    SetTextColor(hdc, COLORREF(TEXT));
    let mut t: Vec<u16> = label.encode_utf16().collect();
    let mut r = *rect;
    DrawTextW(hdc, &mut t, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old);
}

unsafe fn draw_switch_row(
    hdc: HDC,
    font: HFONT,
    rect: &RECT,
    label: &str,
    checked: bool,
    hovered: bool,
) {
    let old_font = SelectObject(hdc, font);
    SetTextColor(hdc, COLORREF(TEXT));
    let mut t: Vec<u16> = label.encode_utf16().collect();
    let mut r = RECT {
        left: rect.left + 4,
        top: rect.top,
        right: rect.right - SWITCH_W - 12,
        bottom: rect.bottom,
    };
    DrawTextW(hdc, &mut t, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old_font);

    let sx = rect.right - SWITCH_W - 4;
    let sy = rect.top + (rect.bottom - rect.top - SWITCH_H) / 2;
    let switch_rect = RECT {
        left: sx,
        top: sy,
        right: sx + SWITCH_W,
        bottom: sy + SWITCH_H,
    };
    draw_switch(hdc, &switch_rect, checked, hovered);
}

unsafe fn draw_switch(hdc: HDC, rect: &RECT, on: bool, hovered: bool) {
    let track_color = if on {
        if hovered {
            ACCENT_HOVER
        } else {
            ACCENT
        }
    } else if hovered {
        CARD_HOVER
    } else {
        BORDER
    };
    let h = rect.bottom - rect.top;
    fill_round_simple(
        hdc,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        track_color,
    );

    let knob = h - 6;
    let kx = if on {
        rect.right - knob - 3
    } else {
        rect.left + 3
    };
    let ky = rect.top + (h - knob) / 2;
    let knob_color = if on {
        TEXT
    } else if hovered {
        TEXT_DIM
    } else {
        TEXT_FAINT
    };
    let knob_brush = CreateSolidBrush(COLORREF(knob_color));
    let knob_pen = CreatePen(PS_SOLID, 1, COLORREF(knob_color));
    let old_brush = SelectObject(hdc, knob_brush);
    let old_pen = SelectObject(hdc, knob_pen);
    let _ = Ellipse(hdc, kx, ky, kx + knob, ky + knob);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(knob_brush);
    let _ = DeleteObject(knob_pen);
}

unsafe fn fill_round(hdc: HDC, rect: &RECT, fill: u32, border: u32, radius: i32) {
    let brush = CreateSolidBrush(COLORREF(fill));
    let pen = CreatePen(PS_SOLID, 1, COLORREF(border));
    let old_brush = SelectObject(hdc, brush);
    let old_pen = SelectObject(hdc, pen);
    let _ = RoundRect(
        hdc,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        radius * 2,
        radius * 2,
    );
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);
}

unsafe fn fill_round_simple(hdc: HDC, l: i32, t: i32, r: i32, b: i32, fill: u32) {
    let brush = CreateSolidBrush(COLORREF(fill));
    let pen = CreatePen(PS_SOLID, 1, COLORREF(fill));
    let old_brush = SelectObject(hdc, brush);
    let old_pen = SelectObject(hdc, pen);
    let h = (b - t).abs();
    let _ = RoundRect(hdc, l, t, r, b, h, h);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);
}

unsafe fn make_font(height: i32, weight: i32, face: PCWSTR) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_OUTLINE_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        0,
        face,
    )
}

// ------------------------------------------------------------- help view
//
// Help is rendered as a stack of self-contained "tip cards" — icon on
// the left, heading + one-line body on the right — rather than a single
// wall of bullet-text. Cards mirror the affordances the user actually
// touches in the settings window (star, eye, hotkey box, tray icon), so
// the reference card visually maps to the thing it describes.

struct HelpCard {
    icon: &'static str,
    heading_en: &'static str,
    heading_fr: &'static str,
    body_en: &'static str,
    body_fr: &'static str,
}

const HELP_CARDS: &[HelpCard] = &[
    HelpCard {
        icon: "\u{E767}", // Volume
        heading_en: "ACTIVE CHANNEL",
        heading_fr: "CHANNEL ACTIF",
        body_en: "Click a channel name to control it now. The highlighted row is what your volume keys move.",
        body_fr: "Clique sur le nom d'un channel pour le contrôler maintenant. La ligne en surbrillance est celle que les touches volume bougent.",
    },
    HelpCard {
        icon: "\u{E735}", // FavoriteStarFill
        heading_en: "STARTUP CHANNEL",
        heading_fr: "CHANNEL DE DÉMARRAGE",
        body_en: "Tap ★ to bookmark a channel as the one the app reopens with at boot. Independent from the active selection.",
        body_fr: "Touche ★ pour qu'un channel devienne celui sur lequel l'app revient au prochain démarrage. Indépendant du channel actif.",
    },
    HelpCard {
        icon: "\u{E7B3}", // RedEye
        heading_en: "VISIBILITY",
        heading_fr: "VISIBILITÉ",
        body_en: "The eye toggles a channel in or out of the tray right-click menu and the cycle hotkey rotation.",
        body_fr: "L'œil ajoute ou retire un channel du menu clic-droit du tray et de la rotation du hotkey.",
    },
    HelpCard {
        icon: "\u{E144}", // KeyboardClassic
        heading_en: "CYCLE HOTKEY",
        heading_fr: "HOTKEY DE CYCLE",
        body_en: "A global shortcut that jumps the active channel to the next visible one. Click the box below to record a new combo.",
        body_fr: "Un raccourci global qui passe le channel actif au visible suivant. Clic dans la boîte ci-dessous pour en enregistrer un nouveau.",
    },
    HelpCard {
        icon: "\u{E700}", // GlobalNavButton — represents the tray menu
        heading_en: "TRAY ICON",
        heading_fr: "ICÔNE TRAY",
        body_en: "Left-click opens this window. Right-click brings up the quick switcher with only the visible channels.",
        body_fr: "Clic gauche ouvre cette fenêtre. Clic droit affiche le sélecteur rapide avec uniquement les channels visibles.",
    },
];

const HELP_CARD_H: i32 = 76;
const HELP_CARD_GAP: i32 = 8;

unsafe fn paint_help(hdc: HDC, fonts: &Fonts, state: &mut WindowState, client: &RECT) {
    let mut y = PADDING;

    // -- Title row --------------------------------------------------
    let title = if state.help_lang == HelpLang::Fr {
        "Aide"
    } else {
        "Help"
    };
    let old_font = SelectObject(hdc, fonts.title);
    SetTextColor(hdc, COLORREF(TEXT));
    let mut t: Vec<u16> = title.encode_utf16().collect();
    let mut r = RECT {
        left: PADDING,
        top: y,
        right: PADDING + 240,
        bottom: y + 36,
    };
    DrawTextW(hdc, &mut t, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old_font);

    let tab_w = 42;
    let tab_h = 26;
    let tab_y = y + (36 - tab_h) / 2;
    let fr_rect = RECT {
        left: client.right - PADDING - tab_w,
        top: tab_y,
        right: client.right - PADDING,
        bottom: tab_y + tab_h,
    };
    let en_rect = RECT {
        left: fr_rect.left - 6 - tab_w,
        top: tab_y,
        right: fr_rect.left - 6,
        bottom: tab_y + tab_h,
    };
    let en_idx = state.regions.len();
    let en_hov = state.hovered == Some(en_idx);
    draw_language_tab(
        hdc,
        fonts.strong,
        &en_rect,
        "EN",
        state.help_lang == HelpLang::En,
        en_hov,
    );
    state.regions.push(HitRegion {
        rect: en_rect,
        action: Action::SetHelpLang(HelpLang::En),
    });
    let fr_idx = state.regions.len();
    let fr_hov = state.hovered == Some(fr_idx);
    draw_language_tab(
        hdc,
        fonts.strong,
        &fr_rect,
        "FR",
        state.help_lang == HelpLang::Fr,
        fr_hov,
    );
    state.regions.push(HitRegion {
        rect: fr_rect,
        action: Action::SetHelpLang(HelpLang::Fr),
    });
    y += 36 + 14;

    // -- Tagline ----------------------------------------------------
    let tagline = if state.help_lang == HelpLang::Fr {
        "Tes touches volume connectées aux faders GoXLR."
    } else {
        "Your volume keys, wired to a GoXLR fader."
    };
    let old_font = SelectObject(hdc, fonts.body);
    SetTextColor(hdc, COLORREF(TEXT_DIM));
    let mut t: Vec<u16> = tagline.encode_utf16().collect();
    let mut r = RECT {
        left: PADDING,
        top: y,
        right: client.right - PADDING,
        bottom: y + 22,
    };
    DrawTextW(hdc, &mut t, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old_font);
    y += 22 + 14;

    // -- Tip cards --------------------------------------------------
    for card in HELP_CARDS {
        let card_rect = RECT {
            left: PADDING,
            top: y,
            right: client.right - PADDING,
            bottom: y + HELP_CARD_H,
        };
        paint_help_card(hdc, fonts, &card_rect, card, state.help_lang);
        y += HELP_CARD_H + HELP_CARD_GAP;
    }

    // -- Back button (anchored to the bottom) -----------------------
    let back_rect = RECT {
        left: PADDING,
        top: client.bottom - PADDING - ROW_H,
        right: client.right - PADDING,
        bottom: client.bottom - PADDING,
    };
    let back_idx = state.regions.len();
    let back_hov = state.hovered == Some(back_idx);
    let back_label = if state.help_lang == HelpLang::Fr {
        "Retour aux réglages"
    } else {
        "Back to settings"
    };
    draw_pill_button(hdc, fonts.strong, &back_rect, back_label, back_hov, true);
    state.regions.push(HitRegion {
        rect: back_rect,
        action: Action::CloseHelp,
    });
}

unsafe fn paint_help_card(hdc: HDC, fonts: &Fonts, rect: &RECT, card: &HelpCard, lang: HelpLang) {
    // Card surface — same elevation as the channel list card, no
    // visible border (the bg contrast does the work).
    fill_round(hdc, rect, CARD, CARD, CARD_RADIUS);

    // Icon column on the left. The accent colour makes it the first
    // thing the eye lands on; consistent across cards so the rhythm
    // reads as "category, content".
    let icon_box = RECT {
        left: rect.left + 8,
        top: rect.top,
        right: rect.left + 8 + 44,
        bottom: rect.bottom,
    };
    let old = SelectObject(hdc, fonts.icon_large);
    SetTextColor(hdc, COLORREF(ACCENT));
    let mut g: Vec<u16> = card.icon.encode_utf16().collect();
    let mut ir = icon_box;
    DrawTextW(hdc, &mut g, &mut ir, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old);

    // Heading — uppercase, semibold, dim accent colour so it reads as
    // a label, not a paragraph header.
    let heading = if lang == HelpLang::Fr {
        card.heading_fr
    } else {
        card.heading_en
    };
    let text_left = icon_box.right + 8;
    let text_right = rect.right - 16;
    let head_old = SelectObject(hdc, fonts.header);
    SetTextColor(hdc, COLORREF(ACCENT));
    let mut h: Vec<u16> = heading.encode_utf16().collect();
    let mut hr = RECT {
        left: text_left,
        top: rect.top + 12,
        right: text_right,
        bottom: rect.top + 30,
    };
    DrawTextW(hdc, &mut h, &mut hr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, head_old);

    // Body — word-wrapped, mid-grey, regular weight.
    let body = if lang == HelpLang::Fr {
        card.body_fr
    } else {
        card.body_en
    };
    let body_old = SelectObject(hdc, fonts.body);
    SetTextColor(hdc, COLORREF(TEXT));
    let mut b: Vec<u16> = body.encode_utf16().collect();
    let mut br = RECT {
        left: text_left,
        top: rect.top + 32,
        right: text_right,
        bottom: rect.bottom - 8,
    };
    DrawTextW(hdc, &mut b, &mut br, DT_LEFT | DT_WORDBREAK);
    SelectObject(hdc, body_old);
}

unsafe fn draw_language_tab(
    hdc: HDC,
    font: HFONT,
    rect: &RECT,
    label: &str,
    active: bool,
    hovered: bool,
) {
    let (fill, border, text_color) = if active {
        let f = if hovered { ACCENT_HOVER } else { ACCENT_SOFT };
        (f, f, TEXT)
    } else {
        let f = if hovered { CARD_HOVER } else { CARD };
        (f, BORDER, TEXT_DIM)
    };
    fill_round(hdc, rect, fill, border, PILL_RADIUS);
    let old = SelectObject(hdc, font);
    SetTextColor(hdc, COLORREF(text_color));
    let mut t: Vec<u16> = label.encode_utf16().collect();
    let mut r = *rect;
    DrawTextW(hdc, &mut t, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old);
}

// ----------------------------------------------------------- hotkey UX

fn is_modifier(vk: u32) -> bool {
    matches!(
        vk,
        0x10 | 0x11 | 0x12 | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5 | 0x5B | 0x5C
    )
}

unsafe fn build_hotkey_spec(vk: u32) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if GetKeyState(VK_CONTROL.0 as i32) < 0 {
        parts.push("ctrl");
    }
    if GetKeyState(VK_SHIFT.0 as i32) < 0 {
        parts.push("shift");
    }
    if GetKeyState(VK_MENU.0 as i32) < 0 {
        parts.push("alt");
    }
    if GetKeyState(VK_LWIN.0 as i32) < 0 || GetKeyState(VK_RWIN.0 as i32) < 0 {
        parts.push("win");
    }
    if parts.is_empty() {
        return None;
    }
    let key = vk_to_string(vk)?;
    let mut spec = parts.join("+");
    spec.push('+');
    spec.push_str(&key);
    Some(spec)
}

fn vk_to_string(vk: u32) -> Option<String> {
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
        return Some((vk as u8 as char).to_ascii_lowercase().to_string());
    }
    if (0x70..=0x87).contains(&vk) {
        return Some(format!("f{}", vk - 0x70 + 1));
    }
    None
}

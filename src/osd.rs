use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::w;
use windows::Win32::Foundation::{
    BOOL, COLORREF, HMODULE, HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, InvalidateRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DT_LEFT, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, FW_SEMIBOLD, OUT_OUTLINE_PRECIS, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows, GetClassNameW, GetMessageW,
    GetSystemMetrics, GetWindowLongW, GetWindowRect, KillTimer, PostThreadMessageW,
    RegisterClassExW, SetLayeredWindowAttributes, SetTimer, SetWindowLongW, SetWindowPos,
    ShowWindow, TranslateMessage, EVENT_OBJECT_SHOW, GWL_EXSTYLE, LWA_ALPHA, MSG, OBJID_WINDOW,
    SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOWNA,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_PAINT, WM_TIMER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

/// COLORREF is BBGGRR. GoXLR orange #FB9C33 → 0x00339CFB.
const COLOR_BG: u32 = 0x001F1F1F;
const COLOR_TRACK: u32 = 0x003C3C3C;
const COLOR_FILL: u32 = 0x00339CFB;
const COLOR_TEXT: u32 = 0x00FFFFFF;

const OSD_WIDTH: i32 = 380;
const OSD_HEIGHT: i32 = 64;
const OSD_RADIUS: i32 = 14;
const PADDING: i32 = 18;
const NAME_WIDTH: i32 = 110;
const NUMBER_WIDTH: i32 = 48;
const BAR_HEIGHT: i32 = 6;

const HIDE_TIMER_ID: usize = 1;
const HIDE_TIMER_MS: u32 = 1500;
/// Periodic safety-net sweep for Windows's native OSD. The accessibility
/// hook catches the moment a candidate is shown, but we also re-sweep at
/// 1Hz to cover hooks that registered after the OSD already existed and
/// to re-apply suppression if the system uncloaks the window.
const SUPPRESS_TIMER_ID: usize = 2;
const SUPPRESS_TIMER_MS: u32 = 1000;
const WM_OSD_UPDATE: u32 = WM_APP + 10;

#[derive(Clone)]
struct OsdState {
    channel: String,
    value: i32,
    max: i32,
}

static STATE: OnceLock<Mutex<OsdState>> = OnceLock::new();
static OSD_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// Spawns the dedicated OSD thread. Owns its own Win32 message pump and
/// a borderless topmost window that paints whenever a volume update lands.
pub fn start() {
    let _ = STATE.set(Mutex::new(OsdState {
        channel: String::new(),
        value: 0,
        max: 255,
    }));
    std::thread::Builder::new()
        .name("osd".into())
        .spawn(thread_main)
        .expect("spawn osd thread");
}

/// Updates the OSD state and pokes its thread to redraw + reset the
/// auto-hide timer. Safe to call from any thread, never blocks.
pub fn show(channel: &str, value: i32, max: i32) {
    let Some(state) = STATE.get() else {
        return;
    };
    if let Ok(mut s) = state.lock() {
        s.channel.clear();
        s.channel.push_str(channel);
        s.value = value;
        s.max = max;
    }
    let tid = OSD_THREAD_ID.load(Ordering::Acquire);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_OSD_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

fn thread_main() {
    unsafe {
        OSD_THREAD_ID.store(GetCurrentThreadId(), Ordering::Release);

        let hmodule = GetModuleHandleW(None).expect("GetModuleHandleW failed");
        let class_name = w!("GoXLRVolumeWheelOSD");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hmodule.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let (x, y) = position();
        let hwnd = CreateWindowExW(
            // TOPMOST so we cover Windows's own OSD when both are at the
            // same screen location, TOOLWINDOW so we never appear in
            // alt-tab, NOACTIVATE so click-events don't steal focus.
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            w!(""),
            WS_POPUP,
            x,
            y,
            OSD_WIDTH,
            OSD_HEIGHT,
            None,
            None,
            hmodule,
            None,
        )
        .expect("CreateWindowExW failed");

        // Rounded rect region clips the painted area to a pill-shaped
        // outline matching the Windows volume OSD silhouette.
        let rgn = CreateRoundRectRgn(
            0,
            0,
            OSD_WIDTH + 1,
            OSD_HEIGHT + 1,
            OSD_RADIUS * 2,
            OSD_RADIUS * 2,
        );
        SetWindowRgn(hwnd, rgn, false);

        // System-wide accessibility hook: fires the moment any window is
        // shown. The callback runs back on this thread because we asked
        // for WINEVENT_OUTOFCONTEXT — the OS marshals the call through
        // our message pump. This is what lets us neutralize the native
        // volume OSD before it can light up a single pixel, even on
        // Win11 24H2+ where the OSD is rendered by a XAML host that
        // ignores ShowWindow(SW_HIDE).
        let _ = SetWinEventHook(
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_SHOW,
            HMODULE::default(),
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );

        // Initial sweep — catch the OSD if it already exists from a prior
        // explorer/ShellHost session (our hook only sees future shows).
        let _ = EnumWindows(Some(enum_yeet_proc), LPARAM(0));
        SetTimer(hwnd, SUPPRESS_TIMER_ID, SUPPRESS_TIMER_MS, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            if msg.message == WM_OSD_UPDATE {
                // Re-sweep on every wheel turn — this is when Windows is
                // most likely to lazily create or unhide its OSD, so we
                // catch it just before our own window paints over it.
                let _ = EnumWindows(Some(enum_yeet_proc), LPARAM(0));
                let _ = ShowWindow(hwnd, SW_SHOWNA);
                let _ = InvalidateRect(hwnd, None, true);
                SetTimer(hwnd, HIDE_TIMER_ID, HIDE_TIMER_MS, None);
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn position() -> (i32, i32) {
    unsafe {
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let x = (screen_w - OSD_WIDTH) / 2;
        // Win11 puts the volume OSD at bottom-center; Win10 at top-center.
        // Mirror that so our window lands directly over Windows's and the
        // user sees only ours.
        let y = if is_win11() {
            screen_h - OSD_HEIGHT - 80
        } else {
            72
        };
        (x, y)
    }
}

fn is_win11() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        use winreg::enums::HKEY_LOCAL_MACHINE;
        use winreg::RegKey;
        let Ok(k) = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        else {
            return false;
        };
        let Ok(build) = k.get_value::<String, _>("CurrentBuild") else {
            return false;
        };
        build.parse::<u32>().map(|n| n >= 22000).unwrap_or(false)
    })
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_TIMER if w_param.0 == HIDE_TIMER_ID => {
            let _ = KillTimer(hwnd, HIDE_TIMER_ID);
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_TIMER if w_param.0 == SUPPRESS_TIMER_ID => {
            let _ = EnumWindows(Some(enum_yeet_proc), LPARAM(0));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

/// SetWinEventHook callback. Fires for every top-level window show event
/// system-wide, so the hot path needs to reject non-candidates fast and
/// without allocating.
unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if event != EVENT_OBJECT_SHOW {
        return;
    }
    if id_object != OBJID_WINDOW.0 {
        return;
    }
    if hwnd.0.is_null() {
        return;
    }
    try_yeet(hwnd);
}

unsafe extern "system" fn enum_yeet_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    try_yeet(hwnd);
    TRUE
}

/// Identifies the Windows volume OSD by class + size and applies the
/// invisibility triple-tap. No-op for windows that don't match.
unsafe fn try_yeet(hwnd: HWND) {
    let mut buf = [0u16; 128];
    let len = GetClassNameW(hwnd, &mut buf);
    if len <= 0 {
        return;
    }
    let class = String::from_utf16_lossy(&buf[..len as usize]);
    let class_str = class.as_str();

    // Allow-list of host classes Windows has used for its volume OSD:
    //   NativeHWNDHost                — Win10, early Win11
    //   XamlExplorerHostIslandWindow  — Win11 22H2+, the new XAML host
    //   Windows.UI.Core.CoreWindow    — UWP root, used by some shells
    if !matches!(
        class_str,
        "NativeHWNDHost" | "XamlExplorerHostIslandWindow" | "Windows.UI.Core.CoreWindow"
    ) {
        return;
    }

    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return;
    }
    let h = rect.bottom - rect.top;
    let w = rect.right - rect.left;

    // Telemetry — these classes are also used by other shell flyouts and
    // many UWP apps, so log every candidate to make heuristic tuning
    // possible across Windows builds. Lands in %LOCALAPPDATA%\GoXLR
    // Volume Wheel\app.log.
    crate::log::info(&format!(
        "[osd-probe] class={} size={}x{} pos=({},{})",
        class_str, w, h, rect.left, rect.top
    ));

    // Volume OSD heuristic: short pill, not full-width. The 300px ceiling
    // is loose enough to absorb DWM shadows but tight enough to spare
    // toasts, action centers and full app windows.
    if h <= 20 || h >= 300 {
        return;
    }
    if w <= 100 || w >= 800 {
        return;
    }

    apply_invisible(hwnd);
    crate::log::info(&format!("[osd-yeet] class={} {}x{}", class_str, w, h));
}

/// Triple-tap suppression. Each layer survives whatever the next one
/// might bypass, so the OSD stays invisible even if the system fights
/// one of the mechanisms.
unsafe fn apply_invisible(hwnd: HWND) {
    // 1. DWM cloak — undocumented but stable since Win8. Removes the
    //    window from desktop composition entirely; survives ShowWindow
    //    calls, position changes, and topmost flags. This is the layer
    //    that XAML-hosted OSDs can't bypass — they render to a swapchain
    //    that DWM refuses to composite.
    let cloak: i32 = 1;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_CLOAK,
        &cloak as *const i32 as *const std::ffi::c_void,
        std::mem::size_of::<i32>() as u32,
    );

    // 2. Alpha 0 layered window — backstop if the system uncloaks the
    //    window. Persists for the window's lifetime.
    let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
    let layered = WS_EX_LAYERED.0 as i32;
    if (style & layered) == 0 {
        SetWindowLongW(hwnd, GWL_EXSTYLE, style | layered);
    }
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);

    // 3. Off-screen yeet — last-ditch fallback. Even if alpha is reset
    //    and the cloak removed, the window renders at (-32000, -32000)
    //    where no monitor can reach.
    let _ = SetWindowPos(
        hwnd,
        HWND::default(),
        -32000,
        -32000,
        0,
        0,
        SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
    );
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let Some(state_lock) = STATE.get() else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };
    let s = match state_lock.lock() {
        Ok(g) => g.clone(),
        Err(e) => e.into_inner().clone(),
    };

    // Background — clipped to the rounded region by SetWindowRgn.
    let bg = CreateSolidBrush(COLORREF(COLOR_BG));
    let bg_rect = RECT {
        left: 0,
        top: 0,
        right: OSD_WIDTH,
        bottom: OSD_HEIGHT,
    };
    FillRect(hdc, &bg_rect, bg);
    let _ = DeleteObject(bg);

    let font = CreateFontW(
        -16,
        0,
        0,
        0,
        FW_SEMIBOLD.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_OUTLINE_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        0, // FONT_PITCH(DEFAULT_PITCH) | FONT_FAMILY(FF_DONTCARE) = 0
        w!("Segoe UI"),
    );
    let old_font = SelectObject(hdc, font);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(COLOR_TEXT));

    // Channel name (left).
    let mut name: Vec<u16> = s.channel.encode_utf16().collect();
    let mut name_rect = RECT {
        left: PADDING,
        top: 0,
        right: PADDING + NAME_WIDTH,
        bottom: OSD_HEIGHT,
    };
    DrawTextW(
        hdc,
        &mut name,
        &mut name_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Percentage (right) — matches the 0–100 display style of Windows's
    // own OSD even though the GoXLR fader is 0–255 internally.
    let percent = if s.max > 0 { s.value * 100 / s.max } else { 0 };
    let mut num: Vec<u16> = format!("{}", percent).encode_utf16().collect();
    let mut num_rect = RECT {
        left: OSD_WIDTH - PADDING - NUMBER_WIDTH,
        top: 0,
        right: OSD_WIDTH - PADDING,
        bottom: OSD_HEIGHT,
    };
    DrawTextW(
        hdc,
        &mut num,
        &mut num_rect,
        DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
    );

    // Bar between name and number.
    let bar_x_start = PADDING + NAME_WIDTH + 12;
    let bar_x_end = OSD_WIDTH - PADDING - NUMBER_WIDTH - 12;
    let bar_y = (OSD_HEIGHT - BAR_HEIGHT) / 2;

    let track = CreateSolidBrush(COLORREF(COLOR_TRACK));
    let track_rect = RECT {
        left: bar_x_start,
        top: bar_y,
        right: bar_x_end,
        bottom: bar_y + BAR_HEIGHT,
    };
    FillRect(hdc, &track_rect, track);
    let _ = DeleteObject(track);

    let bar_w = bar_x_end - bar_x_start;
    let fill_w = if s.max > 0 {
        bar_w * s.value / s.max
    } else {
        0
    };
    if fill_w > 0 {
        let fill = CreateSolidBrush(COLORREF(COLOR_FILL));
        let fill_rect = RECT {
            left: bar_x_start,
            top: bar_y,
            right: bar_x_start + fill_w,
            bottom: bar_y + BAR_HEIGHT,
        };
        FillRect(hdc, &fill_rect, fill);
        let _ = DeleteObject(fill);
    }

    SelectObject(hdc, old_font);
    let _ = DeleteObject(font);
    let _ = EndPaint(hwnd, &ps);
}

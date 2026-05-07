use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::sync::mpsc::UnboundedSender;
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    PowerRegisterSuspendResumeNotification, PowerUnregisterSuspendResumeNotification, HPOWERNOTIFY,
};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_VOLUME_DOWN, VK_VOLUME_UP};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEHID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    PostThreadMessageW, RegisterClassExW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    HWND_MESSAGE, KBDLLHOOKSTRUCT, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
    REGISTER_NOTIFICATION_FLAGS, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_INPUT,
    WM_KEYDOWN, WM_KEYUP, WM_POWERBROADCAST, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_WTSSESSION_CHANGE,
    WNDCLASSEXW,
};

#[derive(Debug, Clone, Copy)]
pub enum VolumeEvent {
    Up,
    Down,
}

static SENDER: OnceLock<UnboundedSender<VolumeEvent>> = OnceLock::new();

/// Daemon connection state, read by the hook proc to decide whether to
/// swallow the event. While disconnected we hand the volume keys back to
/// Windows so the user retains a working master volume control instead of
/// being left with seemingly broken keys.
static CONNECTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Set by the notification window proc when the OS hits a state that may
/// have invalidated our hook (resume from suspend, session unlock, RDP /
/// console connect). The message pump observes it after each dispatch and
/// reinstalls the hook on the same thread.
static REHOOK_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Hook thread id, captured once `run_hook` starts. Used by `request_rehook`
/// to wake the hook thread from any other thread.
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// Posted to the hook thread to ask for a re-installation. Win32 reserves
/// `WM_APP..0xBFFF` for app-defined messages on a per-thread basis.
const WM_REQUEST_REHOOK: u32 = WM_APP + 1;

/// Tray menu trigger: posts WM_REQUEST_REHOOK to the hook thread so the
/// next pump iteration tears down and reinstalls the keyboard hook on the
/// thread that owns it.
pub fn request_rehook() {
    let tid = HOOK_THREAD_ID.load(Ordering::Acquire);
    if tid == 0 {
        return;
    }
    unsafe {
        let _ = PostThreadMessageW(tid, WM_REQUEST_REHOOK, WPARAM(0), LPARAM(0));
    }
}

/// Win32 wParam values for WM_WTSSESSION_CHANGE that mean "the input
/// desktop has changed under us, the hook may have been detached".
/// windows-rs 0.58 does not export them as constants — encoding the
/// values the docs guarantee.
const WTS_CONSOLE_CONNECT: u32 = 0x1;
const WTS_REMOTE_CONNECT: u32 = 0x6;
const WTS_SESSION_UNLOCK: u32 = 0x8;

const DEVICE_NOTIFY_WINDOW_HANDLE: REGISTER_NOTIFICATION_FLAGS = REGISTER_NOTIFICATION_FLAGS(0);

/// HID Usage Page 0x0C, Usage 0x01 = Consumer Control top-level. Wireless
/// headsets (Logitech G Pro X 2 confirmed) route their volume wheel through
/// vendor software that calls IAudioEndpointVolume directly and never emits
/// VK_VOLUME_UP / VK_VOLUME_DOWN, bypassing WH_KEYBOARD_LL. Subscribing to
/// raw HID on this usage lets us see the wheel ticks before they get
/// translated into endpoint-level changes. We can't swallow them — WM_INPUT
/// is informational only — so the OSD still flashes, but the GoXLR fader
/// follows along.
const HID_USAGE_PAGE_CONSUMER: u16 = 0x0C;
const HID_USAGE_CONSUMER_CONTROL: u16 = 0x01;

unsafe extern "system" fn keyboard_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        let kb = &*(l_param.0 as *const KBDLLHOOKSTRUCT);
        let msg = w_param.0 as u32;
        let vk = kb.vkCode as u16;

        if vk == VK_VOLUME_UP.0 || vk == VK_VOLUME_DOWN.0 {
            // While the daemon is unreachable, leave the keys to Windows so
            // the user keeps a usable master volume instead of dead keys.
            let live = CONNECTED
                .get()
                .map(|c| c.load(Ordering::Acquire))
                .unwrap_or(false);
            if !live {
                return CallNextHookEx(None, n_code, w_param, l_param);
            }

            if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                if let Some(tx) = SENDER.get() {
                    let event = if vk == VK_VOLUME_UP.0 {
                        VolumeEvent::Up
                    } else {
                        VolumeEvent::Down
                    };
                    let _ = tx.send(event);
                }
            }

            // Swallow the event entirely so Windows neither shows the OSD
            // nor changes the system master volume. Returning a non-zero
            // value without calling CallNextHookEx blocks the keystroke.
            if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN || msg == WM_KEYUP || msg == WM_SYSKEYUP {
                return LRESULT(1);
            }
        }
    }

    CallNextHookEx(None, n_code, w_param, l_param)
}

unsafe extern "system" fn notify_wndproc(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_INPUT => {
            handle_raw_input(HRAWINPUT(l_param.0 as *mut _));
        }
        WM_POWERBROADCAST => {
            if matches!(
                w_param.0 as u32,
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND
            ) {
                REHOOK_REQUESTED.store(true, Ordering::Release);
            }
        }
        WM_WTSSESSION_CHANGE => {
            if matches!(
                w_param.0 as u32,
                WTS_SESSION_UNLOCK | WTS_REMOTE_CONNECT | WTS_CONSOLE_CONNECT
            ) {
                REHOOK_REQUESTED.store(true, Ordering::Release);
            }
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, w_param, l_param)
}

/// Parses a WM_INPUT packet for HID Consumer Control usage and emits a
/// VolumeEvent when the payload matches the Logitech G-series report
/// layout: byte 0 is the report id (0x02), byte 1 is the action — 0x01
/// = volume up, 0x02 = volume down, 0x00 = release (ignored). The same
/// byte 0 is used by every Logitech audio device tested so far. Other
/// vendors with different report IDs land here too but their second byte
/// will not match 0x01 / 0x02 in this convention, so they are silently
/// dropped — no false positives, just no effect.
unsafe fn handle_raw_input(handle: HRAWINPUT) {
    let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut size: u32 = 0;
    // Don't compare against sizeof::<RAWINPUT>() — that's the Rust struct
    // sized to the largest union variant (RAWMOUSE, ~48 B). A real HID
    // packet ships at ~36 B, perfectly valid, and would be rejected. The
    // wire size returned here is the source of truth for what's available.
    if GetRawInputData(handle, RID_INPUT, None, &mut size, header_size) == u32::MAX || size == 0 {
        return;
    }

    let mut buffer = vec![0u8; size as usize];
    let read = GetRawInputData(
        handle,
        RID_INPUT,
        Some(buffer.as_mut_ptr() as *mut _),
        &mut size,
        header_size,
    );
    if read != size {
        return;
    }

    let raw = &*(buffer.as_ptr() as *const RAWINPUT);
    if raw.header.dwType != RIM_TYPEHID.0 {
        return;
    }

    let hid = &raw.data.hid;
    let payload_size = hid.dwSizeHid as usize;
    let payload_count = hid.dwCount as usize;
    if payload_size < 2 || payload_count == 0 {
        return;
    }

    let data_ptr = hid.bRawData.as_ptr();
    let total = payload_size * payload_count;
    let buffer_end = buffer.as_ptr().add(buffer.len());
    if data_ptr.add(total) > buffer_end {
        return;
    }

    let bytes = std::slice::from_raw_parts(data_ptr, total);
    let Some(tx) = SENDER.get() else { return };

    for i in 0..payload_count {
        let payload = &bytes[i * payload_size..(i + 1) * payload_size];
        if payload[0] != 0x02 {
            continue;
        }
        let event = match payload[1] {
            0x01 => VolumeEvent::Up,
            0x02 => VolumeEvent::Down,
            _ => continue, // 0x00 release, anything else
        };
        let _ = tx.send(event);
    }
}

pub fn run_hook(sender: UnboundedSender<VolumeEvent>, connected: Arc<AtomicBool>) {
    let _ = SENDER.set(sender);
    let _ = CONNECTED.set(connected);

    unsafe {
        HOOK_THREAD_ID.store(GetCurrentThreadId(), Ordering::Release);

        // LowLevelHooksTimeout (~300 ms by default on Win10/11) is per-call,
        // not an average — a single late return drops the hook silently. Our
        // proc is tiny but the OS scheduler can delay this thread under load
        // (AV scan, game launch, GPU driver hiccup). The thread sleeps in
        // GetMessage ~100 % of its life and only wakes for volume keys, so
        // promoting it costs nothing system-wide but makes the deadline
        // trivial to hit.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);

        let hmodule = GetModuleHandleW(None).expect("GetModuleHandleW failed");

        let class_name = w!("GoXLRVolumeWheelNotify");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(notify_wndproc),
            hInstance: hmodule.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        // Message-only window receives suspend/resume + session events on
        // this thread, so the re-hook runs on the very thread that owns the
        // installation — Win32 requires hooks live on their installing
        // thread, and the message-only HWND keeps these notifications out
        // of any future top-level window we might add.
        let notify_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!(""),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            hmodule,
            None,
        )
        .expect("CreateWindowExW failed");

        let mut pwr_raw: *mut core::ffi::c_void = std::ptr::null_mut();
        let pwr_handle = PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_WINDOW_HANDLE,
            HANDLE(notify_hwnd.0),
            &mut pwr_raw,
        )
        .is_ok()
        .then_some(HPOWERNOTIFY(pwr_raw as isize));
        let _ = WTSRegisterSessionNotification(notify_hwnd, NOTIFY_FOR_THIS_SESSION);

        // Subscribe to raw HID Consumer Control input. RIDEV_INPUTSINK
        // delivers WM_INPUT regardless of which window has focus, which is
        // what we need for a tray-only app. Failure is non-fatal: the
        // keyboard hook still works for devices that emit VK codes.
        let rid = [RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_CONSUMER,
            usUsage: HID_USAGE_CONSUMER_CONTROL,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: notify_hwnd,
        }];
        if let Err(err) =
            RegisterRawInputDevices(&rid, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
        {
            crate::log::error(&format!("RegisterRawInputDevices failed: {}", err));
        }

        let mut hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), hmodule, 0)
            .expect("SetWindowsHookExW failed");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            if msg.message == WM_REQUEST_REHOOK {
                REHOOK_REQUESTED.store(true, Ordering::Release);
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            if REHOOK_REQUESTED.swap(false, Ordering::AcqRel) {
                let _ = UnhookWindowsHookEx(hook);
                if let Ok(fresh) =
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), hmodule, 0)
                {
                    hook = fresh;
                }
            }
        }

        let _ = WTSUnRegisterSessionNotification(notify_hwnd);
        if let Some(h) = pwr_handle {
            let _ = PowerUnregisterSuspendResumeNotification(h);
        }
        let _ = DestroyWindow(notify_hwnd);
        let _ = UnhookWindowsHookEx(hook);
    }
}

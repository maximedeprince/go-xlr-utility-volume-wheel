use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

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
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_VOLUME_DOWN, VK_VOLUME_UP};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    RegisterClassExW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HWND_MESSAGE,
    KBDLLHOOKSTRUCT, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
    REGISTER_NOTIFICATION_FLAGS, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_KEYDOWN,
    WM_KEYUP, WM_POWERBROADCAST, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_WTSSESSION_CHANGE, WNDCLASSEXW,
};

#[derive(Debug, Clone, Copy)]
pub enum VolumeEvent {
    Up,
    Down,
}

static SENDER: OnceLock<UnboundedSender<VolumeEvent>> = OnceLock::new();

/// Set by the notification window proc when the OS hits a state that may
/// have invalidated our hook (resume from suspend, session unlock, RDP /
/// console connect). The message pump observes it after each dispatch and
/// reinstalls the hook on the same thread.
static REHOOK_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Win32 wParam values for WM_WTSSESSION_CHANGE that mean "the input
/// desktop has changed under us, the hook may have been detached".
/// windows-rs 0.58 does not export them as constants — encoding the
/// values the docs guarantee.
const WTS_CONSOLE_CONNECT: u32 = 0x1;
const WTS_REMOTE_CONNECT: u32 = 0x6;
const WTS_SESSION_UNLOCK: u32 = 0x8;

const DEVICE_NOTIFY_WINDOW_HANDLE: REGISTER_NOTIFICATION_FLAGS = REGISTER_NOTIFICATION_FLAGS(0);

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
    let needs_rehook = match msg {
        WM_POWERBROADCAST => matches!(
            w_param.0 as u32,
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND
        ),
        WM_WTSSESSION_CHANGE => matches!(
            w_param.0 as u32,
            WTS_SESSION_UNLOCK | WTS_REMOTE_CONNECT | WTS_CONSOLE_CONNECT
        ),
        _ => false,
    };
    if needs_rehook {
        REHOOK_REQUESTED.store(true, Ordering::Release);
    }
    DefWindowProcW(hwnd, msg, w_param, l_param)
}

pub fn run_hook(sender: UnboundedSender<VolumeEvent>) {
    let _ = SENDER.set(sender);

    unsafe {
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

        let mut hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), hmodule, 0)
            .expect("SetWindowsHookExW failed");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
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

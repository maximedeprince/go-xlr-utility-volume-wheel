use std::sync::OnceLock;

use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_VOLUME_DOWN, VK_VOLUME_UP};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

#[derive(Debug, Clone, Copy)]
pub enum VolumeEvent {
    Up,
    Down,
}

static SENDER: OnceLock<UnboundedSender<VolumeEvent>> = OnceLock::new();

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

pub fn run_hook(sender: UnboundedSender<VolumeEvent>) {
    let _ = SENDER.set(sender);

    unsafe {
        let hmodule = GetModuleHandleW(None).expect("GetModuleHandleW failed");
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), hmodule, 0)
            .expect("SetWindowsHookExW failed");

        // Low-level hooks require a message pump on the installing thread.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(hook);
    }
}

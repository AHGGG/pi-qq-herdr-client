use anyhow::Result;
use gpui_kit::{App, Window};

pub fn set_window_icon(window: &mut Window) {
    #[cfg(windows)]
    {
        use raw_window_handle::HasWindowHandle;
        use windows_sys::Win32::{
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_SHARED, LoadImageW, SendMessageW, WM_SETICON,
            },
        };
        if let Ok(handle) = window.window_handle() {
            if let raw_window_handle::RawWindowHandle::Win32(handle) = handle.as_raw() {
                unsafe {
                    let module = GetModuleHandleW(std::ptr::null());
                    for (kind, size) in [(ICON_SMALL, 16), (ICON_BIG, 32)] {
                        let icon = LoadImageW(
                            module,
                            1usize as *const u16,
                            IMAGE_ICON,
                            size,
                            size,
                            LR_SHARED,
                        );
                        if !icon.is_null() {
                            SendMessageW(
                                handle.hwnd.get() as *mut _,
                                WM_SETICON,
                                kind as usize,
                                icon as isize,
                            );
                        }
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = window;
}

pub fn hide(window: &mut Window, cx: &mut App) {
    #[cfg(windows)]
    {
        use raw_window_handle::HasWindowHandle;
        if let Ok(handle) = window.window_handle() {
            if let raw_window_handle::RawWindowHandle::Win32(handle) = handle.as_raw() {
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                        handle.hwnd.get() as *mut _,
                        windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE,
                    );
                }
            }
        }
        let _ = cx;
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        cx.hide();
    }
}
pub fn show(window: &mut Window, cx: &mut App) {
    #[cfg(windows)]
    {
        use raw_window_handle::HasWindowHandle;
        if let Ok(handle) = window.window_handle() {
            if let raw_window_handle::RawWindowHandle::Win32(handle) = handle.as_raw() {
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                        handle.hwnd.get() as *mut _,
                        windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOW,
                    );
                }
            }
        }
    }
    window.activate_window();
    cx.activate(true);
}
pub fn set_start_at_login(enabled: bool) -> Result<()> {
    #[cfg(windows)]
    {
        use winreg::{
            RegKey,
            enums::{HKEY_CURRENT_USER, KEY_SET_VALUE},
        };
        let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            KEY_SET_VALUE,
        )?;
        if enabled {
            key.set_value(
                "PiQQHerdrClient",
                &format!("\"{}\" --background", std::env::current_exe()?.display()),
            )?;
        } else {
            match key.delete_value("PiQQHerdrClient") {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        anyhow::bail!("登录启动目前仅支持 Windows");
    }
    Ok(())
}

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey,
};
use windows::core::Result;

/// 一次与指定窗口和 ID 绑定的全局热键注册。
pub(crate) struct RegisteredHotKey {
    hwnd: HWND,
    id: i32,
}

impl RegisteredHotKey {
    pub(crate) fn register(
        hwnd: HWND,
        id: i32,
        modifiers: HOT_KEY_MODIFIERS,
        virtual_key: u32,
    ) -> Result<Self> {
        unsafe { RegisterHotKey(Some(hwnd), id, modifiers, virtual_key)? };
        Ok(Self { hwnd, id })
    }
}

impl Drop for RegisteredHotKey {
    fn drop(&mut self) {
        let _ = unsafe { UnregisterHotKey(Some(self.hwnd), self.id) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, VK_F24,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    struct WindowGuard(HWND);

    impl Drop for WindowGuard {
        fn drop(&mut self) {
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn registered_hotkey_releases_the_combination_when_dropped() {
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                32,
                32,
                None,
                None,
                None,
                None,
            )
        }
        .expect("test window should be created");
        let _window = WindowGuard(hwnd);
        let modifiers = MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT;

        let registration = RegisteredHotKey::register(hwnd, 1001, modifiers, VK_F24.0 as u32)
            .expect("first registration should succeed");
        assert!(RegisteredHotKey::register(hwnd, 1002, modifiers, VK_F24.0 as u32).is_err());

        drop(registration);

        let replacement = RegisteredHotKey::register(hwnd, 1002, modifiers, VK_F24.0 as u32)
            .expect("dropping the owner should unregister the hotkey");
        drop(replacement);
    }
}

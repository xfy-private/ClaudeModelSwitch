//! Windows 开机自启注册（Phase 19）。
//!
//! 注册到 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`：
//!   - 启用：值 `ClaudeRelay` = `"<exe路径>" --autostart`
//!     （`--autostart` = 后台启动标志：开机自启时不弹配置页、端口被占时静默退出）
//!   - 禁用：删除该值。
//!
//! 幂等：重复注册/删除无害。非 Windows 平台为 no-op。
//! 注册表路径选用 HKCU（无需管理员权限、仅当前用户生效）。

#[cfg(target_os = "windows")]
pub fn apply(enabled: bool) -> Result<(), String> {
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const VALUE: &str = "ClaudeRelay";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe {
        if enabled {
            let exe =
                std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
            let cmd = format!("\"{}\" --autostart", exe.display());
            let mut key = std::ptr::null_mut();
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            );
            if rc != 0 {
                return Err(format!("创建 Run 注册表键失败: {rc}"));
            }
            let data = wide(&cmd);
            let rc = RegSetValueExW(
                key,
                wide(VALUE).as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            );
            RegCloseKey(key);
            if rc != 0 {
                return Err(format!("写入开机自启项失败: {rc}"));
            }
            Ok(())
        } else {
            let mut key = std::ptr::null_mut();
            let rc = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut key,
            );
            if rc == ERROR_FILE_NOT_FOUND {
                return Ok(()); // Run 键不存在 = 本来就没注册
            }
            if rc != 0 {
                return Err(format!("打开 Run 注册表键失败: {rc}"));
            }
            let rc = RegDeleteValueW(key, wide(VALUE).as_ptr());
            RegCloseKey(key);
            if rc == ERROR_FILE_NOT_FOUND {
                return Ok(()); // 值不存在 = 未注册
            }
            if rc != 0 {
                return Err(format!("删除开机自启项失败: {rc}"));
            }
            Ok(())
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn apply(_enabled: bool) -> Result<(), String> {
    Ok(())
}

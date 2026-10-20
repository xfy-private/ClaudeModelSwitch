//! 桌面悬浮小卡片：实时显示当前使用模型的今日用量 / 余额 / Coding Plan。
//!
//! 设计要点：
//!   - **分层窗口 + 逐像素 alpha**（`WS_POPUP | WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW`）：
//!     渲染到 32bpp DIB（GDI+ 画内容）-> 自绘 SDF 圆角 alpha 掩膜 + 逐像素预乘 ->
//!     `UpdateLayeredWindow` 上屏。圆角是真正的亚像素抗锯齿（`SetWindowRgn` 是 1-bit 掩膜，
//!     四角必然锯齿）；ULW 未成功时分层窗口整体不可见，绝不露屏幕残留位。
//!   - 数据刷新由 main 的 tokio 任务读内存快照 + PostMessageW 通知重绘；**仅数据变化才重绘**
//!     （空闲零重绘）。WM_PAINT 绘制跑在托盘线程（GetMessageW 消息泵调度本线程所有窗口）。
//!   - 整卡 `WM_NCHITTEST` 返回 HTCAPTION -> 系统原生拖拽（丝滑）；右上角关闭区 HTCLIENT；
//!     `WM_MOUSEACTIVATE -> MA_NOACTIVATE` 不抢 CC 焦点；双击卡片打开配置页。
//!   - 位置 / 可见性存 `*-card.json`（原子写 tmp+rename），重启记忆并 clamp 进工作区。
//!
//! 注意：windows-sys 0.59 里 `BeginPaint/EndPaint/PAINTSTRUCT/SetWindowRgn/InvalidateRect/
//! UpdateWindow/CreateRoundRectRgn` 都归类在 **`Win32::Graphics::Gdi`**（gdi/user32 绘制函数），
//! 不在 `Win32::UI::WindowsAndMessaging`——import 放错模块会直接编译失败。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::config::AppState;

// ===== 跨平台公共类型（main/tray 依赖，非 Windows 也有最小占位） =====

/// 卡片数据快照（由 main 的 tokio 任务更新；托盘线程只读渲染）。
#[derive(Default, Clone, PartialEq)]
pub struct WidgetData {
    pub provider: String,     // 服务商显示名
    pub provider_id: String,  // 服务商 id（用于把余额/Coding Plan 过滤到当前模型）
    pub alias: String,        // 模型完整别名，如 ds-v4-flash
    pub real: String,     // 真实模型名
    pub ctx: u64,         // 上下文窗口
    pub today_requests: u64,
    pub today_input: u64,
    pub today_output: u64,
    pub balance_line: String, // 如 "余额 ¥5.10"；空 = 无
    pub coding_line: String,  // 如 "5h 65% · 月 30%"；空 = 无
    pub paused: bool,         // 服务是否已停止
}

/// 卡片主题（持久化到 config-card.json，默认暗黑）。
#[derive(Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// 卡片共享状态：窗口句柄（0=未创建）+ 数据快照 + 持久化路径 + 配置页 URL + 主题 + AppState。
pub struct WidgetShared {
    pub hwnd: std::sync::atomic::AtomicIsize,
    pub data: Mutex<WidgetData>,
    pub path: PathBuf,
    pub base_url: String,
    /// 卡片主题（托盘线程读写；持久化在 config-card.json）。
    pub theme: Mutex<Theme>,
    /// AppState（卡片内切换模型用：`set_default_model_blocking` 写盘 + 热重载）。
    pub state: Arc<AppState>,
}

impl WidgetShared {
    pub fn new(config_path: &Path, base_url: String, state: Arc<AppState>) -> Arc<WidgetShared> {
        Arc::new(WidgetShared {
            hwnd: std::sync::atomic::AtomicIsize::new(0),
            data: Mutex::new(WidgetData::default()),
            path: card_path_of(config_path),
            base_url,
            theme: Mutex::new(Theme::Dark),
            state,
        })
    }
}

/// 持久化路径：`config.json` -> `config-card.json`（镜像 main::usage_path_of）。
pub fn card_path_of(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "card".to_string());
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(format!("{stem}-card.json")),
        _ => PathBuf::from(format!("{stem}-card.json")),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn create_window(_shared: &Arc<WidgetShared>) {}
#[cfg(not(target_os = "windows"))]
pub fn toggle_visible(_shared: &Arc<WidgetShared>) {}
#[cfg(not(target_os = "windows"))]
pub fn is_visible(_shared: &Arc<WidgetShared>) -> bool {
    false
}
#[cfg(not(target_os = "windows"))]
pub fn spawn_data_task(
    _state: Arc<AppState>,
    _shared: Arc<WidgetShared>,
    _base_url: String,
    _paused: tokio::sync::watch::Receiver<bool>,
) {
}

// ===== Windows 实现 =====

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use std::sync::atomic::Ordering;

    use windows_sys::Win32::Foundation::{GetLastError, POINT, RECT, SIZE};
                                                        use windows_sys::Win32::Graphics::Gdi::{
        AC_SRC_ALPHA, AC_SRC_OVER, BeginPaint, BI_RGB, BITMAPINFO, BLENDFUNCTION,
        CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject, EndPaint,
        FillRect, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, SelectObject,
        UpdateWindow, DIB_RGB_COLORS, HBITMAP, HBRUSH, HDC, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        PAINTSTRUCT,
    };
    use windows_sys::Win32::Graphics::GdiPlus::{
        GdipAddPathArcI, GdipClosePathFigure, GdipCreateFont, GdipCreateFontFamilyFromName,
        GdipCreateFromHDC, GdipCreateLineBrushI, GdipCreatePath, GdipCreatePen1, GdipCreateSolidFill,
        GdipDrawLineI,
        GdipCreateStringFormat, GdipDeleteBrush, GdipDeleteFont, GdipDeleteFontFamily,
        GdipDeleteGraphics, GdipDeletePath, GdipDeletePen, GdipDeleteStringFormat,
        GdipDrawPath, GdipDrawString, GdipFillPath, GdipFillRectangleI, GdipFlush, GdipMeasureString,
        GdipSetPixelOffsetMode, GdipSetSmoothingMode, GdipSetStringFormatAlign,
        GdipSetStringFormatLineAlign, GdipSetTextRenderingHint, FillModeAlternate,
        FlushIntentionSync, FontStyleBold, FontStyleRegular, GpBrush, GpFont, GpFontFamily,
        GpGraphics, GpPath, GpPen, GpSolidFill, GpStringFormat, PixelOffsetModeHighQuality, Point,
        RectF, SmoothingModeAntiAlias, StringAlignmentCenter, StringAlignmentNear,
        TextRenderingHintClearTypeGridFit, UnitPixel, WrapModeClamp, GdiplusShutdown,
        GdiplusStartup,
        GdiplusStartupInput,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
                                                        use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, GetClientRect, GetWindowLongPtrW, GetWindowRect,
        LoadCursorW, MA_NOACTIVATE, PostMessageW, RegisterClassW, SetWindowLongPtrW, ShowWindow,
        WM_APP, WM_DESTROY, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_LBUTTONUP, WM_MOUSEACTIVATE,
        WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_PAINT, WNDCLASSW, CS_DBLCLKS, GWL_STYLE,
        ULW_ALPHA, UpdateLayeredWindow,
        GWLP_USERDATA, HTCAPTION,
        HTCLIENT, IDC_ARROW, SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_LAYERED, WS_EX_TOPMOST,
        WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
    };

    // 逻辑尺寸（CSS 像素），按 DPI 缩放到物理像素绘制。
    const W_L: f32 = 300.0;
    const H_L: f32 = 140.0;
    const R_L: f32 = 14.0;
    const PAD: f32 = 15.0;

    const WM_DATA_UPDATED: u32 = WM_APP + 1; // tokio 数据任务通知重绘

    /// 持久化卡片状态（config-card.json）。
    #[derive(Default, Clone, Serialize, Deserialize)]
    struct CardState {
        x: i32,
        y: i32,
        #[serde(default = "default_visible")]
        visible: bool,
        #[serde(default)]
        theme: Theme,
    }
    fn default_visible() -> bool {
        // 首次运行默认显示（新功能可发现）；之后记住用户选择
        true
    }
    impl CardState {
        fn load(path: &Path) -> (CardState, bool) {
            match std::fs::read_to_string(path) {
                Ok(raw) => match serde_json::from_str::<CardState>(&raw) {
                    Ok(c) => (c, true),
                    Err(e) => {
                        eprintln!("[card] 状态文件损坏，用默认: {e}");
                        (CardState::default(), false)
                    }
                },
                Err(_) => (CardState::default(), false),
            }
        }
        fn save(&self, path: &Path) {
            if let Ok(s) = serde_json::to_string_pretty(self) {
                // Phase 59：统一自愈式原子写（只读属性自愈 + 原子替换；失败静默保持原语义）
                let _ = crate::config::write_file_atomic(path, &s);
            }
        }
    }

    fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
        ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
    }
    fn rgb(r: u8, g: u8, b: u8) -> u32 {
        argb(255, r, g, b)
    }

    /// 主题调色板（暗黑默认 / 明亮两套）。
    #[derive(Clone, Copy)]
    struct Palette {
        bg0: u32,
        bg1: u32, // 背景渐变
        border: u32,
        title: u32,
        sub: u32,   // 真实模型名（副标题）
        muted: u32, // 次级文字（次请求/余额行）
        req: u32,
        in_txt: u32,
        out_txt: u32,
        ctx_bg: u32,
        ctx_txt: u32,
        close: u32,
        icon: u32, // 主题/切换图标
    }
    fn palette(t: Theme) -> Palette {
        match t {
            Theme::Dark => Palette {
                bg0: rgb(0x13, 0x13, 0x19),
                bg1: rgb(0x20, 0x20, 0x2B),
                border: 0x55_FF_FF_FF,
                title: rgb(0xF4, 0xF4, 0xF5),
                sub: rgb(0x71, 0x71, 0x7A),
                muted: rgb(0x9C, 0xA3, 0xAF),
                req: rgb(0xFF, 0xFF, 0xFF),
                in_txt: rgb(0xA5, 0xB4, 0xFC),
                out_txt: rgb(0x7D, 0xDD, 0xFC),
                ctx_bg: rgb(0x31, 0x2E, 0x81),
                ctx_txt: rgb(0xC7, 0xD2, 0xFE),
                close: rgb(0x7A, 0x7A, 0x82),
                icon: rgb(0x9C, 0xA3, 0xAF),
            },
            Theme::Light => Palette {
                bg0: rgb(0xFF, 0xFF, 0xFF),
                bg1: rgb(0xEE, 0xF2, 0xF7),
                border: 0x40_1F_29_37,
                title: rgb(0x1F, 0x29, 0x37),
                sub: rgb(0x6B, 0x72, 0x80),
                muted: rgb(0x4B, 0x55, 0x63),
                req: rgb(0x11, 0x18, 0x27),
                in_txt: rgb(0x4F, 0x46, 0xE5),
                out_txt: rgb(0x02, 0x84, 0xC7),
                ctx_bg: rgb(0xEE, 0xF2, 0xFF),
                ctx_txt: rgb(0x43, 0x38, 0xCA),
                close: rgb(0x94, 0xA3, 0xB8),
                icon: rgb(0x64, 0x74, 0x8B),
            },
        }
    }

    /// 服务商 -> 徽章色（按名称/前缀匹配）。
    fn provider_color(s: &str) -> u32 {
        let l = s.to_lowercase();
        if l.contains("deepseek") || l.contains("ds") {
            rgb(0x25, 0x63, 0xEB)
        } else if l.contains("讯飞") || l.contains("xf") || l.contains("xfyun") {
            rgb(0xEF, 0x44, 0x44)
        } else if l.contains("火山") || l.contains("volc") || l.contains("ark") {
            rgb(0xF9, 0x73, 0x16)
        } else if l.contains("kimi") || l.contains("moonshot") {
            rgb(0xA8, 0x55, 0xF7)
        } else if l.contains("glm") || l.contains("智谱") || l.contains("zhipu") {
            rgb(0x10, 0xB9, 0x81)
        } else if l.contains("anthropic") || l.contains("claude") {
            rgb(0xD9, 0x77, 0x06)
        } else {
            rgb(0x16, 0xA3, 0x4A) // 品牌绿（默认）
        }
    }

    fn display_name(p: &crate::config::ProviderConfig) -> String {
        if p.name.trim().is_empty() {
            p.id.clone()
        } else {
            p.name.trim().to_string()
        }
    }

    /// UTF-8 -> 宽字符 + NUL（供 GDI+ 的 PCWSTR）。
    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // ---------- 常驻 GDI+ 资源（线程局部，paint 恒跑在托盘线程） ----------

    struct GdiEnv {
        w: i32, // 物理像素宽（按 DPI）
        h: i32,
        dpi: f32,
        /// 离屏 32bpp DIB（分层窗口 ULW 上屏用；bits = BGRA 行主序）
        mem_dc: HDC,
        dib: HBITMAP,
        bits: *mut u32,
        family: *mut GpFontFamily,
        font_title: *mut GpFont, // 14 加粗
        font_body: *mut GpFont,  // 11
        font_num: *mut GpFont,   // 20 加粗
        font_small: *mut GpFont, // 10.5
        fmt_near: *mut GpStringFormat,
        fmt_center: *mut GpStringFormat,
        _token: usize,
    }

    impl Drop for GdiEnv {
        fn drop(&mut self) {
            unsafe {
                GdipDeleteFont(self.font_title);
                GdipDeleteFont(self.font_body);
                GdipDeleteFont(self.font_num);
                GdipDeleteFont(self.font_small);
                GdipDeleteFontFamily(self.family);
                GdipDeleteStringFormat(self.fmt_near);
                GdipDeleteStringFormat(self.fmt_center);
                GdiplusShutdown(self._token);
                if !self.mem_dc.is_null() {
                    DeleteDC(self.mem_dc);
                }
                if !self.dib.is_null() {
                    DeleteObject(self.dib);
                }
            }
        }
    }

    impl GdiEnv {
        fn new() -> GdiEnv {
            let mut token: usize = 0;
            let input = GdiplusStartupInput {
                GdiplusVersion: 1,
                DebugEventCallback: 0,
                SuppressBackgroundThread: 0,
                SuppressExternalCodecs: 0,
            };
            unsafe {
                if GdiplusStartup(&mut token, &input, std::ptr::null_mut()) != 0 {
                    eprintln!("[card] GDI+ 初始化失败");
                }
            }

            let dpi = dpi_scale();
            let pw = (W_L * dpi).round() as i32;
            let ph = (H_L * dpi).round() as i32;

            // 离屏 32bpp 顶朝下 DIB + 内存 DC（一次创建，paint 反复复用）
            let (mem_dc, dib, bits) = unsafe {
                let mut bmi: BITMAPINFO = std::mem::zeroed();
                bmi.bmiHeader.biSize = std::mem::size_of::<
                    windows_sys::Win32::Graphics::Gdi::BITMAPINFOHEADER,
                >() as u32;
                bmi.bmiHeader.biWidth = pw;
                bmi.bmiHeader.biHeight = -ph; // 顶朝下
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 32;
                bmi.bmiHeader.biCompression = BI_RGB;
                let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
                let dib = CreateDIBSection(
                    std::ptr::null_mut(),
                    &bmi,
                    DIB_RGB_COLORS,
                    &mut bits,
                    std::ptr::null_mut(),
                    0,
                );
                let mem_dc = CreateCompatibleDC(std::ptr::null_mut());
                if !dib.is_null() && !mem_dc.is_null() {
                    SelectObject(mem_dc, dib);
                }
                (mem_dc, dib, bits as *mut u32)
            };

            let family = unsafe {
                let name = w("Microsoft YaHei UI");
                let mut fam: *mut GpFontFamily = std::ptr::null_mut();
                let _ = GdipCreateFontFamilyFromName(name.as_ptr(), std::ptr::null_mut(), &mut fam);
                fam
            };
            let f = |size: f32, style: i32| -> *mut GpFont {
                unsafe {
                    let mut font: *mut GpFont = std::ptr::null_mut();
                    let _ = GdipCreateFont(family, size * dpi, style, UnitPixel, &mut font);
                    font
                }
            };
            let font_title = f(14.0, FontStyleBold as i32);
            let font_body = f(11.0, FontStyleRegular as i32);
            let font_num = f(20.0, FontStyleBold as i32);
            let font_small = f(10.5, FontStyleRegular as i32);

            let (fmt_near, fmt_center) = unsafe {
                let mut nf: *mut GpStringFormat = std::ptr::null_mut();
                let mut cf: *mut GpStringFormat = std::ptr::null_mut();
                let _ = GdipCreateStringFormat(0, 0, &mut nf);
                let _ = GdipCreateStringFormat(0, 0, &mut cf);
                if !nf.is_null() {
                    GdipSetStringFormatAlign(nf, StringAlignmentNear);
                    GdipSetStringFormatLineAlign(nf, StringAlignmentNear);
                }
                if !cf.is_null() {
                    GdipSetStringFormatAlign(cf, StringAlignmentCenter);
                    GdipSetStringFormatLineAlign(cf, StringAlignmentCenter);
                }
                (nf, cf)
            };

            GdiEnv {
                w: pw,
                h: ph,
                dpi,
                mem_dc,
                dib,
                bits,
                family,
                font_title,
                font_body,
                font_num,
                font_small,
                fmt_near,
                fmt_center,
                _token: token,
            }
        }
    }

    fn dpi_scale() -> f32 {
        let dpi = unsafe { GetDpiForSystem() };
        if dpi == 0 {
            1.0
        } else {
            dpi as f32 / 96.0
        }
    }

    /// 深色背景画刷（泄漏给 WNDCLASS，进程存活期间有效）。
    /// COLORREF = 0x00BBGGRR；#131319（R=0x13,G=0x13,B=0x19）-> 0x00191313。
    fn dark_brush() -> windows_sys::Win32::Graphics::Gdi::HBRUSH {
        unsafe { windows_sys::Win32::Graphics::Gdi::CreateSolidBrush(0x0019_1313) }
    }

    /// 主题底色（COLORREF 0x00BBGGRR）：暗 #131319 / 亮 #FFFFFF。
    fn theme_bg_color(theme: Theme) -> u32 {
        match theme {
            Theme::Dark => 0x0019_1313,
            Theme::Light => 0x00FF_FFFF,
        }
    }

    /// 罕见路径诊断：追加一行到 `config-card-diag.log`（服务黑框隐藏时 stderr 不可见，
    /// 排查「卡片背景异常」这类真机渲染问题用）。
    fn diag(shared: &WidgetShared, msg: &str) {
        let p = shared.path.with_extension("diag.log");
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{msg}");
        }
    }

    /// 在托盘线程创建卡片窗口（标准顶置 popup + 圆角区域 + 首帧绘制 + 按记忆显隐）。
    pub fn create_window(shared: &Arc<WidgetShared>) {
        let (mut st, existed) = CardState::load(&shared.path);
        *shared.theme.lock().unwrap() = st.theme; // 恢复上次主题（默认暗黑）
        let dpi = dpi_scale();
        let cw = (W_L * dpi).round() as i32;
        let ch = (H_L * dpi).round() as i32;
        if !existed {
            // 首次：放到主屏工作区右上角
            let pt = POINT { x: 100, y: 100 };
            unsafe {
                let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
                let mut mi: MONITORINFO = std::mem::zeroed();
                mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                if GetMonitorInfoW(mon, &mut mi) != 0 {
                    let w = mi.rcWork;
                    st.x = w.right - cw - 24;
                    st.y = w.top + 24;
                    st.visible = true;
                } else {
                    st.x = 100;
                    st.y = 100;
                }
            }
        } else {
            clamp_to_work_area(&mut st, cw, ch);
        }

        let class_name: Vec<u16> = "ClaudeRelayCard"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
        let wc = unsafe {
            WNDCLASSW {
                style: CS_DBLCLKS as u32,
                lpfnWndProc: Some(card_wndproc),
                hInstance: hinst,
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                hbrBackground: dark_brush(),
                lpszClassName: class_name.as_ptr(),
                ..std::mem::zeroed()
            }
        };
        unsafe { RegisterClassW(&wc) };

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class_name.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                st.x,
                st.y,
                cw,
                ch,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            eprintln!("[card] 窗口创建失败 error={}", unsafe { GetLastError() });
            return;
        }
        // 圆角走逐像素 alpha（SDF 掩膜 + ULW），不用 SetWindowRgn（1-bit 掩膜四角锯齿）
        // 状态指针挂到窗口
        let leaked = Arc::into_raw(shared.clone()) as isize;
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, leaked);
        }
        shared.hwnd.store(hwnd as isize, Ordering::SeqCst);

        // 先渲染一帧再显示：分层窗口 ULW 前不可见，不存在露垃圾位的问题
        render_and_present(hwnd);

        if st.visible {
            unsafe {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                // 显示后强制同步重绘一帧：隐藏中创建的窗口从未收过 WM_PAINT
                // （Windows 不给隐藏窗口投递 paint），表面是未初始化位
                // （DWM 常以屏幕内容填充 -> 「页面当背景」）；erase=1 先清再画。
                InvalidateRect(hwnd, std::ptr::null(), 1);
                UpdateWindow(hwnd);
            }
        }
        let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
        eprintln!(
            "[card] 桌面卡片已创建（{}×{}，pos=({},{})，{}）visible_bit={:#x}",
            cw,
            ch,
            st.x,
            st.y,
            if st.visible { "显示" } else { "隐藏" },
            (ex as u32) & (WS_VISIBLE as u32)
        );
    }

    fn clamp_to_work_area(st: &mut CardState, cw: i32, ch: i32) {
        unsafe {
            let pt = POINT {
                x: st.x + cw / 2,
                y: st.y + ch / 2,
            };
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(mon, &mut mi) == 0 {
                return;
            }
            let w = mi.rcWork;
            st.x = st.x.clamp(w.left, w.right - cw);
            st.y = st.y.clamp(w.top, w.bottom - ch);
        }
    }

    unsafe extern "system" fn card_wndproc(
        hwnd: windows_sys::Win32::Foundation::HWND,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        let shared: Option<&WidgetShared> = {
            let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
            if raw == 0 {
                None
            } else {
                Some(unsafe { &*(raw as *const WidgetShared) })
            }
        };
        let shared = match shared {
            Some(s) => s,
            None => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        };
        match msg {
            WM_DATA_UPDATED => {
                unsafe {
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    UpdateWindow(hwnd);
                }
                0
            }
            WM_PAINT => {
                // 分层窗口：窗口 DC 不参与合成，仅验证更新区；真正上屏在
                // render_and_present 的 UpdateLayeredWindow。
                unsafe {
                    let mut ps: PAINTSTRUCT = std::mem::zeroed();
                    BeginPaint(hwnd, &mut ps);
                    render_and_present(hwnd);
                    EndPaint(hwnd, &ps);
                }
                0
            }
            WM_ERASEBKGND => {
                // 分层窗口不显示窗口 DC 内容，无需擦除
                1
            }
            WM_NCHITTEST => {
                let dpi = dpi_scale();
                let cw = (W_L * dpi).round() as i32;
                let sx = (lparam & 0xFFFF) as i32 as i16 as i32;
                let sy = ((lparam >> 16) & 0xFFFF) as i32 as i16 as i32;
                let mut r: RECT = unsafe { std::mem::zeroed() };
                if unsafe { GetWindowRect(hwnd, &mut r) } != 0 {
                    let x = sx - r.left;
                    let y = sy - r.top;
                    // 右上角三按钮区（主题/切换/关闭，88px 宽）-> HTCLIENT（收 WM_LBUTTONUP）
                    let btn_w = (88.0 * dpi) as i32;
                    let btn_h = (30.0 * dpi) as i32;
                    if x >= cw - btn_w && y < btn_h {
                        return HTCLIENT as isize;
                    }
                }
                HTCAPTION as isize
            }
            WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
            WM_NCLBUTTONDBLCLK => {
                // 双击卡片 -> 打开配置页
                let _ = crate::tray::open_browser(&shared.base_url);
                0
            }
            WM_LBUTTONUP => {
                let dpi = dpi_scale();
                let cw = (W_L * dpi).round() as i32;
                let x = (lparam & 0xFFFF) as i32 as i16 as i32;
                let y = ((lparam >> 16) & 0xFFFF) as i32 as i16 as i32;
                let theme = *shared.theme.lock().unwrap();
                if y < (34.0 * dpi) as i32 {
                    if x >= cw - (36.0 * dpi) as i32 {
                        // 关闭
                        hide_and_save(hwnd, &shared.path, theme);
                    } else if x >= cw - (62.0 * dpi) as i32 {
                        // 切换模型
                        cycle_model(shared);
                    } else if x >= cw - (88.0 * dpi) as i32 {
                        // 主题
                        toggle_theme(hwnd, shared);
                    }
                }
                0
            }
            WM_EXITSIZEMOVE => {
                let theme = *shared.theme.lock().unwrap();
                save_pos(hwnd, &shared.path, true, theme);
                0
            }
            WM_DESTROY => 0,
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn hide_and_save(hwnd: windows_sys::Win32::Foundation::HWND, path: &Path, theme: Theme) {
        unsafe { ShowWindow(hwnd, SW_HIDE) };
        save_pos(hwnd, path, false, theme);
    }

    fn save_pos(
        hwnd: windows_sys::Win32::Foundation::HWND,
        path: &Path,
        visible: bool,
        theme: Theme,
    ) {
        let mut r: RECT = unsafe { std::mem::zeroed() };
        if unsafe { GetWindowRect(hwnd, &mut r) } != 0 {
            let s = CardState {
                x: r.left,
                y: r.top,
                visible,
                theme,
            };
            s.save(path);
        }
    }

    // ---------- 绘制 ----------

    /// 渲染整卡并上屏：GDI+ 画进 32bpp DIB -> GDI 底色兜底 -> SDF 圆角 alpha
    /// 掩膜 + 逐像素预乘 -> UpdateLayeredWindow（分层窗口唯一上屏路径）。
    fn render_and_present(hwnd: windows_sys::Win32::Foundation::HWND) {
        let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
        if raw == 0 {
            return;
        }
        let shared = unsafe { &*(raw as *const WidgetShared) };
        let data = shared.data.lock().unwrap().clone();
        let theme = *shared.theme.lock().unwrap();

        thread_local! {
            static ENV: GdiEnv = GdiEnv::new();
        }
        ENV.with(|env| {
            unsafe {
                if env.mem_dc.is_null() || env.dib.is_null() || env.bits.is_null() {
                    return;
                }
                // GDI 实底打底（不依赖 GDI+）：GDI+ 整体失败时也是主题底色卡
                let brush: HBRUSH = CreateSolidBrush(theme_bg_color(theme));
                if !brush.is_null() {
                    let mut r: RECT = std::mem::zeroed();
                    if GetClientRect(hwnd, &mut r) != 0 {
                        FillRect(env.mem_dc, &r, brush);
                    }
                    DeleteObject(brush);
                }
                let mut g: *mut GpGraphics = std::ptr::null_mut();
                let st = GdipCreateFromHDC(env.mem_dc, &mut g);
                if st != 0 || g.is_null() {
                    eprintln!(
                        "[card] GdipCreateFromHDC(mem dc) 失败 status={st} error={}",
                        GetLastError()
                    );
                    static FAILED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !FAILED.swap(true, Ordering::SeqCst) {
                        diag(shared, &format!("gdiplus_create_from_hdc_failed status={st}"));
                    }
                } else {
                    static PAINTED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !PAINTED.swap(true, Ordering::SeqCst) {
                        diag(shared, "paint_ok");
                    }
                    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
                    GdipSetPixelOffsetMode(g, PixelOffsetModeHighQuality);
                    // ClearTypeGridFit：小字号带 hinting 的子像素渲染，比纯 AA 锐利得多
                    // （11px 小字纯 AntiAlias 会发糊）
                    GdipSetTextRenderingHint(g, TextRenderingHintClearTypeGridFit);
                    draw_content(g, env, &data, theme);
                    GdipFlush(g, FlushIntentionSync);
                    GdipDeleteGraphics(g);
                }
                // SDF 圆角 alpha 掩膜 + 预乘（ULW 需要预乘 BGRA）
                apply_round_mask(env);
                // 上屏（pptDst=null 保持当前位置）
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                let size = SIZE { cx: env.w, cy: env.h };
                let zero = POINT { x: 0, y: 0 };
                UpdateLayeredWindow(
                    hwnd,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                    env.mem_dc,
                    &zero,
                    0,
                    &blend,
                    ULW_ALPHA,
                );
            }
        });
    }

    /// 圆角矩形 SDF alpha 掩膜 + 逐像素预乘：圆角内 A=255、外 A=0、边缘亚像素渐变。
    /// 掩膜完全接管 alpha，不依赖 GDI+ 的 alpha 写入行为（内容全部不透明绘制）。
    unsafe fn apply_round_mask(env: &GdiEnv) {
        let (w, h) = (env.w as f32, env.h as f32);
        let r = R_L * env.dpi;
        let hw = w / 2.0 - r; // 直边段半长
        let hh = h / 2.0 - r;
        for y in 0..env.h {
            let py = y as f32 + 0.5;
            let dy = (py - h / 2.0).abs() - hh;
            for x in 0..env.w {
                let px = x as f32 + 0.5;
                let dx = (px - w / 2.0).abs() - hw;
                // rounded-rect SDF：角落取圆距离，其余取直边
                let d = if dx > 0.0 && dy > 0.0 {
                    ((dx * dx + dy * dy) as f32).sqrt() - r
                } else {
                    dx.max(dy) - r
                };
                let a = (0.5 - d).clamp(0.0, 1.0);
                let a8 = (a * 255.0 + 0.5) as u32;
                let i = (y * env.w + x) as isize;
                let v = *env.bits.offset(i);
                let b = v & 0xFF;
                let g = (v >> 8) & 0xFF;
                let rr = (v >> 16) & 0xFF;
                let b = (b * a8 + 127) / 255;
                let g = (g * a8 + 127) / 255;
                let rr = (rr * a8 + 127) / 255;
                *env.bits.offset(i) = b | (g << 8) | (rr << 16) | (a8 << 24);
            }
        }
    }

    #[allow(non_snake_case)]
    unsafe fn draw_content(g: *mut GpGraphics, env: &GdiEnv, d: &WidgetData, theme: Theme) {
        let dpi = dpi_scale();
        let S = dpi;
        let W = env.w as f32;
        let H = env.h as f32;
        let pal = palette(theme);

        // ---- 背景：整面不透明渐变 ----
        let p1 = Point { X: 0, Y: 0 };
        let p2 = Point { X: 0, Y: env.h };
        let mut bg: *mut windows_sys::Win32::Graphics::GdiPlus::GpLineGradient = std::ptr::null_mut();
        let _ = GdipCreateLineBrushI(&p1, &p2, pal.bg0, pal.bg1, WrapModeClamp, &mut bg);
        let _ = GdipFillRectangleI(g, bg as *mut GpBrush, 0, 0, env.w, env.h);
        let _ = GdipDeleteBrush(bg as *mut GpBrush);

        // ---- 圆角描边（内缩 1px，四边四角一致，不被 SetWindowRgn 裁掉半根线）----
        let mut path: *mut GpPath = std::ptr::null_mut();
        let _ = GdipCreatePath(FillModeAlternate, &mut path);
        round_path(path, 1.0, 1.0, W - 2.0, H - 2.0, R_L * S);
        let mut pen: *mut GpPen = std::ptr::null_mut();
        let _ = GdipCreatePen1(pal.border, 1.0, UnitPixel, &mut pen);
        let _ = GdipDrawPath(g, pen, path);
        let _ = GdipDeletePen(pen);
        let _ = GdipDeletePath(path);

        // ---- 标题行：徽章 + 真实模型名 + 上下文徽章 + 三按钮，同一水平带内垂直居中 ----
        // 各元素高度不同（18/16/文字），统一按带中心对齐：y = row_top + (row_h-h)/2；
        // 文字一律 text_center（StringFormat 行居中）在整带高度里画，光学中线自然一致。
        let row_top = 10.0 * S;
        let row_h = 22.0 * S;
        let mut x = PAD * S;
        let prov = if d.provider.is_empty() { "?" } else { &d.provider };
        let badge_h = 18.0 * S;
        let badge_y = row_top + (row_h - badge_h) / 2.0;
        let prov_w = measure(g, env.font_body, prov);
        let badge_w = prov_w + 16.0 * S;
        fill_round(g, x, badge_y, badge_w, badge_h, badge_h / 2.0, provider_color(&d.provider));
        text_center(g, env, env.font_body, x, badge_y, badge_w, badge_h, prov, argb(255, 0xEC, 0xFD, 0xF5));
        x += badge_w + 8.0 * S;

        // 标题直接显示真实模型名（不显示别名--服务商+别名+真实名两行容易挤换行）
        let name = if !d.real.is_empty() {
            d.real.clone()
        } else if !d.alias.is_empty() {
            d.alias.clone()
        } else {
            "（未设置）".to_string()
        };
        let name_max = W - x - 130.0 * S; // 留 ctx + 三个按钮空间
        let name = fit_text(g, env.font_title, &name, name_max);
        let name_w = measure(g, env.font_title, &name);
        text_center(g, env, env.font_title, x, row_top, name_w, row_h, &name, pal.title);
        x += name_w + 6.0 * S;

        if d.ctx > 0 {
            let ctx_txt = format!("[{}]", fmt_ctx(d.ctx));
            let cw = measure(g, env.font_body, &ctx_txt) + 10.0 * S;
            let ch = 16.0 * S;
            let cy = row_top + (row_h - ch) / 2.0;
            fill_round(g, x, cy, cw, ch, 8.0 * S, pal.ctx_bg);
            text_center(g, env, env.font_body, x, cy, cw, ch, &ctx_txt, pal.ctx_txt);
        }

        // 右上角三按钮：主题（暗黑☾/明亮☀）、切换模型（⇄）、关闭（✕）
        // 与 WM_LBUTTONUP 判定一致：主题区 W-88..W-62，切换区 W-62..W-36，关闭区 W-36..W
        // 各自在自己的命中区内水平+垂直居中
        let theme_glyph = if theme == Theme::Dark { "☾" } else { "☀" };
        text_center(g, env, env.font_body, W - 88.0 * S, row_top, 26.0 * S, row_h, theme_glyph, pal.icon);
        text_center(g, env, env.font_body, W - 62.0 * S, row_top, 26.0 * S, row_h, "⇄", pal.icon);
        text_center(g, env, env.font_body, W - 36.0 * S, row_top, 36.0 * S, row_h, "✕", pal.close);

                // ---- 今日用量：请求 / 输入 / 输出 三列一行，垂直居中于卡片中部 ----
        let col_w = W / 3.0;
        let num_y = 56.0 * S; // 数值行（20 号粗体）
        let label_y = 84.0 * S; // 标签行（副色小字）
        let label_h = 14.0 * S;
        let stats = [
            (fmt_parts(d.today_requests, 1), "请求次数", pal.req),
            (fmt_parts(d.today_input, 3), "输入 tokens", pal.in_txt),
            (fmt_parts(d.today_output, 3), "输出 tokens", pal.out_txt),
        ];
        for (i, ((big, small), label, color)) in stats.iter().enumerate() {
            let cx = col_w * i as f32;
            draw_stat(g, env, cx + col_w / 2.0, num_y, big, small, *color, col_w - 10.0 * S);
            text_center(g, env, env.font_small, cx, label_y, col_w, label_h, label, pal.sub);
        }
        // 列间细分隔线（弱化，仅提示分组）
        let divider = if theme == Theme::Dark {
            argb(255, 0x3A, 0x3A, 0x46)
        } else {
            argb(255, 0xE2, 0xE8, 0xF0)
        };
        let mut pen: *mut GpPen = std::ptr::null_mut();
        let _ = GdipCreatePen1(divider, 1.0, UnitPixel, &mut pen);
        for i in 1..3 {
            let x = col_w * i as f32;
            let _ = GdipDrawLineI(g, pen, x as i32, (60.0 * S) as i32, x as i32, (94.0 * S) as i32);
        }
        let _ = GdipDeletePen(pen);

        // ---- 余额 / Coding Plan 行 ----
        let mut info = String::new();
        if !d.balance_line.is_empty() {
            info.push_str(&d.balance_line);
        }
        if !d.coding_line.is_empty() {
            if !info.is_empty() {
                info.push_str("  ·  ");
            }
            info.push_str(&d.coding_line);
        }
        if info.is_empty() {
            info = if d.paused {
                "服务已停止".into()
            } else {
                "实时监控中".into()
            };
        }
        let info = fit_text(g, env.font_body, &info, W - 2.0 * PAD * S);
        let info_color = if d.paused {
            rgb(0xEF, 0x44, 0x44)
        } else {
            pal.muted
        };
        text(g, env.font_body, PAD * S, H - 22.0 * S, &info, info_color, env.fmt_near);
    }

    /// 构造圆角矩形路径（4 段弧）。
    unsafe fn round_path(path: *mut GpPath, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let r = r.min(w / 2.0).min(h / 2.0);
        let d = 2.0 * r;
        let _ = GdipAddPathArcI(path, x as i32, y as i32, d as i32, d as i32, 180.0, 90.0); // 左上
        let _ = GdipAddPathArcI(path, (x + w - d) as i32, y as i32, d as i32, d as i32, 270.0, 90.0); // 右上
        let _ = GdipAddPathArcI(path, (x + w - d) as i32, (y + h - d) as i32, d as i32, d as i32, 0.0, 90.0); // 右下
        let _ = GdipAddPathArcI(path, x as i32, (y + h - d) as i32, d as i32, d as i32, 90.0, 90.0); // 左下
        let _ = GdipClosePathFigure(path);
    }

    unsafe fn fill_round(g: *mut GpGraphics, x: f32, y: f32, w: f32, h: f32, r: f32, color: u32) {
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(color, &mut brush);
        let mut path: *mut GpPath = std::ptr::null_mut();
        let _ = GdipCreatePath(FillModeAlternate, &mut path);
        round_path(path, x, y, w, h, r);
        let _ = GdipFillPath(g, brush as *mut GpBrush, path);
        let _ = GdipDeletePath(path);
        let _ = GdipDeleteBrush(brush as *mut GpBrush);
    }

    unsafe fn text(
        g: *mut GpGraphics,
        font: *mut GpFont,
        x: f32,
        y: f32,
        s: &str,
        color: u32,
        fmt: *mut GpStringFormat,
    ) {
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(color, &mut brush);
        let ws = w(s);
        let rect = RectF {
            X: x,
            Y: y,
            Width: 4000.0,
            Height: 4000.0,
        };
        let _ = GdipDrawString(g, ws.as_ptr(), -1, font, &rect, fmt, brush as *mut GpBrush);
        let _ = GdipDeleteBrush(brush as *mut GpBrush);
    }

    unsafe fn text_center(
        g: *mut GpGraphics,
        env: &GdiEnv,
        font: *mut GpFont,
        x: f32,
        y: f32,
        width: f32,
        h: f32,
        s: &str,
        color: u32,
    ) {
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(color, &mut brush);
        let ws = w(s);
        let rect = RectF {
            X: x,
            Y: y,
            Width: width,
            Height: h,
        };
        let _ = GdipDrawString(g, ws.as_ptr(), -1, font, &rect, env.fmt_center, brush as *mut GpBrush);
        let _ = GdipDeleteBrush(brush as *mut GpBrush);
    }

    unsafe fn measure(g: *mut GpGraphics, font: *mut GpFont, s: &str) -> f32 {
        let ws = w(s);
        let rect = RectF {
            X: 0.0,
            Y: 0.0,
            Width: 4000.0,
            Height: 4000.0,
        };
        let mut bbox: RectF = std::mem::zeroed();
        let _ = GdipMeasureString(
            g,
            ws.as_ptr(),
            -1,
            font,
            &rect,
            std::ptr::null(),
            &mut bbox,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        bbox.Width
    }

    /// 超宽截断 + "…"（保证不溢出徽章/行宽）。
    /// 数值排版：整数部分 20 号粗体，小数+单位 11 号小字（基线视觉对齐，
    /// 整体在列内水平居中）；整体超宽时降级为单字号截断，绝不溢出。
    unsafe fn draw_stat(
        g: *mut GpGraphics,
        env: &GdiEnv,
        cx: f32,
        y: f32,
        big: &str,
        small: &str,
        color: u32,
        max_w: f32,
    ) {
        if small.is_empty() {
            let big = fit_text(g, env.font_num, big, max_w);
            text_center(g, env, env.font_num, cx - max_w / 2.0, y, max_w, 26.0, &big, color);
            return;
        }
        let w1 = measure(g, env.font_num, big);
        let w2 = measure(g, env.font_body, small);
        if w1 + w2 <= max_w {
            let x0 = cx - (w1 + w2) / 2.0;
            text(g, env.font_num, x0, y, big, color, env.fmt_near);
            // 小字下移与大字基线对齐（20 号与 11 号字面高差）
            text(g, env.font_body, x0 + w1, y + 7.0, small, color, env.fmt_near);
        } else {
            // 超宽降级：合并成小字单行截断
            let merged = format!("{big}{small}");
            let merged = fit_text(g, env.font_body, &merged, max_w);
            text_center(g, env, env.font_body, cx - max_w / 2.0, y + 6.0, max_w, 20.0, &merged, color);
        }
    }

    unsafe fn fit_text(g: *mut GpGraphics, font: *mut GpFont, s: &str, max_w: f32) -> String {
        if max_w <= 0.0 {
            return "…".to_string();
        }
        if measure(g, font, s) <= max_w {
            return s.to_string();
        }
        let ell = "…";
        let chars: Vec<char> = s.chars().collect();
        let mut lo: usize = 0;
        let mut hi: usize = chars.len();
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let t: String = chars[..mid].iter().collect::<String>() + ell;
            if measure(g, font, &t) <= max_w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        if lo == 0 {
            ell.to_string()
        } else {
            let prefix: String = chars[..lo].iter().collect();
            format!("{prefix}{ell}")
        }
    }

    /// 用量数值拆两部分显示：`(大字整数部分, 小字小数+单位)`。
    /// >=1亿 用亿、>=1万 用万（保留 `decimals` 位小数，四舍五入、去尾零），
    /// 万以下原样整数。例如 (12_345, 3) -> ("1", ".234万")；(10_000, 3) -> ("1", "万")；
    /// (999, 3) -> ("999", "")。小数+单位用小号字排版，数值再大也不会溢出列宽。
    fn fmt_parts(n: u64, decimals: u32) -> (String, String) {
        let (v, unit) = if n >= 100_000_000 {
            (n as f64 / 1e8, "亿")
        } else if n >= 10_000 {
            (n as f64 / 1e4, "万")
        } else {
            return (n.to_string(), String::new());
        };
        let mul = 10u64.pow(decimals);
        let scaled = (v * mul as f64).round() as u64;
        let int = scaled / mul;
        // 小数部分定宽后去尾零（全零则只剩单位）
        let frac = (scaled % mul).to_string();
        let frac = format!("{frac:0>width$}", width = decimals as usize);
        let frac = frac.trim_end_matches('0');
        let small = if frac.is_empty() {
            unit.to_string()
        } else {
            format!(".{frac}{unit}")
        };
        (int.to_string(), small)
    }

    /// 上下文 -> `[1m]`/`[200k]`（与前端一致）。
    fn fmt_ctx(ctx: u64) -> String {
        if ctx % 1_000_000 == 0 {
            format!("{}m", ctx / 1_000_000)
        } else if ctx % 1_000 == 0 {
            format!("{}k", ctx / 1_000)
        } else {
            format!("{}k", (ctx as f64 / 1000.0).round() as u64)
        }
    }

    /// 切换卡片显隐（托盘菜单调用，跑在托盘线程）。
    pub fn toggle_visible(shared: &Arc<WidgetShared>) {
        let hwnd = shared.hwnd.load(Ordering::SeqCst);
        if hwnd == 0 {
            return;
        }
        let vis = is_visible_impl(hwnd);
        unsafe {
            ShowWindow(hwnd as _, if vis { SW_HIDE } else { SW_SHOWNOACTIVATE });
            if !vis {
                // 显示后强制同步重绘：隐藏期间表面位可能失效/含屏幕垃圾
                //（WM_ERASEBKGND 被跳过，不清就会「页面当背景」）。
                InvalidateRect(hwnd as _, std::ptr::null(), 1);
                UpdateWindow(hwnd as _);
            }
        }
        save_pos(hwnd as _, &shared.path, !vis, *shared.theme.lock().unwrap());
    }

    fn is_visible_impl(hwnd: isize) -> bool {
        let style = unsafe { GetWindowLongPtrW(hwnd as _, GWL_STYLE) };
        (style & (WS_VISIBLE as isize)) != 0
    }

    pub fn is_visible(shared: &Arc<WidgetShared>) -> bool {
        let hwnd = shared.hwnd.load(Ordering::SeqCst);
        if hwnd == 0 {
            return false;
        }
        is_visible_impl(hwnd)
    }

    /// 卡片内循环切换「当前使用模型」（⇄ 按钮）：按配置顺序取下一个模型，
    /// 经 `AppState::set_default_model_blocking` 写盘 + 热重载（D25 同机制，托盘线程阻塞安全）。
    fn cycle_model(shared: &WidgetShared) {
        let state = &shared.state;
        // 收集全部完整别名（按配置顺序）
        let fulls: Vec<String> = {
            let cfg = state.cfg.blocking_read();
            let sep = if cfg.server.separator.is_empty() {
                "-"
            } else {
                &cfg.server.separator
            };
            let mut v = Vec::new();
            for p in &cfg.providers {
                for m in &p.models {
                    v.push(format!("{}{}{}", p.id, sep, m.alias));
                }
            }
            v
        };
        if fulls.is_empty() {
            return;
        }
        let cur = {
            let cfg = state.cfg.blocking_read();
            let mut c = cfg.server.default_model.clone();
            if let Some(pos) = c.find('[') {
                c.truncate(pos);
            }
            c
        };
        let idx = fulls.iter().position(|f| f == &cur).unwrap_or(fulls.len());
        let next = fulls[(idx + 1) % fulls.len()].clone();
        match state.set_default_model_blocking(&next) {
            Ok(_) => {
                eprintln!("[card] 切换当前使用模型 → {next}");
                // 立即刷新快照（数据任务下一轮 5s 也会刷，这里即时生效）
                let snapshot = {
                    let cfg = state.cfg.blocking_read();
                    let usage = state.usage.lock().unwrap().clone();
                    let paused = shared.data.lock().unwrap().paused;
                    compute_snapshot(&cfg, &usage, paused)
                };
                *shared.data.lock().unwrap() = snapshot;
                let hwnd = shared.hwnd.load(Ordering::SeqCst);
                if hwnd != 0 {
                    unsafe { PostMessageW(hwnd as _, WM_DATA_UPDATED, 0, 0) };
                }
            }
            Err(e) => eprintln!("[card] 切换模型失败: {e}"),
        }
    }

    /// 卡片内切换暗黑/明亮主题（☀/☾ 按钮）：翻转 theme → 持久化 → 重绘。
    fn toggle_theme(hwnd: windows_sys::Win32::Foundation::HWND, shared: &WidgetShared) {
        let new = {
            let mut t = shared.theme.lock().unwrap();
            *t = match *t {
                Theme::Dark => Theme::Light,
                Theme::Light => Theme::Dark,
            };
            *t
        };
        save_pos(hwnd, &shared.path, is_visible_impl(hwnd as isize), new);
        eprintln!(
            "[card] 主题 → {}",
            if new == Theme::Dark { "暗黑" } else { "明亮" }
        );
        unsafe {
            InvalidateRect(hwnd, std::ptr::null(), 0);
            UpdateWindow(hwnd);
        }
    }

    /// main 里 tokio 数据任务：按 `server.card_refresh_secs` 周期（默认 3s，可在配置页改）
    /// 读内存快照；余额 / Coding Plan 每 ≈30s 拉一次（轮数按刷新周期折算）；
    /// **仅数据变化才 PostMessageW 通知重绘**（空闲零重绘）。
    pub fn spawn_data_task(
        state: Arc<AppState>,
        shared: Arc<WidgetShared>,
        base_url: String,
        paused: tokio::sync::watch::Receiver<bool>,
    ) {
        let _ = base_url; // base_url 已在 WidgetShared 中，此处参数保留与跨平台签名一致
        tokio::spawn(async move {
            let mut tick = 0u32;
            let mut last: Option<WidgetData> = None;
            loop {
                tick = tick.wrapping_add(1);
                // 每轮现读配置：刷新周期改了下一轮即生效（无需重启）
                let refresh = {
                    let cfg = state.cfg.read().await;
                    cfg.server.card_refresh_secs.clamp(1, 60)
                };
                // 余额/Coding Plan ≈30s 拉一次（首轮即拉）；服务停止时跳过（自调会失败）
                let balance_every = ((30.0 / refresh as f32).ceil() as u32).max(1);
                let snapshot = {
                    let cfg = state.cfg.read().await;
                    let usage = state.usage.lock().unwrap().clone();
                    let paused_now = *paused.borrow();
                    compute_snapshot(&cfg, &usage, paused_now)
                };
                let (bal, coding) = if tick % balance_every == 1 && !snapshot.paused {
                    fetch_balances(&state, &shared.base_url, &snapshot.provider_id).await
                } else {
                    (String::new(), String::new())
                };
                let changed = {
                    let mut d = shared.data.lock().unwrap();
                    if d.provider_id != snapshot.provider_id {
                        // 切换模型/服务商：清掉旧服务商的余额/Coding Plan，防止串卡
                        d.balance_line.clear();
                        d.coding_line.clear();
                    }
                    d.provider_id = snapshot.provider_id.clone();
                    d.provider = snapshot.provider.clone();
                    d.alias = snapshot.alias.clone();
                    d.real = snapshot.real.clone();
                    d.ctx = snapshot.ctx;
                    d.today_requests = snapshot.today_requests;
                    d.today_input = snapshot.today_input;
                    d.today_output = snapshot.today_output;
                    d.paused = snapshot.paused;
                    if !bal.is_empty() {
                        d.balance_line = bal;
                    }
                    if !coding.is_empty() {
                        d.coding_line = coding;
                    }
                    let now = d.clone();
                    match &last {
                        Some(prev) => now != *prev,
                        None => true,
                    }
                };
                if changed {
                    last = Some(shared.data.lock().unwrap().clone());
                    let hwnd = shared.hwnd.load(Ordering::SeqCst);
                    if hwnd != 0 {
                        unsafe { PostMessageW(hwnd as _, WM_DATA_UPDATED, 0, 0) };
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(refresh)).await;
            }
        });
    }

    /// 从 config + usage 计算快照（纯函数，可单测）。
    fn compute_snapshot(
        cfg: &crate::config::Config,
        usage: &crate::config::UsageStats,
        paused: bool,
    ) -> WidgetData {
        let mut wd = WidgetData {
            paused,
            ..Default::default()
        };
        let sep = if cfg.server.separator.is_empty() {
            "-"
        } else {
            &cfg.server.separator
        };
        let mut full = cfg.server.default_model.clone();
        if let Some(pos) = full.find('[') {
            full.truncate(pos);
        }

        let mut found = false;
        // Phase 57：当前模型所属服务商 hide_usage 时，卡片不显示其用量/余额
        //（余额/Coding Plan 走 /api/balance、/api/coding-plan，服务端已过滤；这里只管今日用量）
        let mut usage_hidden = false;
        for p in &cfg.providers {
            for m in &p.models {
                let alias_full = format!("{}{}{}", p.id, sep, m.alias);
                if alias_full == full {
                    wd.provider = display_name(p);
                    wd.provider_id = p.id.clone();
                    wd.real = m.real.clone();
                    wd.ctx = m.context;
                    usage_hidden = p.hide_usage;
                    found = true;
                    break;
                }
            }
            if found {
                break;
            }
        }
        if !found {
            if let Some(p) = cfg.providers.first() {
                wd.provider = display_name(p);
                wd.provider_id = p.id.clone();
                usage_hidden = p.hide_usage;
                if full.is_empty() {
                    if let Some(m) = p.models.first() {
                        full = format!("{}{}{}", p.id, sep, m.alias);
                        wd.real = m.real.clone();
                        wd.ctx = m.context;
                    }
                } else if let Some(m) = p.models.first() {
                    // default_model 非空但未匹配：保留别名，仍取该服务商首个模型的真实名/上下文作参考
                    wd.real = m.real.clone();
                    wd.ctx = m.context;
                }
            }
        }
        wd.alias = full;

        // 今日用量（本地日零点天桶）+ 近 24h 小时桶（Phase 57：hide_usage 的服务商跳过）
        let today = crate::config::day_start_with_offset(
            crate::config::unix_now(),
            crate::config::local_utc_offset_secs(),
        );
        if !usage_hidden {
            for pu in usage.providers.values() {
                if let Some(mu) = pu.by_model.get(&wd.alias) {
                    for day in &mu.time.days {
                        if day.ts == today {
                            wd.today_requests = day.requests;
                            wd.today_input = day.input_tokens;
                            wd.today_output = day.output_tokens;
                            break;
                        }
                    }
                    break;
                }
            }
        }
        wd
    }

    /// 请求自身余额（DeepSeek）与 Coding Plan 摘要行（只读 renew=0，不写盘）。
    /// 只取「当前使用模型所属服务商」的数据，其余服务商一律跳过（卡片只显示当前模型）。
    async fn fetch_balances(state: &AppState, base_url: &str, provider_id: &str) -> (String, String) {
        let mut bal = String::new();
        let mut coding = String::new();
        // 余额
        if let Ok(r) = state
            .client
            .get(format!("{base_url}/api/balance"))
            .send()
            .await
        {
            if let Ok(j) = r.json::<serde_json::Value>().await {
                if let Some(arr) = j.get("balances").and_then(|x| x.as_array()) {
                    for b in arr {
                        let pid = b.get("provider").and_then(|x| x.as_str()).unwrap_or("");
                        if !pid.is_empty() && pid != provider_id {
                            continue; // 只显示当前模型所属服务商
                        }
                        if let Some(infos) = b.get("balance_infos").and_then(|x| x.as_array()) {
                            for i in infos {
                                let cur = i.get("currency").and_then(|x| x.as_str()).unwrap_or("");
                                let tot = i
                                    .get("total_balance")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("");
                                if !tot.is_empty() {
                                    let cur = if cur.is_empty() { "¥" } else { cur };
                                    bal = format!("余额 {cur} {tot}");
                                }
                            }
                        }
                    }
                }
            }
        }
        // Coding Plan：ark/oc -> levels（5h/周/月）；xf -> plans（包月百分比）。
        if let Ok(r) = state
            .client
            .get(format!("{base_url}/api/coding-plan?renew=0"))
            .send()
            .await
        {
            if let Ok(j) = r.json::<serde_json::Value>().await {
                if let Some(arr) = j.get("coding_plans").and_then(|x| x.as_array()) {
                    for c in arr {
                        let pid = c.get("provider").and_then(|x| x.as_str()).unwrap_or("");
                        if !pid.is_empty() && pid != provider_id {
                            continue; // 只显示当前模型所属服务商
                        }
                        let ok = c.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
                        if !ok {
                            continue;
                        }
                        // ark / oc：levels
                        if let Some(levels) = c.get("levels").and_then(|x| x.as_array()) {
                            let mut parts: Vec<String> = Vec::new();
                            for l in levels {
                                let label = l.get("label").and_then(|x| x.as_str()).unwrap_or("");
                                let pct = l.get("percent").and_then(|x| x.as_f64()).unwrap_or(0.0);
                                let short = match label {
                                    "5小时" => "5h",
                                    "一周" => "周",
                                    "一月" => "月",
                                    other => other,
                                };
                                parts.push(format!("{short} {pct:.2}%"));
                            }
                            if !parts.is_empty() {
                                coding = parts.join(" · ");
                            }
                        }
                        // xf：plans（取首个套餐包月百分比）
                        if coding.is_empty() {
                            if let Some(plans) = c.get("plans").and_then(|x| x.as_array()) {
                                if let Some(p0) = plans.first() {
                                    let pct =
                                        p0.get("percent").and_then(|x| x.as_f64()).unwrap_or(0.0);
                                    coding = format!("Coding Plan {pct:.2}%");
                                }
                            }
                        }
                        if !coding.is_empty() {
                            break;
                        }
                    }
                }
            }
        }
        (bal, coding)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::config::{
            Config, ModelEntry, ProviderConfig, ProviderUsage, TimeBucket, UsageStats,
        };

        fn prov(id: &str, name: &str, alias: &str, real: &str, ctx: u64) -> ProviderConfig {
            ProviderConfig {
                id: id.into(),
                name: name.into(),
                base_url: "https://x.test/anthropic".into(),
                api_key: "k".into(),
                auth: "bearer".into(),
                models: vec![ModelEntry {
                    alias: alias.into(),
                    real: real.into(),
                    context: ctx,
                    protocol: None,
                }],
                strip_beta: false,
                rewrite_model: true,
                protocol: "anthropic".into(),
                console_cookie: String::new(),
                console_usage_url: String::new(),
                hide_usage: false,
            }
        }

        #[test]
        fn fmt_parts_units() {
            // 万以下原样
            assert_eq!(fmt_parts(0, 3), ("0".to_string(), "".to_string()));
            assert_eq!(fmt_parts(999, 3), ("999".to_string(), "".to_string()));
            // 万/亿 + 三位小数四舍五入去尾零
            assert_eq!(fmt_parts(10_000, 3), ("1".to_string(), "万".to_string()));
            assert_eq!(fmt_parts(12_300, 3), ("1".to_string(), ".23万".to_string()));
            assert_eq!(fmt_parts(12_345, 3), ("1".to_string(), ".235万".to_string()));
            assert_eq!(fmt_parts(123_456_789, 3), ("1".to_string(), ".235亿".to_string()));
            assert_eq!(fmt_parts(1_000_000_000, 3), ("10".to_string(), "亿".to_string()));
            // 次数只留 1 位小数
            assert_eq!(fmt_parts(123_456, 1), ("12".to_string(), ".3万".to_string()));
        }

        #[test]
        fn fmt_ctx_units() {
            assert_eq!(fmt_ctx(1_000_000), "1m");
            assert_eq!(fmt_ctx(200_000), "200k");
            assert_eq!(fmt_ctx(128_000), "128k");
            assert_eq!(fmt_ctx(65_536), "66k");
        }

        #[test]
        fn provider_color_matches() {
            assert_eq!(provider_color("DeepSeek"), rgb(0x25, 0x63, 0xEB));
            assert_eq!(provider_color("讯飞星辰"), rgb(0xEF, 0x44, 0x44));
            assert_eq!(provider_color("火山方舟"), rgb(0xF9, 0x73, 0x16));
            assert_eq!(provider_color("unknown"), rgb(0x16, 0xA3, 0x4A));
        }

        #[test]
        fn snapshot_strips_ctx_suffix_and_matches_model() {
            let mut cfg = Config::default();
            cfg.server.default_model = "ds-v4-flash[1m]".into();
            cfg.providers = vec![prov("ds", "DeepSeek", "v4-flash", "deepseek-v4-flash", 1_000_000)];
            let wd = compute_snapshot(&cfg, &UsageStats::default(), false);
            assert_eq!(wd.alias, "ds-v4-flash"); // 后缀已剥
            assert_eq!(wd.provider, "DeepSeek");
            assert_eq!(wd.provider_id, "ds"); // 只显示当前模型所属服务商
            assert_eq!(wd.real, "deepseek-v4-flash");
            assert_eq!(wd.ctx, 1_000_000);
            assert!(!wd.paused);
        }

        #[test]
        fn snapshot_falls_back_to_first_when_default_empty() {
            let mut cfg = Config::default();
            cfg.server.default_model = String::new();
            cfg.providers = vec![prov("ds", "DeepSeek", "chat", "deepseek-chat", 200_000)];
            let wd = compute_snapshot(&cfg, &UsageStats::default(), false);
            assert_eq!(wd.alias, "ds-chat");
            assert_eq!(wd.provider, "DeepSeek");
            assert_eq!(wd.provider_id, "ds"); // 只显示当前模型所属服务商
            assert_eq!(wd.ctx, 200_000);
        }

        #[test]
        fn snapshot_today_bucket() {
            let mut cfg = Config::default();
            cfg.server.default_model = "ds-chat".into();
            cfg.providers = vec![prov("ds", "DeepSeek", "chat", "deepseek-chat", 200_000)];

            let now = crate::config::unix_now();
            let today = crate::config::day_start_with_offset(now, crate::config::local_utc_offset_secs());

            let mut u = UsageStats::default();
            let pu = u.providers.entry("ds".into()).or_default();
            let mu = pu.by_model.entry("ds-chat".into()).or_default();
            mu.time.days.push(TimeBucket {
                ts: today,
                requests: 7,
                input_tokens: 1000,
                output_tokens: 500,
            });
            let wd = compute_snapshot(&cfg, &u, false);
            assert_eq!(wd.today_requests, 7);
            assert_eq!(wd.today_input, 1000);
            assert_eq!(wd.today_output, 500);
        }

        #[test]
        fn snapshot_paused_flag() {
            let cfg = Config::default();
            let wd = compute_snapshot(&cfg, &UsageStats::default(), true);
            assert!(wd.paused);
        }

        #[test]
        fn snapshot_hide_usage_zeroes_today() {
            // Phase 57：当前模型所属服务商 hide_usage=true → 卡片今日用量不显示（其余字段照常）
            let mut cfg = Config::default();
            cfg.server.default_model = "ds-chat".into();
            let mut p = prov("ds", "DeepSeek", "chat", "deepseek-chat", 200_000);
            p.hide_usage = true;
            cfg.providers = vec![p];

            let now = crate::config::unix_now();
            let today = crate::config::day_start_with_offset(now, crate::config::local_utc_offset_secs());
            let mut u = UsageStats::default();
            let pu = u.providers.entry("ds".into()).or_default();
            let mu = pu.by_model.entry("ds-chat".into()).or_default();
            mu.time.days.push(TimeBucket { ts: today, requests: 7, input_tokens: 1000, output_tokens: 500 });

            let wd = compute_snapshot(&cfg, &u, false);
            assert_eq!(wd.provider, "DeepSeek"); // 服务商名照常显示
            assert_eq!(wd.today_requests, 0, "hide_usage 时今日用量不显示");
            assert_eq!(wd.today_input, 0);
            assert_eq!(wd.today_output, 0);
        }

        #[test]
        fn card_state_roundtrip() {
            let tmp = std::env::temp_dir().join(format!("relay_card_{}.json", std::process::id()));
            let _ = std::fs::remove_file(&tmp);
            let s = CardState {
                x: 120,
                y: 80,
                visible: false,
                theme: Theme::Dark,
            };
            s.save(&tmp);
            let (loaded, existed) = CardState::load(&tmp);
            assert!(existed);
            assert_eq!(loaded.x, 120);
            assert_eq!(loaded.y, 80);
            assert!(!loaded.visible);
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_file(tmp.with_extension("tmp"));
        }

        // 抑制未使用告警（ProviderUsage 等用于构造测试数据）
        #[test]
        fn _provider_usage_constructible() {
            let _ = ProviderUsage::default();
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::{create_window, is_visible, spawn_data_task, toggle_visible};

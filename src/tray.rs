//! 系统托盘图标（仅 Windows）：右键菜单「设置」/「模型」/「停止|启动」/「退出」。
//!
//! 非 Windows 平台为 no-op（空实现）。「退出」直接 `process::exit(0)`。
//!
//! 关键点（Phase 16 修复）：
//!   1. tray-icon 0.20 无内置默认图标，必须 `.with_icon()` 提供合法 HICON，
//!      否则 build 直接 Err、托盘不出现 → 这里用 `Icon::from_rgba` 程序化生成。
//!   2. Windows 下托盘隐藏窗口需要**消息泵**（GetMessageW/DispatchMessageW）处理
//!      WM_TRAYICON（图标事件）/WM_COMMAND（菜单事件）；旧代码线程阻塞在
//!      `MenuEvent::receiver().recv()`、从不泵消息，导致图标不显示、菜单无响应。
//!
//! Phase 18：停止语义改为**释放端口停掉整个服务**。`paused` 是
//! `tokio::sync::watch::Sender<bool>`（共享给 main 的 run_server 循环）：
//! 点「停止」→ `send(true)` → 服务循环 drop serve、端口关闭（浏览器显示服务不存在）；
//! 点「启动」→ `send(false)` → 服务循环重新 bind 恢复。
//!
//! Phase 25：「模型」子菜单（模型 → 服务商 → 模型）选中即设「当前使用模型」——
//! 经 `AppState::set_default_model_blocking` 写盘 + 热重载（D22 哨兵路由：仅当 CC
//! 发 `claude-relay-model` 哨兵时用它；CC 指定具体模型则继续走指定）。

#[cfg(target_os = "windows")]
mod imp {
    use std::process::Command;
    use std::sync::Arc;
    use tokio::sync::watch;
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, Submenu};
    use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

    use crate::config::AppState;

    /// 启动托盘线程（独立 std 线程，与 tokio 单线程 runtime 互不干扰）。
    /// `paused` 为运行/停止状态的 watch Sender：true=停止（服务循环释放端口）。
    ///
    /// `state` 供「模型」菜单设当前使用模型（`set_default_model_blocking`，写盘+热重载）；
    /// `model_menu` 是配置快照 `(服务商显示名, [(完整别名, 别名)])`，`current` 为当前完整别名
    /// （勾选展示用）。菜单在启动时构建一次；Web UI 改配置后需重启托盘刷新（已文档化）。
    ///
    /// Phase 55：`card_shared` 供「桌面卡片」——在本线程创建卡片窗口
    /// （GetMessageW 消息泵调度本线程所有窗口），右键菜单「桌面卡片」切换显隐。
    pub fn spawn(
        base_url: String,
        paused: watch::Sender<bool>,
        state: Arc<AppState>,
        model_menu: Vec<(String, Vec<(String, String)>)>,
        current: String,
        card_shared: std::sync::Arc<crate::widget::WidgetShared>,
    ) {
        std::thread::spawn(move || {
            let settings = MenuItem::new("设置", true, None);
            // Phase 42/43：把 Web UI 顶部两个按钮搬到托盘——写 ~/.claude/settings.json
            // （当前配置快照 → 哨兵 model），服务常驻隐藏时也能一键更新 claude 配置。
            // Phase 43：收纳进「更新」子菜单（鼠标移入展开，与「模型」子菜单同交互）。
            let update_claude = MenuItem::new("更新 claude 配置", true, None);
            let update_env = MenuItem::new("更新环境变量", true, None);
            let update_root = Submenu::new("更新", true);
            if update_root.append(&update_claude).is_err() || update_root.append(&update_env).is_err() {
                eprintln!("[tray] 更新子菜单追加失败");
                return;
            }
            let toggle = MenuItem::new(
                if *paused.borrow() { "启动" } else { "停止" },
                true,
                None,
            );
            // Phase 55：桌面卡片显隐开关（勾选态 = 显示）
            let card_item = CheckMenuItem::new("桌面卡片", true, false, None);
            let quit = MenuItem::new("退出", true, None);

            // Phase 25：「模型」子菜单 —— 模型 → 服务商 → 模型，选中即热重载「当前使用模型」。
            let model_root = Submenu::new("模型", true);
            let mut model_items: Vec<(CheckMenuItem, String)> = Vec::new();
            if model_menu.is_empty() {
                let _ = model_root.append(&MenuItem::new("（未配置服务商）", false, None));
            }
            for (pname, ms) in &model_menu {
                let psub = Submenu::with_id(
                    MenuId::new(format!("model-p:{pname}")),
                    pname.clone(),
                    true,
                );
                for (full, alias) in ms {
                    let mi = CheckMenuItem::with_id(
                        MenuId::new(format!("model-m:{full}")),
                        alias.clone(),
                        true,
                        full == &current,
                        None,
                    );
                    if psub.append(&mi).is_err() {
                        eprintln!("[tray] 模型菜单项追加失败");
                        return;
                    }
                    model_items.push((mi, full.clone()));
                }
                if model_root.append(&psub).is_err() {
                    eprintln!("[tray] 服务商子菜单追加失败");
                    return;
                }
            }

            let menu = Menu::new();
            if menu.append(&settings).is_err()
                || menu.append(&update_root).is_err()
                || menu.append(&model_root).is_err()
                || menu.append(&card_item).is_err()
                || menu.append(&toggle).is_err()
                || menu.append(&quit).is_err()
            {
                eprintln!("[tray] 菜单组装失败");
                return;
            }

            let icon = match make_icon() {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("[tray] 图标生成失败（不影响服务运行）: {e}");
                    return;
                }
            };
            let _tray = match TrayIconBuilder::new()
                .with_tooltip("ClaudeRelay — 多服务商模型中转")
                .with_icon(icon)
                .with_menu(Box::new(menu))
                .build()
            {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[tray] 托盘图标创建失败（不影响服务运行）: {e}");
                    return;
                }
            };
            eprintln!("[tray] 托盘已就绪：右键=设置/更新(子菜单)/模型/停止|启动/退出，双击=打开配置页");

            // Phase 55：桌面卡片窗口建在本线程（消息泵调度），菜单勾选态与初始显隐同步。
            crate::widget::create_window(&card_shared);
            let _ = card_item.set_checked(crate::widget::is_visible(&card_shared));

            // Windows 消息泵：GetMessageW 阻塞等消息（空闲零 CPU），
            // DispatchMessageW 触发托盘窗口 WndProc → 菜单/图标事件入队 → try_recv 消费。
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, GetMessageW, TranslateMessage, MSG,
            };
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            unsafe {
                loop {
                    if GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) <= 0 {
                        break;
                    }
                    TranslateMessage(&mut msg);
                    DispatchMessageW(&mut msg);

                    while let Ok(ev) = MenuEvent::receiver().try_recv() {
                        let id = ev.id();
                        if id == settings.id() {
                            if let Err(e) = open_browser(&base_url) {
                                eprintln!("[tray] 打开浏览器失败: {e}");
                            }
                        } else if id == update_claude.id() || id == update_env.id() {
                            // Phase 42：与 Web UI 顶部两按钮同语义——当前配置快照写 claude settings
                            // （顶层 model + env 6 键 = 哨兵值），服务隐藏运行时用气泡反馈结果。
                            let is_env = id == update_env.id();
                            let label = if is_env { "环境变量" } else { "claude 配置" };
                            match crate::webui::update_settings_blocking(&state) {
                                Ok((base_url, model, path)) => {
                                    eprintln!("[tray] 已更新{label} → {path}（{base_url} / {model}）");
                                    balloon("ClaudeRelay", &format!(
                                        "已更新{label}：哨兵模型 {model}\n{path}"
                                    ));
                                }
                                Err(e) => {
                                    eprintln!("[tray] 更新{label}失败: {e}");
                                    balloon("ClaudeRelay", &format!("更新{label}失败：{e}"));
                                }
                            }
                        } else if let Some(full) = model_items
                            .iter()
                            .find(|(mi, _)| mi.id() == &id)
                            .map(|(_, f)| f.clone())
                        {
                            // Phase 25：选中模型 → 写盘 + 热重载（D22：仅 CC 发哨兵时生效）
                            match state.set_default_model_blocking(&full) {
                                Ok(_) => {
                                    for (mi, f) in &model_items {
                                        mi.set_checked(f == &full);
                                    }
                                    eprintln!(
                                        "[tray] 当前使用模型 → {full}（CC 发 claude-relay-model 哨兵时生效，热重载即时；CC 若指定具体模型则继续走指定）"
                                    );
                                }
                                Err(e) => eprintln!("[tray] 设置模型失败: {e}"),
                            }
                        } else if id == card_item.id() {
                            // Phase 55：切换桌面卡片显隐 + 同步勾选态
                            crate::widget::toggle_visible(&card_shared);
                            let vis = crate::widget::is_visible(&card_shared);
                            let _ = card_item.set_checked(vis);
                            eprintln!(
                                "[tray] 桌面卡片{}",
                                if vis { "显示" } else { "隐藏" }
                            );
                        } else if id == toggle.id() {
                            let now_paused = !*paused.borrow(); // 取反当前状态
                            let _ = paused.send(now_paused); // 通知 run_server 停止/恢复监听
                            let _ = toggle.set_text(if now_paused { "启动" } else { "停止" });
                            eprintln!("[tray] 服务已{}", if now_paused { "停止" } else { "启动" });
                        } else if id == quit.id() {
                            eprintln!("[tray] 退出");
                            std::process::exit(0);
                        }
                    }
                    while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                        if matches!(ev, TrayIconEvent::DoubleClick { .. }) {
                            if let Err(e) = open_browser(&base_url) {
                                eprintln!("[tray] 双击打开浏览器失败: {e}");
                            }
                        }
                    }
                }
            }
        });
    }

    pub fn open_browser(url: &str) -> std::io::Result<()> {
        // `cmd /c start "" <url>` 在 Windows 上最可靠（不依赖 PATH 里的浏览器）。
        Command::new("cmd").args(["/c", "start", "", url]).spawn().map(|_| ())
    }

    /// 气泡用的隐藏消息窗口（hWnd 承载 `Shell_NotifyIconW` 通知）。托盘线程的消息泵
    /// （GetMessageW/DispatchMessageW）会调度本线程所有窗口消息，此窗口无需任何处理逻辑
    /// （WndProc 直接用 DefWindowProcW）。用消息专用窗口（HWND_MESSAGE），不占任务栏。
    static BALLOON_HWND: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
    fn balloon_window() -> isize {
        *BALLOON_HWND.get_or_init(|| unsafe {
            use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, HWND_MESSAGE, RegisterClassW, WNDCLASSW,
                WS_OVERLAPPED,
            };
            let class_name: Vec<u16> = "ClaudeRelayBalloon"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: GetModuleHandleW(std::ptr::null()),
                lpszClassName: class_name.as_ptr(),
                ..std::mem::zeroed()
            };
            // 重复注册返回 CLASS_EXISTS，无妨
            let _ = RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                std::ptr::null(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            hwnd as isize
        })
    }

    /// 托盘信息气泡（Phase 42）：`Shell_NotifyIconW` + `NIF_INFO`——加一个**只显示气泡**
    /// 的临时通知（不设 NIF_ICON/NIF_MESSAGE/NIF_GUID，用经典 hWnd+uID 模式，全版本可靠），
    /// 气泡消失后临时图标自动移除。服务隐藏运行（--autostart 黑框隐藏）时 stderr 不可见，
    /// 气泡是唯一可见反馈（成功/失败都弹）。
    fn balloon(title: &str, body: &str) {
        use windows_sys::Win32::UI::Shell::{
            Shell_NotifyIconW, NOTIFYICONDATAW, NIF_INFO, NIIF_INFO, NIM_ADD,
        };
        let hwnd = balloon_window();
        if hwnd == 0 {
            eprintln!("[tray] 气泡窗口创建失败（不影响服务）");
            return;
        }
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd as _;
        nid.uID = 1;
        nid.uFlags = NIF_INFO; // 仅气泡：临时图标，气泡消失自动移除
        nid.dwInfoFlags = NIIF_INFO;
        put_wide(&mut nid.szInfoTitle, title);
        put_wide(&mut nid.szInfo, body);
        let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
        if ok == 0 {
            eprintln!(
                "[tray] 气泡发送失败（Shell_NotifyIconW error={}）",
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
            );
        }
    }

    /// 把 UTF-8 字符串写入宽字符缓冲区（截断并保证 NUL 结尾）。
    fn put_wide(buf: &mut [u16], s: &str) {
        let mut i = 0;
        for u in s.encode_utf16() {
            if i >= buf.len() - 1 {
                break;
            }
            buf[i] = u;
            i += 1;
        }
        buf[i] = 0;
    }

    /// 程序化生成 32×32 托盘图标：绿圆 + 白色闪电（无需外部 .ico 文件）。
    fn make_icon() -> Result<Icon, String> {
        const S: u32 = 32;
        const CX: i32 = 16;
        const CY: i32 = 16;
        const R: i32 = 14;
        // 白色闪电多边形（顶点坐标，32×32 画布内）
        let bolt: [(i32, i32); 6] = [(19, 5), (10, 17), (15, 17), (12, 27), (24, 12), (18, 12)];
        let mut rgba = vec![0u8; (S * S * 4) as usize];
        for y in 0..S {
            for x in 0..S {
                let i = ((y * S + x) * 4) as usize;
                let dx = x as i32 - CX;
                let dy = y as i32 - CY;
                if dx * dx + dy * dy <= R * R {
                    rgba[i] = 0x16;
                    rgba[i + 1] = 0xa3;
                    rgba[i + 2] = 0x4a;
                    rgba[i + 3] = 255;
                }
                if in_poly(x as i32, y as i32, &bolt) {
                    rgba[i] = 255;
                    rgba[i + 1] = 255;
                    rgba[i + 2] = 255;
                    rgba[i + 3] = 255;
                }
            }
        }
        Icon::from_rgba(rgba, S, S).map_err(|e| e.to_string())
    }

    /// 射线法：点是否落在多边形内（画图标用，i64 防溢出）。
    fn in_poly(px: i32, py: i32, pts: &[(i32, i32)]) -> bool {
        let mut inside = false;
        let mut j = pts.len() - 1;
        for i in 0..pts.len() {
            let (xi, yi) = pts[i];
            let (xj, yj) = pts[j];
            if ((yi > py) != (yj > py))
                && (px as i64) < ((xj - xi) as i64 * (py - yi) as i64) / (yj - yi) as i64 + xi as i64
            {
                inside = !inside;
            }
            j = i;
        }
        inside
    }
}

#[cfg(target_os = "windows")]
pub use imp::open_browser;
#[cfg(target_os = "windows")]
pub use imp::spawn;

#[cfg(not(target_os = "windows"))]
pub fn spawn(
    _base_url: String,
    _paused: tokio::sync::watch::Sender<bool>,
    _state: std::sync::Arc<crate::config::AppState>,
    _model_menu: Vec<(String, Vec<(String, String)>)>,
    _current: String,
    _card_shared: std::sync::Arc<crate::widget::WidgetShared>,
) {
    // 非 Windows：托盘为 no-op。
}

#[cfg(not(target_os = "windows"))]
pub fn open_browser(_url: &str) -> std::io::Result<()> {
    // 非 Windows：不打开浏览器（无对应实现）。
    Ok(())
}

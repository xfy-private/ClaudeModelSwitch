//! ClaudeRelay — Claude Code 多服务商多模型动态切换中转服务。
//!
//! 单进程单线程（tokio current_thread），单端口：
//!   - `/v1/messages`、`/v1/messages/count_tokens`：Anthropic 协议纯透传
//!   - `/v1/models`、`/healthz`、`/`、`/api/config`：辅助端点
//!
//! 用法：`claude-relay [--config config.json] [--host 127.0.0.1] [--port 8787] [--debug]`

mod api;
mod autostart;
mod commands;
mod config;
mod cookies;
mod gemini;
mod proxy;
mod router;
mod translate;
mod tray;
mod webui;
mod widget;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{watch, RwLock};

use crate::config::{AppState, Config};

const DEFAULT_CONFIG: &str = "config.json";

fn print_help() {
    println!(
        "ClaudeRelay v{} — Claude Code 多服务商多模型动态切换中转\n\
\n\
用法: claude-relay [选项]\n\
\n\
选项:\n\
  --config <path>   配置文件路径（默认 config.json）\n\
  --host <ip>       监听地址（默认 127.0.0.1）\n\
  --port <port>     监听端口（默认 8787）\n\
  --debug           打印请求/转发日志\n\
  --print-default-model\n\
                    打印写入 claude 的模型值（claude-relay-model[上下文]，供 start-claude.cmd 提示）后退出\n\
  --autostart       后台启动（开机自启/脚本启动）：不打开配置页；端口被占时静默退出\n\
  --restart         内部：/api/restart 自重启用——跳过端口占用检查（bind_with_retry 兜底）、不打开配置页\n\
  -h, --help        显示帮助\n\
\n\
示例:\n\
  claude-relay --debug\n\
  claude-relay --config my.json --port 9000\n",
        api::VERSION
    );
}

struct Args {
    config_path: Option<PathBuf>,
    host: Option<String>,
    port: Option<u16>,
    debug: bool,
    print_default_model: bool,
    autostart: bool,
    /// /api/restart 自重启用（Phase 20 修复）：跳过端口占用检查、不打开配置页。
    restart: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        config_path: None,
        host: None,
        port: None,
        debug: false,
        print_default_model: false,
        autostart: false,
        restart: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => a.config_path = args.next().map(PathBuf::from),
            "--host" => a.host = args.next(),
            "--port" => a.port = args.next().and_then(|s| s.parse().ok()),
            "--debug" => a.debug = true,
            "--print-default-model" => a.print_default_model = true,
            "--autostart" => a.autostart = true,
            "--restart" => a.restart = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}\n");
                print_help();
                std::process::exit(1);
            }
        }
    }
    a
}

/// 解析配置文件路径：显式 `--config` 优先；否则 CWD/config.json；
/// 不存在则回退 exe 同目录（开机自启时 CWD 是系统目录，也能找到配置）。
fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    let cwd_cfg = PathBuf::from(DEFAULT_CONFIG);
    if cwd_cfg.exists() {
        return cwd_cfg;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join(DEFAULT_CONFIG);
        }
    }
    cwd_cfg
}

/// Phase 27：用量持久化文件路径——配置文件同目录 sibling：`config.json` → `config-usage.json`。
fn usage_path_of(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "usage".to_string());
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(format!("{stem}-usage.json")),
        _ => PathBuf::from(format!("{stem}-usage.json")),
    }
}

/// 双击/Explorer 启动时隐藏黑色控制台窗口（Phase 19）。
/// 判定：`GetConsoleProcessList` 返回 1 = 独占该控制台（Explorer 双击、`start` 新建）
/// → 隐藏；返回 >1 = 共享父终端控制台（cmd/PowerShell/脚本）→ 保留日志输出。
///
/// 只 `ShowWindow(SW_HIDE)` 不 `FreeConsole`：stdout/stderr 句柄仍有效（写入隐藏
/// 控制台），后续 println/eprintln 不会因句柄失效而 panic。
/// 返回是否「双击启动」场景（独占控制台且被隐藏）。
#[cfg(target_os = "windows")]
fn hide_console_if_double_clicked() -> bool {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    let hwnd = unsafe { GetConsoleWindow() };
    if hwnd.is_null() {
        return false; // 无控制台（如 CREATE_NO_WINDOW 启动、e2e）
    }
    let mut list = [0u32; 8];
    let n = unsafe { GetConsoleProcessList(list.as_mut_ptr(), list.len() as u32) };
    if n == 1 {
        unsafe { ShowWindow(hwnd, SW_HIDE) };
        return true;
    }
    false
}

#[cfg(not(target_os = "windows"))]
fn hide_console_if_double_clicked() -> bool {
    false
}

/// Phase 62：把进程 stderr 重定向到 exe 同目录 `relay.log`（append）。
/// 背景：`--autostart` 隐藏控制台后 stderr 不可见，断流 `[warn]`（D61）、
/// 托盘告警等诊断信息全部丢失，排障无据。须早于任何 `eprintln!`/panic
/// 输出调用（std stderr 惰性取 `STD_ERROR_HANDLE`，main 首行换柄即全局生效）。
/// 文件 >5MB 时启动截断（防无限膨胀）。打开失败（目录只读等）保持原 stderr。
#[cfg(target_os = "windows")]
fn redirect_stderr_to_log() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE};
    let log = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("relay.log")))
        .unwrap_or_else(|| std::path::PathBuf::from("relay.log"));
    if std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0) > 5_000_000 {
        let _ = std::fs::remove_file(&log);
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
        let handle = f.as_raw_handle();
        std::mem::forget(f); // 句柄交给系统 stderr，进程生命周期内常驻（勿 CloseHandle）
        unsafe {
            let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn redirect_stderr_to_log() {}

fn main() {
    // Phase 62：stderr 落盘（断流 [warn] 等诊断日志在 --autostart 场景唯一可查处）
    redirect_stderr_to_log();

    // Phase 55：桌面卡片高 DPI——进程级 PMv2（须为进程内第一个 DPI 调用）。
    // 使 280×132 物理像素在缩放屏上不模糊；对托盘图标/muda 菜单/balloon 隐藏窗口无副作用。
    // 失败（旧系统）回退 SetProcessDPIAware（system-aware）。
    #[cfg(target_os = "windows")]
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        let ok = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if ok == 0 {
            use windows_sys::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;
            let _ = SetProcessDPIAware();
        }
    }

    // 双击启动：最先隐藏黑色控制台（越早越好，减少闪烁）
    let gui_launch = hide_console_if_double_clicked();
    let a = parse_args();
    let config_path = resolve_config_path(a.config_path);

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[error] {e}");
            std::process::exit(1);
        }
    };

    // 供 start-claude.cmd 提示：打印写入 claude 的哨兵模型值 `claude-relay-model[上下文]`
    //（CC 发回哨兵后中继用「当前使用模型」替换，Phase 22）。
    if a.print_default_model {
        println!("{}", cfg.sentinel_model());
        return;
    }

    // Phase 45：确保 Claude Code 自定义 `/models` 命令存在（分发 exe 后跑一次即自动生成；
    // 已存在则不动——用户自定义内容不覆盖）。失败仅告警，不阻断服务。
    if let Err(e) = commands::ensure_models_command() {
        eprintln!("[warn] 自动安装 /models 命令失败: {e}（不影响服务运行）");
    }

    let warnings = match cfg.validate() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[error] config invalid: {e}");
            std::process::exit(1);
        }
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    rt.block_on(async_main(
        config_path,
        cfg,
        warnings,
        a.host,
        a.port,
        a.debug,
        a.autostart,
        a.restart,
        gui_launch,
    ));
}

/// 绑定监听地址，带重试。`POST /api/restart` 自重启时新进程会在旧进程
/// 释放端口前抢先 bind，这里最多重试 20×150ms（3s）扛过这个瞬间。
/// 失败返回 Err（首次启动由调用方 exit，托盘重启路径转为 1s 重试）。
async fn bind_with_retry(addr: &str) -> Result<tokio::net::TcpListener, String> {
    for attempt in 1..=20 {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => return Ok(l),
            Err(e) => {
                eprintln!("[warn] bind {addr} 第 {attempt} 次失败: {e}（重启时旧进程可能未退出）");
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
        }
    }
    Err(format!("bind {addr} 重试 20 次仍失败"))
}

/// 短超时探测端口是否已被监听（relay 可能已在运行）。500ms 内连上视为占用。
async fn port_is_busy(addr: &str) -> bool {
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    {
        Ok(Ok(_)) => true,
        _ => false,
    }
}

/// 托盘「停止/启动」驱动的服务循环（Phase 18）。
///
/// 状态由 `tokio::sync::watch::Sender<bool>` 承载（托盘线程 send，本循环 borrow/await）：
///   - `paused=false` → `bind_with_retry` + `axum::serve`，`tokio::select!` 同时监听停止信号；
///   - `paused=true`  → 等待「启动」信号，之后重新 bind。
/// 停止时 select 的 serve future 被 drop → **监听器释放、端口关闭** → 浏览器直接访问
/// 显示「服务不存在」（连接被拒），而不是 503 页面；恢复只能靠托盘「启动」。
///
/// 每轮先 `borrow_and_update()` 丢弃陈旧唤醒，避免「停止→启动」快速连点产生的多余
/// watch 版本号导致误停。绑定失败：首次启动 exit(1)，托盘重启路径 1s 重试（用户可「停止」放弃）。
async fn run_server(
    addr: String,
    state: Arc<AppState>,
    mut paused_rx: watch::Receiver<bool>,
    open_browser: bool,
) {
    let mut served_once = false;
    loop {
        paused_rx.borrow_and_update();
        if *paused_rx.borrow() {
            eprintln!("[server] 已停止 — 等待托盘「启动」恢复");
            // 通道正常由 async_main 保活，Err 仅理论路径（托盘线程退出）→ 结束进程
            if paused_rx.changed().await.is_err() {
                eprintln!("[server] 托盘已退出，ClaudeRelay 退出");
                std::process::exit(0);
            }
            continue;
        }
        match bind_with_retry(&addr).await {
            Ok(listener) => {
                if served_once {
                    println!("[server] 托盘「启动」：恢复监听 {addr}");
                }
                let first_start = !served_once;
                served_once = true;
                // 双击启动（非 --autostart）→ 首次监听就绪后打开配置页
                if first_start && open_browser {
                    let url = format!("http://{addr}");
                    eprintln!("[info] 双击启动，打开配置页 {url}");
                    let _ = crate::tray::open_browser(&url);
                }
                let app = api::build(state.clone());
                tokio::select! {
                    _ = axum::serve(listener, app) => {
                        eprintln!("[server] serve 结束");
                    }
                    changed = paused_rx.changed() => {
                        // changed: Result<(), RecvError>，Sender 保活时恒 Ok
                        if changed.is_err() {
                            eprintln!("[server] 托盘已退出，ClaudeRelay 退出");
                            std::process::exit(0);
                        }
                        eprintln!("[tray] 服务已停止 — 端口 {addr} 已释放，浏览器访问显示「服务不存在」");
                    }
                }
            }
            Err(e) => {
                if !served_once {
                    eprintln!("[error] {e}");
                    std::process::exit(1);
                }
                eprintln!("[warn] {e} — 1s 后重试（托盘「停止」可放弃）");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn async_main(
    config_path: PathBuf,
    mut cfg: Config,
    warnings: Vec<String>,
    host_override: Option<String>,
    port_override: Option<u16>,
    debug: bool,
    autostart: bool,
    restart: bool,
    gui_launch: bool,
) {
    if let Some(h) = host_override {
        cfg.server.host = h;
    }
    if let Some(p) = port_override {
        cfg.server.port = p;
    }
    for w in &warnings {
        eprintln!("[warn] {w}");
    }

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);

    // 端口已被占用（relay 已在运行）：
    //   双击（非 --autostart/--restart）→ 打开配置页后退出；后台启动（--autostart）→ 静默退出。
    //   --restart（/api/restart 自重启用）：跳过检查——旧进程尚在占用端口（600ms 后退出），
    //   由 bind_with_retry（20×150ms）扛过交接瞬间；否则新进程会误判"已有 relay"直接退出，
    //   旧进程再退出 → 服务整体被杀（Phase 20 修复）。
    if !restart && port_is_busy(&addr).await {
        if gui_launch && !autostart {
            eprintln!("[info] 已有 relay 在运行，打开配置页 http://{addr}");
            let _ = crate::tray::open_browser(&format!("http://{addr}"));
        } else {
            eprintln!("[info] 已有 relay 在运行，退出");
        }
        return;
    }

    // 启动时同步开机自启注册：仅当配置开启时注册（幂等）。关闭方向由 UI 保存
    // （config_put）负责——避免测试/其他配置启动时误删用户已启用的注册表项。
    if cfg.server.auto_start {
        if let Err(e) = autostart::apply(true) {
            eprintln!("[warn] 开机自启注册失败: {e}");
        }
    }

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build http client");

    // Phase 27：用量持久化——启动时从 `{config_path}-usage.json` 加载（重启不清零），
    // 周期落盘任务每 5s 写一次（dirty 才写，空闲零 IO）。
    let usage_path = usage_path_of(&config_path);
    let state = Arc::new(AppState {
        cfg: Arc::new(RwLock::new(cfg.clone())),
        client,
        debug,
        config_path: config_path.clone(),
        started: std::time::Instant::now(),
        usage: Arc::new(std::sync::Mutex::new(crate::config::UsageStats::load(&usage_path))),
        usage_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });

    // 托盘「停止/启动」共享状态：Sender 在托盘线程，Receiver 在 run_server 服务循环。
    let (paused_tx, paused_rx) = watch::channel(false);
    // `_keepalive` 保活 watch 通道：托盘初始化失败时（menu/icon/build 任一失败，
    // 线程 return、Sender drop）也不会让通道关闭 → run_server 的 borrow() 不会 panic，
    // 服务继续运行（托盘失败不致命，与历史行为一致）。
    let _keepalive = paused_tx.clone();

    println!(
        "ClaudeRelay v{} — {}",
        api::VERSION,
        if debug { "DEBUG 模式" } else { "运行中" }
    );
    println!("  监听:  http://{}", addr);
    println!("  配置:  {}", config_path.display());
    println!("  中转:  Claude Code 设置 ANTHROPIC_BASE_URL=http://{} 后 /model 切换", addr);
    let aliases = router::available_aliases(&cfg.providers, &cfg.server.separator);
    if aliases.is_empty() {
        println!("  模型:  （未配置服务商 — 打开配置页 http://{} 添加）", addr);
    } else {
        println!("  模型:  {}", aliases.join(", "));
    }
    if cfg.server.auto_start {
        println!("  开机自启: 已注册（--autostart 后台运行）");
    }
    println!("  Ctrl+C 或托盘「退出」结束服务；托盘「停止」释放端口、停掉整个服务");
    // Phase 25：托盘「模型」菜单快照 —— (服务商显示名, [(完整别名, 别名)]) + 当前模型完整别名
    let sep: &str = if cfg.server.separator.is_empty() {
        "-"
    } else {
        &cfg.server.separator
    };
    let model_menu: Vec<(String, Vec<(String, String)>)> = cfg
        .providers
        .iter()
        .map(|p| {
            let name = if p.name.trim().is_empty() {
                p.id.clone()
            } else {
                p.name.trim().to_string()
            };
            let ms = p
                .models
                .iter()
                .map(|m| (format!("{}{}{}", p.id, sep, m.alias), m.alias.clone()))
                .collect();
            (name, ms)
        })
        .collect();
    let current_model = cfg.default_model();
    // Phase 55：桌面悬浮卡片——共享状态（路径按配置 sibling），数据任务与托盘共用。
    let base_url = format!("http://{}", addr);
    let card_shared = crate::widget::WidgetShared::new(&config_path, base_url.clone(), state.clone());
    crate::widget::spawn_data_task(
        state.clone(),
        card_shared.clone(),
        base_url.clone(),
        paused_rx.clone(),
    );
    tray::spawn(
        base_url,
        paused_tx,
        state.clone(),
        model_menu,
        current_model,
        card_shared,
    );

    // Phase 27：用量周期落盘——每 5s 检查脏标记，有变化才写（空闲零 IO）。
    // Phase 54：每轮读保留窗口并剪除超期时间桶（内存显示正确；写前再剪快照，磁盘有界）。
    // current_thread runtime 内 tokio::spawn 的任务与 run_server 交错调度，正常执行。
    {
        let st = state.clone();
        let path = usage_path.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let (hours, days) = {
                    let cfg = st.cfg.read().await;
                    (cfg.server.usage_hours, cfg.server.usage_days)
                };
                {
                    let mut u = st.usage.lock().unwrap();
                    u.prune(hours, days);
                }
                if st.usage_dirty.load(std::sync::atomic::Ordering::Relaxed) {
                    st.usage_dirty
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    let mut snap = st.usage.lock().unwrap().clone();
                    snap.prune(hours, days);
                    if let Err(e) = snap.save(&path) {
                        eprintln!("[warn] 用量落盘失败: {e}");
                        st.usage_dirty
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        });
    }

    // Phase 53：Coding Plan 控制台 Cookie 每小时自动刷新——用**存档 Cookie**（浏览器不必登录）
    // 请求各服务商用量接口，从响应 Set-Cookie 取新 Cookie 续期，保持不过期（隔天不必重新登录）。
    // 请求是 async（reqwest），直接在 current_thread runtime 内 tokio::spawn；无 Set-Cookie / 值未变化
    // / 请求失败 → 保留旧值零写盘。启动 ~30s 先续期一次（覆盖重启后立刻用旧 Cookie 的窗口），此后每 1 小时。
    {
        let st = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            loop {
                let changed = webui::refresh_console_cookies(st.clone()).await;
                if !changed.is_empty() {
                    eprintln!("[cookie] 已自动续期控制台 Cookie: {}", changed.join(", "));
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }

    // 双击启动（非 --autostart/--restart）→ run_server 首次就绪后打开配置页
    let open_browser_on_start = gui_launch && !autostart && !restart;
    run_server(addr, state, paused_rx, open_browser_on_start).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppState, Config};

    /// 轮询等待条件成立（async 闭包），100×20ms = 2s 超时。
    async fn wait_until<F, Fut>(what: &str, mut f: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..100 {
            if f().await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("timeout waiting for {what}");
    }

    async fn can_connect(addr: &str) -> bool {
        tokio::net::TcpStream::connect(addr).await.is_ok()
    }

    #[test]
    fn tray_stop_releases_port_and_start_rebinds() {
        // Phase 18：托盘「停止」= 真正停掉服务 —— 释放端口，浏览器访问显示服务不存在
        // （连接被拒）；托盘「启动」= 重新监听恢复。在真实端口上驱动 run_server 验证。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // 先占一个临时端口拿空闲端口号，再释放给 run_server 绑定
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = probe.local_addr().unwrap().port();
            drop(probe);
            let addr = format!("127.0.0.1:{port}");

            let state = Arc::new(AppState {
                cfg: Arc::new(tokio::sync::RwLock::new(Config::default())),
                client: reqwest::Client::new(),
                debug: false,
                config_path: std::path::PathBuf::from("config.json"),
                started: std::time::Instant::now(),
                usage: Arc::new(std::sync::Mutex::new(crate::config::UsageStats::default())),
                usage_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            });

            let (paused_tx, paused_rx) = watch::channel(false);
            tokio::spawn(run_server(addr.clone(), state, paused_rx, false));

            // 运行中：端口可连接
            wait_until("port open", || can_connect(&addr)).await;

            // 停止：端口释放，连接被拒（浏览器即「服务不存在」）
            paused_tx.send(true).unwrap();
            wait_until("port closed", || {
                let a = addr.clone();
                async move { !can_connect(&a).await }
            })
            .await;

            // 启动：端口重新监听，恢复访问
            paused_tx.send(false).unwrap();
            wait_until("port reopened", || can_connect(&addr)).await;

            // 清理：停止并等端口关闭
            paused_tx.send(true).unwrap();
            wait_until("port closed after cleanup", || {
                let a = addr.clone();
                async move { !can_connect(&a).await }
            })
            .await;
        });
    }
}

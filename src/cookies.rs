//! Phase 28/30：从 Chrome/Edge Cookie 数据库读取控制台域名的 Cookie
//! （`console.volcengine.com` 火山方舟 / `maas.xfyun.cn` 讯飞星辰）。
//!
//! Windows 解密链（均为 best-effort，失败回退手动粘贴的 `console_cookie`）：
//!   ① `Local State`（浏览器数据目录根）读 `os_crypt.encrypted_key`（base64，前缀 `DPAPI`）
//!   ② `CryptUnprotectData`（DPAPI，当前进程 = 同一 Windows 用户即可）→ 32 字节主密钥
//!   ③ Cookie 库（SQLite，运行中处于 WAL 模式 → 用 rusqlite bundled 读取最稳）
//!      里 `encrypted_value` 前缀 `v10` = 3B + 12B nonce + AES-128-GCM 密文（key = 主密钥前 16B）
//!   ④ 新版 `v11`（app-bound）：Local State `app_bound_encrypted_key` 尽力而为，失败提示手动粘贴。

use std::path::{Path, PathBuf};

/// 读取火山方舟控制台（`console.volcengine.com`）的 Cookie 头值。
pub fn read_volcengine_console_cookie() -> Result<String, String> {
    read_console_cookie("%volcengine.com", "console.volcengine.com")
}

/// 读取讯飞星辰 MaaS 控制台（`maas.xfyun.cn`）的 Cookie 头值（Phase 30）。
pub fn read_xf_console_cookie() -> Result<String, String> {
    read_console_cookie("%xfyun.cn", "maas.xfyun.cn")
}

/// 读取 OpenCode 控制台（`opencode.ai`）的 Cookie 头值（Phase 48）。
pub fn read_opencode_console_cookie() -> Result<String, String> {
    read_console_cookie("%opencode.ai", "opencode.ai")
}

/// 读取某控制台域名的 Cookie 头值（`name=value; name=value`）。
/// `domain_like` 是 SQLite `host_key LIKE` 模式（如 `%volcengine.com`），`display` 用于错误提示。
/// Err = 找不到浏览器/库/目标 Cookie，或**浏览器运行中锁定了 Cookie 库**（调用方回退手动粘贴）。
fn read_console_cookie(domain_like: &str, display: &str) -> Result<String, String> {
    let mut reasons: Vec<String> = Vec::new();
    let mut saw_lock = false;
    for (local_state, db) in browser_cookie_files() {
        let key = match load_os_crypt_key(&local_state) {
            Ok(k) => k,
            Err(e) => {
                reasons.push(format!("{}: {e}", local_state.display()));
                continue;
            }
        };
        match query_cookies(&db, &key, domain_like) {
            Ok(Some(header)) => return Ok(header),
            Ok(None) => {
                reasons.push(format!("{}: 无 {display} Cookie", db.display()));
                continue;
            }
            Err(e) => {
                // Windows 共享冲突（os error 32）＝ 浏览器运行中独占锁定 Cookie 库
                if e.contains("os error 32") || e.to_lowercase().contains("lock") {
                    saw_lock = true;
                }
                reasons.push(format!("读 {} 失败: {e}", db.display()));
            }
        }
    }
    if saw_lock {
        Err("浏览器运行中锁定了 Cookie 数据库（无法读取）——请手动粘贴 Cookie，或关闭浏览器后再点「导入 Cookie」".into())
    } else {
        Err(format!("未能从浏览器读取 {display} 的 Cookie：{}", reasons.join("；")).into())
    }
}

/// Chrome/Edge 各 profile 的 `(Local State, Cookies)` 对（Local State 在数据目录根，全局共享）。
fn browser_cookie_files() -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    if local.is_empty() {
        return out;
    }
    let roots = [
        format!(r"{local}\Google\Chrome\User Data"),
        format!(r"{local}\Microsoft\Edge\User Data"),
    ];
    for root in roots {
        let ls = PathBuf::from(&root).join("Local State");
        if !ls.exists() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&root) {
            for e in entries.flatten() {
                let db = e.path().join("Network").join("Cookies");
                if db.exists() {
                    out.push((ls.clone(), db));
                }
            }
        }
    }
    out
}

struct OsCryptKey {
    /// v10 主密钥（32 字节，取前 16 字节做 AES-128-GCM）。
    v10: Vec<u8>,
    /// v11 app-bound 密钥（尽力而为，缺失则 v11 Cookie 解不开）。
    app_bound: Option<Vec<u8>>,
}

/// 读 Local State，解出 os_crypt 主密钥 +（尽力）app-bound 密钥。
fn load_os_crypt_key(local_state: &Path) -> Result<OsCryptKey, String> {
    let raw =
        std::fs::read_to_string(local_state).map_err(|e| format!("read Local State: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("parse Local State: {e}"))?;
    let os = v.get("os_crypt").ok_or("Local State 无 os_crypt")?;

    let enc = os
        .get("encrypted_key")
        .and_then(|k| k.as_str())
        .ok_or("os_crypt 无 encrypted_key")?;
    let key = decrypt_dpapi_base64(enc)?;
    if key.len() != 32 {
        return Err(format!("主密钥长度 {} != 32", key.len()));
    }

    // v11 app-bound 密钥（尽力而为）
    let app_bound = os
        .get("app_bound_encrypted_key")
        .and_then(|k| k.as_str())
        .and_then(|s| decrypt_dpapi_base64(s).ok())
        .and_then(|k| (k.len() == 32).then_some(k));

    Ok(OsCryptKey { v10: key, app_bound })
}

/// `DPAPI` 前缀 base64 → DPAPI 解密 → 原始密钥字节。
fn decrypt_dpapi_base64(encoded: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| format!("base64: {e}"))?;
    let blob = blob
        .strip_prefix(b"DPAPI")
        .ok_or("前缀不是 DPAPI")?;
    // v11 app-bound blob 头部是 `APPBOUND` magic，剥掉再 DPAPI
    let blob = blob.strip_prefix(b"APPBOUND").unwrap_or(blob);
    dpapi_unprotect(blob)
}

#[cfg(target_os = "windows")]
fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptUnprotectData 失败 (win32 error {})",
            unsafe { GetLastError() }
        ));
    }
    let out = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(output.pbData as *mut core::ffi::c_void);
    }
    Ok(out)
}

#[cfg(not(target_os = "windows"))]
fn dpapi_unprotect(_data: &[u8]) -> Result<Vec<u8>, String> {
    Err("DPAPI 解密仅支持 Windows".into())
}

/// 查询 Cookie 库里目标域名的行，拼成 Cookie 头。
/// Ok(None) = 库可读但无该域 Cookie；Err = 库打不开/查询失败。
fn query_cookies(db: &Path, key: &OsCryptKey, domain_like: &str) -> Result<Option<String>, String> {
    // 浏览器运行中时 Cookie 库处于 WAL 模式且被独占锁住——只读直开会报
    // "unable to open database file"。标准做法：连 `-wal`/`-shm` 一起复制到
    // 临时目录再打开副本（SQLite 自动回放 WAL，快照足够新）。
    let copy = copy_cookie_db(db)?;
    let res = query_cookies_conn(&copy, key, domain_like);
    let _ = std::fs::remove_file(&copy);
    let _ = std::fs::remove_file(PathBuf::from(format!("{}-wal", copy.display())));
    let _ = std::fs::remove_file(PathBuf::from(format!("{}-shm", copy.display())));
    res
}

/// 把 Cookie 库主文件 + `-wal` + `-shm`（存在则）复制到临时目录，返回副本路径。
fn copy_cookie_db(db: &Path) -> Result<PathBuf, String> {
    let tmp = std::env::temp_dir().join(format!("cookies_{}.db", std::process::id()));
    let tmp_wal = PathBuf::from(format!("{}-wal", tmp.display()));
    let tmp_shm = PathBuf::from(format!("{}-shm", tmp.display()));
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(&tmp_wal);
    let _ = std::fs::remove_file(&tmp_shm);
    std::fs::copy(db, &tmp).map_err(|e| format!("copy cookie db: {e}"))?;
    for suffix in ["-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{}", db.display(), suffix));
        if src.exists() {
            let _ = std::fs::copy(&src, PathBuf::from(format!("{}{}", tmp.display(), suffix)));
        }
    }
    Ok(tmp)
}

/// 在已复制/可写的 Cookie 库副本上查询（读-写打开副本让 SQLite 回放 WAL）。
fn query_cookies_conn(
    copy: &Path,
    key: &OsCryptKey,
    domain_like: &str,
) -> Result<Option<String>, String> {
    let conn = rusqlite::Connection::open_with_flags(
        copy,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open cookie db: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT name, value, encrypted_value FROM cookies WHERE host_key LIKE ?1")
        .map_err(|e| format!("prepare: {e}"))?;
    let mut rows = stmt.query([domain_like]).map_err(|e| format!("query: {e}"))?;
    let mut pairs = Vec::new();
    while let Some(row) = rows.next().map_err(|e| format!("row: {e}"))? {
        let name: String = row.get(0).map_err(|e| e.to_string())?;
        let plain: String = row.get(1).map_err(|e| e.to_string())?;
        let enc: Vec<u8> = row.get(2).map_err(|e| e.to_string())?;
        let value = if !plain.is_empty() {
            plain
        } else if enc.is_empty() {
            continue;
        } else {
            match decrypt_value(&key.v10, &enc, key.app_bound.as_deref()) {
                Ok(v) => String::from_utf8_lossy(&v).to_string(),
                Err(e) => {
                    eprintln!("[cookies] 解密 {name} 失败: {e}");
                    continue; // 单条失败跳过，不整体失败
                }
            }
        };
        pairs.push((name, value));
    }
    if pairs.is_empty() {
        return Ok(None);
    }
    Ok(Some(build_cookie_header(pairs)))
}

/// 解密单条 Cookie：`v10`（主密钥）或 `v11`（app-bound，尽力）。
fn decrypt_value(key: &[u8], enc: &[u8], app_key: Option<&[u8]>) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes128Gcm, Nonce};

    if enc.len() < 3 + 12 + 16 {
        return Err("encrypted_value 过短".into());
    }
    let (prefix, rest) = enc.split_at(3);
    let (nonce_bytes, ct) = rest.split_at(12);
    let k16: &[u8; 16] = match prefix {
        b"v10" => key[..16].try_into().map_err(|_| "主密钥不足 16B")?,
        b"v11" => app_key
            .and_then(|k| k[..16].try_into().ok())
            .ok_or("v11 Cookie 需 app-bound 密钥（建议手动粘贴兜底）")?,
        other => return Err(format!("未知 Cookie 加密前缀 {:?}", String::from_utf8_lossy(other))),
    };
    let cipher = Aes128Gcm::new_from_slice(k16).map_err(|e| e.to_string())?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let pt = cipher.decrypt(nonce, ct).map_err(|e| format!("AES-GCM 解密失败: {e}"))?;
    Ok(pt)
}

/// 拼 Cookie 头：`name=value; name=value`。
fn build_cookie_header(pairs: Vec<(String, String)>) -> String {
    pairs
        .iter()
        .map(|(n, v)| format!("{n}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_cookie_header_joins() {
        assert_eq!(
            build_cookie_header(vec![("a".into(), "1".into()), ("b".into(), "x y".into())]),
            "a=1; b=x y"
        );
        assert_eq!(build_cookie_header(vec![]), "");
    }

    #[test]
    fn v10_decrypt_roundtrip() {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes128Gcm, Nonce};
        // 自编自解验证 decrypt_value 的 v10 路径
        let key = [7u8; 32];
        let cipher = Aes128Gcm::new_from_slice(&key[..16]).unwrap();
        let nonce_bytes = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), b"sessionid=abc123".as_slice())
            .unwrap();
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ct);
        let out = decrypt_value(&key, &blob, None).unwrap();
        assert_eq!(out, b"sessionid=abc123");
    }

    #[test]
    fn v11_without_app_key_errors() {
        // v11 无 app-bound 密钥 → 明确报错（回退手动粘贴）
        let mut blob = b"v11".to_vec();
        blob.extend_from_slice(&[0u8; 12]);
        blob.extend_from_slice(&[0u8; 16]);
        let e = decrypt_value(&[0u8; 32], &blob, None).unwrap_err();
        assert!(e.contains("v11"), "got: {e}");
    }

    #[test]
    fn query_cookies_reads_volcengine_rows() {
        // 全链路（SQLite 查询 → v10 解密 → 拼头）：造一个假 Cookie 库验证成功路径
        //（真实浏览器运行中锁库无法读，此测试保证代码路径正确）
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes128Gcm, Nonce};
        let key = [9u8; 32];
        let cipher = Aes128Gcm::new_from_slice(&key[..16]).unwrap();
        let nonce = [3u8; 12];
        let enc = cipher
            .encrypt(Nonce::from_slice(&nonce), b"volc-session-val".as_slice())
            .unwrap();
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&enc);

        let tmp = std::env::temp_dir().join(format!("cookies_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        {
            let conn = rusqlite::Connection::open(&tmp).unwrap();
            conn.execute_batch(
                "CREATE TABLE cookies (host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cookies (host_key, name, value, encrypted_value) VALUES ('console.volcengine.com', 'sessionid', '', ?1)",
                rusqlite::params![blob],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cookies (host_key, name, value, encrypted_value) VALUES ('console.volcengine.com', 'plaincookie', 'plainval', X'')",
                rusqlite::params![],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cookies (host_key, name, value, encrypted_value) VALUES ('other.com', 'x', 'y', X'')",
                rusqlite::params![],
            )
            .unwrap();
        }
        let k = OsCryptKey { v10: key.to_vec(), app_bound: None };
        let hdr = query_cookies(&tmp, &k, "%volcengine.com")
            .unwrap()
            .expect("should find cookie");
        assert!(hdr.contains("sessionid=volc-session-val"), "got: {hdr}");
        assert!(hdr.contains("plaincookie=plainval"), "got: {hdr}");
        assert!(!hdr.contains("other.com"), "got: {hdr}");
        // Phase 30：域参数化——同一库按 `%xfyun.cn` 查应无结果（不误读别的域）
        assert!(query_cookies(&tmp, &k, "%xfyun.cn").unwrap().is_none());

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(PathBuf::from(format!("{}-wal", tmp.display())));
        let _ = std::fs::remove_file(PathBuf::from(format!("{}-shm", tmp.display())));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn dpapi_roundtrip() {
        // 验证 CryptUnprotectData 调用方式在真实 Windows 上可用（加密→解密）
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{
            CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        };
        let data = b"hello dpapi key material";
        let input =
            CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        assert_ne!(ok, 0, "CryptProtectData 失败");
        let blob = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe {
            LocalFree(out.pbData as *mut core::ffi::c_void);
        }
        assert_eq!(dpapi_unprotect(&blob).unwrap(), data);
    }
}

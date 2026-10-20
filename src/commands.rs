//! Claude Code 自定义命令自动安装（Phase 45）。
//!
//! 双击运行 relay 时，确保 Claude Code 的全局自定义命令 `models.md`（`/models`）存在——
//! 缺失则自动写入内嵌内容。这样把 `claude-relay.exe` 分发给用户后，跑一次即自动生成
//! `/models` 命令（查看当前模型列表），无需手工创建命令文件。
//!
//! 幂等：目标文件已存在则**不动**（用户可能自定义过内容，不覆盖）。
//! 失败仅由调用方告警，不阻断服务启动。跨平台（非 Windows 同样有效）。

use std::path::PathBuf;

/// 内嵌的 `/models` 命令文件内容（与仓库 `assets/commands/models.md` 一致，
/// 分发 exe 时随二进制一起携带）。
pub const MODELS_COMMAND: &str = include_str!("../assets/commands/models.md");

/// 确保 `{commands_dir}/models.md` 存在。返回写入的路径（或已存在路径），供日志。
pub fn ensure_models_command() -> Result<PathBuf, String> {
    let Some(dir) = commands_dir() else {
        return Err("cannot determine commands dir (no USERPROFILE/HOME/CLAUDE_CONFIG_DIR)".into());
    };
    let path = dir.join("models.md");
    if path.exists() {
        return Ok(path); // 已有（含用户自定义）→ 不覆盖
    }
    // Phase 59：统一自愈式原子写（新系统 `~/.claude` 目录缺失自动创建、只读属性自愈）
    crate::config::write_file_atomic(&path, MODELS_COMMAND)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// 用户全局命令目录：尊重 `CLAUDE_CONFIG_DIR`（`{dir}/commands`），否则 `~/.claude/commands`。
/// 与 `webui::claude_settings_path` 的解析规则一致。
fn commands_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir).join("commands"));
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(PathBuf::from(home).join(".claude").join("commands"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_models_command_creates_and_preserves() {
        // 用临时 CLAUDE_CONFIG_DIR 隔离：缺失 → 创建 + 内容为内嵌命令；已存在 → 不覆盖。
        let dir = std::env::temp_dir().join(format!("relay_cmds_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("CLAUDE_CONFIG_DIR", &dir);

        let path = ensure_models_command().unwrap();
        assert_eq!(path, dir.join("commands").join("models.md"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, MODELS_COMMAND, "应写入内嵌命令内容");
        assert!(content.contains("allowed-tools"), "frontmatter 在位");
        assert!(content.contains("/api/config"), "正文含数据源");

        // 用户改动内容后再次 ensure → 不覆盖
        std::fs::write(&path, "# 用户自定义\n").unwrap();
        let p2 = ensure_models_command().unwrap();
        assert_eq!(p2, path);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# 用户自定义\n",
            "已存在文件不得被覆盖"
        );

        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! 来源目录：声明式单一事实源（§3 两层目录 / §9 catalog）。
//!
//! 上层「来源族」（stable）：provider + 物理形态 + 配置根，少变；
//! 下层「已验证实现」（volatile）：具体子目录 + glob + 状态，随实现验证演进。
//! 新增 provider = 在 `builtin_descriptors()` 加一个描述符。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::rawevent::{SourceMode, SourceType};

/// 扫描 profile：只要元数据，还是连正文一起物化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// 只发元数据/用量事件；`RawEvent.content` 恒为 None。
    Metadata,
    /// 物化正文（总库 full 写者用）。
    Full,
}

/// 已验证实现的成熟度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// 实机验证过，可增量。
    Stable,
    /// 结构已知、未充分验证。
    Experimental,
    /// 仅占位，尚无解析器。
    Planned,
}

/// 一个具体的「已验证实现」条目（provider 下的某个数据产物）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// 相对 provider 配置根的子目录（如 `projects` / `sessions`）。
    pub subdir: String,
    /// 文件匹配 glob（如 `**/*.jsonl`）。
    pub glob: String,
    pub source_mode: SourceMode,
    pub status: Status,
    /// 是否递归子目录发现。
    pub recursive: bool,
}

/// 一个 provider 的完整描述符（来源族 + 其下实现）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub source_type: SourceType,
    /// 人类可读名。
    pub name: String,
    /// 配置根目录（已解析绝对路径；None = 本机未发现该 provider）。
    pub config_dir: Option<PathBuf>,
    pub artifacts: Vec<Artifact>,
}

/// Claude Code 配置根：`$CLAUDE_CONFIG_DIR` 覆盖，否则 `~/.claude`。
pub fn claude_config_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    dirs_next::home_dir().map(|h| h.join(".claude"))
}

/// Codex 配置根：`$CODEX_HOME` 覆盖，否则 `~/.codex`。
pub fn codex_config_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CODEX_HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    dirs_next::home_dir().map(|h| h.join(".codex"))
}

/// 内置 provider 描述符（§3.2 Claude / §3.3 Codex）。
pub fn builtin_descriptors() -> Vec<ProviderDescriptor> {
    vec![
        ProviderDescriptor {
            source_type: SourceType::ClaudeCode,
            name: "Claude Code".to_string(),
            config_dir: claude_config_dir(),
            artifacts: vec![
                Artifact {
                    subdir: "projects".to_string(),
                    glob: "**/*.jsonl".to_string(),
                    source_mode: SourceMode::AppendLog,
                    status: Status::Stable,
                    recursive: true,
                },
                Artifact {
                    subdir: "sessions".to_string(),
                    glob: "**/*.jsonl".to_string(),
                    source_mode: SourceMode::AppendLog,
                    status: Status::Stable,
                    recursive: true,
                },
            ],
        },
        ProviderDescriptor {
            source_type: SourceType::Codex,
            name: "Codex".to_string(),
            config_dir: codex_config_dir(),
            artifacts: vec![
                Artifact {
                    subdir: "sessions".to_string(),
                    glob: "**/*.jsonl".to_string(),
                    source_mode: SourceMode::AppendLog,
                    status: Status::Stable,
                    recursive: true,
                },
                Artifact {
                    subdir: "archived_sessions".to_string(),
                    glob: "**/*.jsonl".to_string(),
                    source_mode: SourceMode::AppendLog,
                    status: Status::Experimental,
                    recursive: true,
                },
            ],
        },
    ]
}

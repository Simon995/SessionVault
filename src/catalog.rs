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
    /// **空串 = 配置根自身**（`root.join("")` 等价于 `root`）。
    pub subdir: String,
    /// 文件匹配 glob。两种形态：
    /// - 含通配符（`**/*.jsonl` / `*.md`）—— 按后缀收集，`recursive` 决定层数；
    /// - **不含通配符**（`CLAUDE.md`）—— 那就是个**精确文件名**，按名字过滤。
    pub glob: String,
    pub source_mode: SourceMode,
    pub status: Status,
    /// 是否递归子目录发现。
    pub recursive: bool,
    /// 这份制品**是什么类别** —— 消费方按它分派（TumeFlow 的 Class-B 就是
    /// 按 `instruction` 走另一条解析）。
    ///
    /// 🔴 **声明，不是从 `glob` 猜**（2026-08-26 改）。此前是
    /// `fn artifact_kind(mode, glob)`：`.rules` 结尾算 `rules`，**其余一律
    /// `memory`**。于是往表里加一条 `CLAUDE.md` 会被判成 `memory`，而消费方
    /// 按 `instruction` 分派 —— 加一行数据，行为在另一个文件里悄悄错掉，
    /// 且没有任何东西会报。
    ///
    /// ⚠️ `AppendLog` 没有类别（它是会话流，不是状态快照）⇒ `None`。
    #[serde(default)]
    pub kind: Option<ArtifactKind>,
}

/// 状态快照的类别。**与 `RawEvent.artifact_kind` 的取值一一对应**，
/// 序列化成同样的小写串，消费方（TumeFlow `snapshot.py`）据此分派。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// agent 自己写下的、已原子化的记忆文件。
    Memory,
    /// 授权规则一类的程序性记忆。
    Rules,
    /// **人手写的指令文件**（`CLAUDE.md` / `AGENTS.md`）——
    /// 项目级的，以及**配置根下那份全局的**。
    Instruction,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Rules => "rules",
            Self::Instruction => "instruction",
        }
    }
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

/// 通用 JSONL 根：**只认 `$SVAULT_JSONL_DIR`，没有默认值**。
///
/// 🔴 **没有默认根是刻意的。** 这个 provider 的用途是「把外部投喂的对话变成一个
/// **来源**」：调用方把 messages 写成 append-log，再由常规扫描摄取。
/// 它不该在任何人的机器上默认扫任何东西 —— 环境变量没设 ⇒ `None` ⇒ 发现器跳过
/// 并记一条 `no_config_dir`，既有消费者的行为**一个字节都不变**。
///
/// ⚠️ **别在这里举某个消费者当用途** —— 一个 provider 的正当性不该依赖谁在用它。
/// 写进内核文档的消费者名字会让后来人以为它是为那件事存在的，而那个消费者消失
/// 之后，这段注释仍然在这儿，声称一个不再成立的理由。
///
/// 判据：`the_generic_jsonl_provider_is_absent_without_its_env_var` 钉着这条。
pub fn jsonl_config_dir() -> Option<PathBuf> {
    match std::env::var("SVAULT_JSONL_DIR") {
        Ok(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => None,
    }
}

/// 内置 provider 描述符（§3.2 Claude / §3.3 Codex / 通用 JSONL）。
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
                    kind: None,
                },
                Artifact {
                    subdir: "sessions".to_string(),
                    glob: "**/*.jsonl".to_string(),
                    source_mode: SourceMode::AppendLog,
                    status: Status::Stable,
                    recursive: true,
                    kind: None,
                },
                Artifact {
                    // 🔴 **配置根下那份全局 `CLAUDE.md`**（2026-08-26 加）。
                    //
                    // 它此前不在表里，于是消费方两条路都拿不到它：主路径（快照）
                    // 不扫，回落路径（TumeFlow 的直读 adapter）当时也跳过。实测那台
                    // 机器上它有 33 KB、剥掉托管块后 3278 字**全是人手写的规则**。
                    //
                    // `subdir: ""` = 配置根自身；`glob` 不含通配符 ⇒ 按**精确文件名**
                    // 匹配（见 `Artifact::glob` 的文档），所以不会顺带收走根下别的 `.md`。
                    subdir: String::new(),
                    glob: "CLAUDE.md".to_string(),
                    source_mode: SourceMode::SnapshotFile,
                    status: Status::Experimental,
                    recursive: false,
                    kind: Some(ArtifactKind::Instruction),
                },
                Artifact {
                    subdir: "projects".to_string(),
                    glob: "**/memory/*.md".to_string(),
                    source_mode: SourceMode::SnapshotFile,
                    status: Status::Experimental,
                    recursive: true,
                    kind: Some(ArtifactKind::Memory),
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
                    kind: None,
                },
                Artifact {
                    subdir: "archived_sessions".to_string(),
                    glob: "**/*.jsonl".to_string(),
                    source_mode: SourceMode::AppendLog,
                    status: Status::Experimental,
                    recursive: true,
                    kind: None,
                },
                Artifact {
                    // 🔴 **配置根下那份全局 `AGENTS.md`** —— 与 Claude 那条同理。
                    // 实测 30 KB、剥掉托管块后 1491 字。
                    subdir: String::new(),
                    glob: "AGENTS.md".to_string(),
                    source_mode: SourceMode::SnapshotFile,
                    status: Status::Experimental,
                    recursive: false,
                    kind: Some(ArtifactKind::Instruction),
                },
                Artifact {
                    subdir: "memories".to_string(),
                    glob: "*.md".to_string(),
                    source_mode: SourceMode::SnapshotFile,
                    status: Status::Experimental,
                    recursive: false,
                    kind: Some(ArtifactKind::Memory),
                },
                Artifact {
                    subdir: "memories/rollout_summaries".to_string(),
                    glob: "*.md".to_string(),
                    source_mode: SourceMode::SnapshotFile,
                    status: Status::Experimental,
                    recursive: false,
                    kind: Some(ArtifactKind::Memory),
                },
                Artifact {
                    subdir: "memories/extensions/ad_hoc/notes".to_string(),
                    glob: "*.md".to_string(),
                    source_mode: SourceMode::SnapshotFile,
                    status: Status::Experimental,
                    recursive: false,
                    kind: Some(ArtifactKind::Memory),
                },
                Artifact {
                    subdir: "rules".to_string(),
                    glob: "*.rules".to_string(),
                    source_mode: SourceMode::SnapshotFile,
                    status: Status::Experimental,
                    recursive: false,
                    kind: Some(ArtifactKind::Rules),
                },
            ],
        },
        // 通用 JSONL：把**外部投喂的对话**变成一个来源。
        //
        // 🔴 `config_dir` 由 `$SVAULT_JSONL_DIR` 决定，**没有默认值** —— 没设时
        // 整个 provider 被发现器跳过（`no_config_dir`），既有消费者行为零变化。
        // 这是本 provider 能安全加进内置清单的**全部理由**。
        ProviderDescriptor {
            source_type: SourceType::Jsonl,
            name: "Generic JSONL".to_string(),
            config_dir: jsonl_config_dir(),
            artifacts: vec![Artifact {
                subdir: "sessions".to_string(),
                glob: "**/*.jsonl".to_string(),
                source_mode: SourceMode::AppendLog,
                status: Status::Experimental,
                recursive: true,
                // AppendLog 是会话流，不是状态快照 ⇒ 没有类别。
                kind: None,
            }],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 🔴 **配置根下那份全局指令文件必须在表里** —— 两个 provider 各一条。
    ///
    /// 它此前不在，于是消费方两条路都拿不到它（主路径快照不扫、回落路径也跳过）。
    /// 实测那台机器上四份加起来近 10 万字**全是人手写的规则**，而记忆层的核心
    /// 价值正是把它们同步到各个 agent。
    #[test]
    fn the_global_instruction_file_is_declared_for_both_providers() {
        let found: Vec<_> = builtin_descriptors()
            .into_iter()
            .flat_map(|d| {
                d.artifacts
                    .into_iter()
                    .filter(|a| a.subdir.is_empty())
                    .map(move |a| (d.source_type, a))
            })
            .collect();
        assert_eq!(found.len(), 2, "配置根下的制品应当恰好两条：{found:?}");
        for (_, art) in &found {
            assert!(
                art.glob == "CLAUDE.md" || art.glob == "AGENTS.md",
                "配置根下只该声明指令文件，得到 {}",
                art.glob
            );
            assert_eq!(
                art.kind,
                Some(ArtifactKind::Instruction),
                "{} 的类别不是 instruction —— 消费方会走错分派",
                art.glob
            );
            assert_eq!(art.source_mode, SourceMode::SnapshotFile);
            assert!(!art.recursive, "配置根不该递归扫");
        }
    }

    /// 🔴 **每条快照制品都得声明类别，一条都不许漏。**
    ///
    /// 上一版类别是从 `glob` 猜的（`.rules` → rules，**其余一律 memory**）。
    /// 于是往表里加一行数据，行为会在另一个文件里悄悄错掉，且没有任何东西会报。
    /// 现在它是声明 —— 而声明的价值取决于**没有人忘记写**。
    #[test]
    fn every_snapshot_artifact_declares_its_kind() {
        for d in builtin_descriptors() {
            for art in d.artifacts {
                match art.source_mode {
                    SourceMode::SnapshotFile => assert!(
                        art.kind.is_some(),
                        "{:?} 的 {}/{} 没声明类别",
                        d.source_type,
                        art.subdir,
                        art.glob
                    ),
                    // 会话流不是状态快照 —— 它**必须**没有类别，而不是随便填一个。
                    _ => assert!(
                        art.kind.is_none(),
                        "{:?} 的 {} 是 AppendLog 却带了类别",
                        d.source_type,
                        art.glob
                    ),
                }
            }
        }
    }

    /// 🔴 **加一个内置 provider 会改变每个消费者的行为 —— 除非它默认不存在。**
    ///
    /// 本仓有两个互不知情的消费者（一个当 Rust 库编译、一个内嵌 `svault` 可执行文件）。
    /// 通用 JSONL 这个 provider 是给「外部投喂的对话」用的，**不该在任何人的机器上
    /// 默认扫任何东西**：`$SVAULT_JSONL_DIR` 没设 ⇒ `config_dir: None` ⇒ 发现器跳过
    /// 并记 `no_config_dir`。
    ///
    /// 这条断言就是它能安全进内置清单的**全部理由**。
    ///
    /// ⚠️ 环境变量是进程级的，所以这里**不设也不清**它 —— 只断言「没设时是 None」，
    /// 用一个测试专用的判定入口，避免与并行跑的其它测试互相干扰。
    #[test]
    fn the_generic_jsonl_provider_is_absent_without_its_env_var() {
        let descriptors = builtin_descriptors();
        let jsonl = descriptors
            .iter()
            .find(|d| d.source_type == SourceType::Jsonl)
            .expect("通用 JSONL 描述符应当在内置清单里（框架可发现）");

        if std::env::var("SVAULT_JSONL_DIR").is_ok_and(|v| !v.is_empty()) {
            // 有人显式设了它 —— 那时**应当**有根，这条测试换个方向断言
            assert!(jsonl.config_dir.is_some());
        } else {
            assert!(
                jsonl.config_dir.is_none(),
                "没设 $SVAULT_JSONL_DIR 却给出了根 —— 那会让两个消费者开始扫一个它们\
                 从没同意过的目录"
            );
        }
    }

    /// 既有 provider 一个不少 —— 加法式改动的正面断言。
    #[test]
    fn the_builtin_providers_still_include_claude_and_codex() {
        let kinds: Vec<_> = builtin_descriptors()
            .iter()
            .map(|d| d.source_type)
            .collect();
        assert!(kinds.contains(&SourceType::ClaudeCode));
        assert!(kinds.contains(&SourceType::Codex));
    }
}

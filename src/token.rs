//! 投影操作的稳定身份（ADR-051 I7）。
//!
//! # 它解决什么
//!
//! `Rollback` 与 `Reparse` **开新代**。开新代的操作不幂等时，一次崩溃就留下一代垃圾：
//!
//! ```text
//! ① 总库成功 Rollback，推进 source_revision
//! ② UI 索引提交前进程退出
//! ③ UI 仍是旧游标 ⇒ 下轮再次检出 rollback
//! ④ 总库再次推进 source_revision
//! ⑤ Rollback 的旧版本**按设计永不自动回收**（`store::Projection::Rollback` 的注释：
//!    磁盘上那段内容已经不存在，前一个源版本是它的唯一副本）
//! ```
//!
//! 于是**每崩一次留一代不可回收的源版本**。`Reparse` 稍好（它会取代被超越的那代），
//! 但也会多开一代。
//!
//! 🔴 **`Append` 不需要 token** —— 它靠 `seq` 去重，重放同一批事件天然幂等。
//! 需要 token 的恰恰是「开新代」这个动作本身。
//!
//! # 判据
//!
//! **同一个 token = 同一次操作**，重复应用必须返回**原来的 head**，不得开新代。
//! 所以 token 要覆盖「什么决定了这次投影的内容」：
//!
//! | 分量 | 为什么在里面 |
//! | --- | --- |
//! | `SourceKey` | 不同文件是不同操作 |
//! | 源指纹 | 同一路径的**不同字节**是不同操作（同尺寸原地重写也算，见 ADR-051 §1） |
//! | 字节范围 | 全读与增量的产物不同 |
//! | `parser_revision` | 同一份字节、更好的解析器 ⇒ 不同内容 |
//! | `attribution_revision` | 同一份字节、更全的注册表 ⇒ 不同 `project_root` |
//!
//! ⚠️ **少一个分量就会把两次不同的操作当成同一次**，于是第二次被静默忽略 ——
//! 那比多开一代更糟：它会让新解析器的结果永远进不去。

use crate::rawevent::{SourceLocation, SourceType};
use crate::store::SourceKey;

/// 版本前缀 —— token 的构成规则将来改了，旧 token 不会被误当成同一次操作。
const TOKEN_VERSION: &str = "pt1";

/// 一次**开新代**投影操作的稳定身份。
///
/// 不透明：外部只能构造与比较，不能拆开。存进总库时用 [`Self::as_str`]。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectionToken(String);

impl ProjectionToken {
    /// 从决定「这次投影内容」的全部分量构造。
    ///
    /// `source_fingerprint` 是源字节的指纹（全读时算）。**`None` 表示指纹未知** ——
    /// 那种情况下 token 里放一个显式的 `nofp` 标记而不是空串：两个「指纹未知」的
    /// 操作**不该**被当成同一次，所以调用方必须另外用字节范围区分，
    /// 而 `nofp` 让这件事在 token 里看得见。
    pub fn new(
        source: &SourceKey,
        source_fingerprint: Option<&str>,
        parser_revision: u32,
        attribution_revision: i64,
        byte_range: (u64, u64),
    ) -> Self {
        let location = match &source.source_location {
            SourceLocation::Local => "local".to_string(),
            SourceLocation::Wsl(d) => format!("wsl:{d}"),
        };
        let source_type = match source.source_type {
            SourceType::ClaudeCode => "claude_code",
            SourceType::Codex => "codex",
            SourceType::Cursor => "cursor",
            SourceType::Gemini => "gemini",
            SourceType::Jsonl => "jsonl",
            // 穷举匹配：将来加 SourceType 变体时编译器会指到这里 ——
            // 新来源必须拿到自己的 token 前缀，不能悄悄复用别人的。
        };
        // 路径里可能有 `|`，所以用长度前缀而不是纯分隔符 —— 否则两个不同的
        // (路径, 指纹) 组合可以拼出同一个串。
        let fp = source_fingerprint.unwrap_or("nofp");
        Self(format!(
            "{TOKEN_VERSION}|{source_type}|{location}|{}:{}|{}:{}|{parser_revision}|{attribution_revision}|{}-{}",
            source.source_path.len(),
            source.source_path,
            fp.len(),
            fp,
            byte_range.0,
            byte_range.1,
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 从库里读回来。**不校验内容** —— 它只是一个不透明的相等性载体。
    pub fn from_stored(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for ProjectionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str) -> SourceKey {
        SourceKey {
            source_type: SourceType::ClaudeCode,
            source_location: SourceLocation::Local,
            source_path: path.to_string(),
        }
    }

    #[test]
    fn the_same_operation_yields_the_same_token() {
        let a = ProjectionToken::new(&key("/a.jsonl"), Some("fp1"), 4, 7, (0, 100));
        let b = ProjectionToken::new(&key("/a.jsonl"), Some("fp1"), 4, 7, (0, 100));
        assert_eq!(a, b, "同一次操作重放必须同 token，否则幂等无从谈起");
    }

    /// 🔴 每一个分量都必须能区分开操作。**少一个分量的后果比多开一代更糟**：
    /// 第二次操作会被当成重复而静默忽略，新解析器的结果永远进不去。
    #[test]
    fn every_component_distinguishes_the_operation() {
        let base = ProjectionToken::new(&key("/a.jsonl"), Some("fp1"), 4, 7, (0, 100));
        let variants = [
            (
                "路径",
                ProjectionToken::new(&key("/b.jsonl"), Some("fp1"), 4, 7, (0, 100)),
            ),
            (
                "指纹",
                ProjectionToken::new(&key("/a.jsonl"), Some("fp2"), 4, 7, (0, 100)),
            ),
            (
                "parser",
                ProjectionToken::new(&key("/a.jsonl"), Some("fp1"), 5, 7, (0, 100)),
            ),
            (
                "attribution",
                ProjectionToken::new(&key("/a.jsonl"), Some("fp1"), 4, 8, (0, 100)),
            ),
            (
                "字节范围",
                ProjectionToken::new(&key("/a.jsonl"), Some("fp1"), 4, 7, (0, 200)),
            ),
        ];
        for (what, v) in variants {
            assert_ne!(base, v, "{what}变了就是另一次操作");
        }
    }

    #[test]
    fn source_type_and_location_are_part_of_the_identity() {
        let mut wsl = key("/a.jsonl");
        wsl.source_location = SourceLocation::Wsl("Ubuntu".into());
        let mut codex = key("/a.jsonl");
        codex.source_type = SourceType::Codex;
        let base = ProjectionToken::new(&key("/a.jsonl"), Some("fp1"), 4, 7, (0, 100));
        assert_ne!(
            base,
            ProjectionToken::new(&wsl, Some("fp1"), 4, 7, (0, 100))
        );
        assert_ne!(
            base,
            ProjectionToken::new(&codex, Some("fp1"), 4, 7, (0, 100))
        );
    }

    /// 🔴 长度前缀不是装饰：路径与指纹里都可能出现分隔符，
    /// 纯拼接会让两个不同的组合拼出同一个串。
    #[test]
    fn a_separator_inside_a_component_cannot_forge_another_token() {
        let a = ProjectionToken::new(&key("/a|4|7"), Some("fp"), 4, 7, (0, 1));
        let b = ProjectionToken::new(&key("/a"), Some("4|7|fp"), 4, 7, (0, 1));
        assert_ne!(a, b, "分量里的分隔符不得伪造出另一个 token");
    }

    #[test]
    fn an_unknown_fingerprint_is_marked_not_blank() {
        let t = ProjectionToken::new(&key("/a.jsonl"), None, 4, 7, (0, 100));
        assert!(t.as_str().contains("nofp"), "指纹未知要看得见：{t}");
        assert_ne!(
            t,
            ProjectionToken::new(&key("/a.jsonl"), Some(""), 4, 7, (0, 100)),
            "「未知」与「空指纹」不是一回事"
        );
    }
}

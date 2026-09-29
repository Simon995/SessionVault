//! 会话自己记下的仓库远端 —— 目录搬走或删掉之后，项目身份唯一的机械线索。
//!
//! 🔴 出处是**会话记录**，不是对 checkout 的探测，所以不写进 `project_identity`
//! （那张表的身份是根的属性）。id 经 [`crate::identity::git_id_of_remote`] 算，
//! 与注册根同一个出口 ⇒ 同一个远端得到同一个 `git:` id。
//!
//! 只有 codex 会话记远端（`session_meta.payload.git.repository_url`）；
//! Claude Code 的日志只记分支。

use std::path::Path;

use serde_json::Value;

use crate::deadline::Deadline;
use crate::probe::Probed;
use crate::rawevent::SourceLocation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOrigin {
    Recorded {
        repository_url: String,
        canonical_id: String,
    },
    /// 读到了会话，它没记（或记了个规范化不出来的）远端。
    NotRecorded,
    /// 会话源文件已经不在。
    SourceMissing,
    /// 没读成 —— 不是「没记」。
    Unreadable(String),
}

/// 读一个 codex 会话的源文件，取**这个会话**记下的远端。
pub fn read_codex_origin(
    location: &SourceLocation,
    path: &str,
    session_id: &str,
    deadline: Deadline,
) -> SessionOrigin {
    let text = match location {
        SourceLocation::Local => match crate::probe::read_text(Path::new(path), None) {
            Probed::Found(t) => t,
            Probed::Absent => return SessionOrigin::SourceMissing,
            Probed::Unknown(e) => return SessionOrigin::Unreadable(e.to_string()),
        },
        SourceLocation::Wsl(distro) => match crate::wsl::read_file_at(distro, path, deadline) {
            Ok(Some(t)) => t,
            Ok(None) => return SessionOrigin::SourceMissing,
            Err(e) => return SessionOrigin::Unreadable(e),
        },
    };
    codex_origin(&text, session_id)
}

/// 在 codex rollout 全文里找这个会话的 `session_meta`。一个文件可含多个
/// 会话（见 parser 的黄金语料），所以按 `payload.id` 对号，不取第一条。
pub fn codex_origin(text: &str, session_id: &str) -> SessionOrigin {
    for line in text.lines().filter(|l| l.contains("\"session_meta\"")) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("session_meta")
            || v.pointer("/payload/id").and_then(Value::as_str) != Some(session_id)
        {
            continue;
        }
        let url = v
            .pointer("/payload/git/repository_url")
            .and_then(Value::as_str);
        return match url.and_then(|u| Some((u, crate::identity::git_id_of_remote(u)?))) {
            Some((u, id)) => SessionOrigin::Recorded {
                repository_url: u.to_string(),
                canonical_id: id,
            },
            None => SessionOrigin::NotRecorded,
        };
    }
    SessionOrigin::NotRecorded
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn meta(id: &str, git: Value) -> String {
        serde_json::json!({"type": "session_meta", "payload": {"id": id, "cwd": "/w/p", "git": git}})
            .to_string()
    }

    /// 判据是「与新家注册后的 id 相等」—— 所以对照走注册根的真实路径，
    /// 且 checkout 里用另一种写法记同一个远端。
    #[test]
    fn the_recorded_remote_gets_the_same_id_as_a_registered_root() {
        let root = std::env::temp_dir().join(format!("sv-origin-root-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git").join("config"),
            "[remote \"origin\"]\n\turl = https://example.com/team/repo/\n",
        )
        .unwrap();
        let root_id = crate::identity::canonical_repo_id(&root).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        let url = "git@example.com:Team/Repo.git";
        let text = meta("s1", serde_json::json!({"repository_url": url}));
        assert_eq!(
            codex_origin(&text, "s1"),
            SessionOrigin::Recorded {
                repository_url: url.to_string(),
                canonical_id: root_id,
            }
        );
    }

    /// 两个会话在同一个文件里、各记各的远端 —— 取第一条的实现在这里会错。
    #[test]
    fn each_session_gets_its_own_remote() {
        let text = [
            meta(
                "a",
                serde_json::json!({"repository_url": "https://example.com/o/first"}),
            ),
            r#"{"type":"response_item","payload":{}}"#.to_string(),
            meta(
                "b",
                serde_json::json!({"repository_url": "https://example.com/o/second"}),
            ),
        ]
        .join("\n");
        let id_of = |s| match codex_origin(&text, s) {
            SessionOrigin::Recorded { canonical_id, .. } => canonical_id,
            other => panic!("{s}: {other:?}"),
        };
        assert_eq!(id_of("a"), "git:example.com/o/first");
        assert_eq!(id_of("b"), "git:example.com/o/second");
    }

    #[test]
    fn no_usable_remote_is_not_recorded() {
        for git in [
            serde_json::json!(null),
            serde_json::json!({"branch": "main"}),
            serde_json::json!({"repository_url": "  "}),
        ] {
            assert_eq!(
                codex_origin(&meta("s", git.clone()), "s"),
                SessionOrigin::NotRecorded,
                "{git}"
            );
        }
        let other = meta(
            "x",
            serde_json::json!({"repository_url": "https://example.com/o/r"}),
        );
        assert_eq!(
            codex_origin(&other, "s"),
            SessionOrigin::NotRecorded,
            "别的会话的远端不算"
        );
    }

    #[test]
    fn a_missing_file_and_an_unreadable_one_are_told_apart() {
        let dir = std::env::temp_dir().join(format!("sv-origin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("rollout.jsonl");
        std::fs::write(
            &file,
            meta(
                "s",
                serde_json::json!({"repository_url": "https://example.com/o/r"}),
            ),
        )
        .unwrap();
        let read = |p: &Path| {
            read_codex_origin(
                &SourceLocation::Local,
                &p.to_string_lossy(),
                "s",
                Deadline::unbounded(),
            )
        };

        assert!(matches!(read(&file), SessionOrigin::Recorded { .. }));
        assert_eq!(read(&dir.join("gone.jsonl")), SessionOrigin::SourceMissing);
        // 目录当文件读：读失败，不是「不在」也不是「没记」。
        assert!(matches!(read(&dir), SessionOrigin::Unreadable(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

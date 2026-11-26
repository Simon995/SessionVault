//! Windows Antigravity CLI：只读已知目录形态与会话摘要。转录与摘要是两个输入。

use std::path::{Path, PathBuf};

#[cfg(all(windows, feature = "store"))]
mod summary;

use crate::catalog::{Artifact, ProviderDescriptor, Status};
use crate::deadline::Deadline;
use crate::discover::{DiscoveryOutcome, SourceRef, LOCAL_LOCATION};
use crate::logging::tag;
use crate::probe::{self, Probed, RealEntryKind};
use crate::rawevent::{SourceLocation, SourceMode, SourceType};

const BRAIN: &str = "brain";
const GENERATED: &str = ".system_generated";
const LOGS: &str = "logs";
const TRANSCRIPT: &str = "transcript.jsonl";
const SUMMARY_DB: &str = "conversation_summaries.db";
const DATA_ROOT_ENV: &str = "SVAULT_ANTIGRAVITY_CLI_DIR";
const MAX_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        source_type: SourceType::AntigravityCli,
        name: "Antigravity CLI".to_string(),
        config_dir: config_dir(),
        artifacts: vec![Artifact {
            subdir: BRAIN.to_string(),
            glob: format!("*/{GENERATED}/{LOGS}/{TRANSCRIPT}"),
            source_mode: SourceMode::AppendLog,
            status: Status::Experimental,
            recursive: true,
            kind: None,
            may_be_compressed: false,
        }],
    }
}

pub(crate) fn config_dir() -> Option<PathBuf> {
    if !cfg!(all(windows, feature = "store")) {
        return None;
    }
    std::env::var_os(DATA_ROOT_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn discover_configured(deadline: Deadline) -> crate::Result<DiscoveryOutcome> {
    if !cfg!(all(windows, feature = "store")) {
        return Err(crate::Error::Parse(
            "Antigravity CLI discovery requires a Windows store build".into(),
        ));
    }
    let Some(root) = config_dir() else {
        return Ok(DiscoveryOutcome::default());
    };
    discover(&root, deadline).map_err(crate::Error::Parse)
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// 路径与 ID 是同一条形状判据，不能拿两个同名日志猜来源。
pub(crate) fn transcript_layout(path: &Path) -> Option<(&Path, &str)> {
    let logs = path.parent()?;
    let generated = logs.parent()?;
    let session = generated.parent()?;
    let brain = session.parent()?;
    let id = session.file_name()?.to_str()?;
    (path.file_name()? == TRANSCRIPT
        && logs.file_name()? == LOGS
        && generated.file_name()? == GENERATED
        && brain.file_name()? == BRAIN
        && valid_id(id))
    .then(|| (brain.parent().expect("brain has a name"), id))
}

fn real_dir(path: &Path, root: &Path) -> Result<bool, String> {
    match probe::lstat_entry(path, Some(root)) {
        Probed::Found(RealEntryKind::Directory) => Ok(true),
        Probed::Absent => Ok(false),
        Probed::Found(_) => Err("Antigravity CLI directory is not a real directory".into()),
        Probed::Unknown(error) => Err(error.to_string()),
    }
}

fn within_deadline(deadline: Deadline) -> Result<(), String> {
    if deadline.expired() {
        Err("Antigravity CLI scan deadline exhausted".into())
    } else {
        Ok(())
    }
}

fn valid_root(root: &Path) -> bool {
    if !root.is_absolute()
        || root
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return false;
    }
    #[cfg(windows)]
    if !matches!(root.components().next(), Some(std::path::Component::Prefix(p)) if matches!(p.kind(), std::path::Prefix::Disk(_)))
    {
        return false;
    }
    true
}

pub(crate) fn discover(root: &Path, deadline: Deadline) -> Result<DiscoveryOutcome, String> {
    within_deadline(deadline)?;
    if !valid_root(root) {
        return Err(format!(
            "{DATA_ROOT_ENV} must be an absolute local directory"
        ));
    }
    let mut outcome = DiscoveryOutcome::default();
    let brain = root.join(BRAIN);
    if !real_dir(root, root)? || !real_dir(&brain, root)? {
        return Ok(outcome);
    }
    let entries = match probe::read_dir_entries(&brain, Some(root)) {
        Probed::Found(entries) => entries,
        Probed::Absent => return Ok(outcome),
        Probed::Unknown(error) => return Err(error.to_string()),
    };
    for entry in entries {
        if let Err(error) = within_deadline(deadline) {
            report_incomplete(&mut outcome, &error);
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report_incomplete(&mut outcome, &error);
                continue;
            }
        };
        if !entry.file_name.to_str().is_some_and(valid_id) {
            continue;
        }
        match discover_transcript(&entry.path, root) {
            Ok(Some(path)) => outcome.sources.push(SourceRef {
                source_type: SourceType::AntigravityCli,
                source_location: SourceLocation::Local,
                source_mode: SourceMode::AppendLog,
                path,
                project_root: None,
                artifact_kind: None,
            }),
            Ok(None) => {}
            Err(error) => {
                report_incomplete(
                    &mut outcome,
                    format_args!("{}: {error}", entry.path.display()),
                );
            }
        }
    }
    Ok(outcome)
}

fn discover_transcript(session: &Path, root: &Path) -> Result<Option<PathBuf>, String> {
    let generated = session.join(GENERATED);
    let logs = generated.join(LOGS);
    if !real_dir(session, root)? || !real_dir(&generated, root)? || !real_dir(&logs, root)? {
        return Ok(None);
    }
    let path = logs.join(TRANSCRIPT);
    match probe::lstat_entry(&path, Some(root)) {
        Probed::Found(RealEntryKind::RegularFile(_)) => Ok(Some(path)),
        Probed::Absent => Ok(None),
        Probed::Found(_) => Err("Antigravity CLI transcript is not a regular file".into()),
        Probed::Unknown(error) => Err(error.to_string()),
    }
}

fn report_incomplete(outcome: &mut DiscoveryOutcome, error: impl std::fmt::Display) {
    log::warn!(target: tag::DISCOVER, "Antigravity CLI discovery incomplete at {LOCAL_LOCATION}: {error}");
    if outcome.unreachable.is_empty() {
        outcome.unreachable.push(LOCAL_LOCATION.to_string());
    }
}

pub(crate) struct Input {
    pub bytes: Vec<u8>,
    pub attribution: crate::rawevent::EventAttribution,
}

pub(crate) fn read_input(
    source: &SourceRef,
    roots: &crate::attribution::RootRegistry,
    deadline: Deadline,
) -> Result<Input, String> {
    within_deadline(deadline)?;
    if !cfg!(all(windows, feature = "store")) || source.source_location != SourceLocation::Local {
        return Err("Antigravity CLI scanning requires a local Windows store build".into());
    }
    let (root, id) =
        transcript_layout(&source.path).ok_or("invalid Antigravity CLI transcript path")?;
    if !valid_root(root) || !real_dir(root, root)? {
        return Err("Antigravity CLI data root unavailable".into());
    }
    let dirs = source
        .path
        .parent()
        .and_then(|parent| parent.strip_prefix(root).ok())
        .ok_or("invalid Antigravity CLI transcript ancestry")?;
    let mut path = root.to_path_buf();
    for component in dirs.components() {
        path.push(component);
        if !real_dir(&path, root)? {
            return Err("Antigravity CLI transcript directory unavailable".into());
        }
    }
    let paths = workspace_paths(root, id)?;
    within_deadline(deadline)?;
    let bytes = match probe::read_pinned_regular_file(&source.path, MAX_TRANSCRIPT_BYTES, Some(root)) {
        Probed::Found(probe::PinnedRead::Complete { bytes, .. }) => bytes,
        Probed::Found(_) | Probed::Absent => return Err("Antigravity CLI transcript unavailable, changed during read, or above the 64 MiB read limit".into()),
        Probed::Unknown(error) => return Err(error.to_string()),
    };
    Ok(Input {
        bytes,
        attribution: workspace_attribution(&paths, roots),
    })
}

fn workspace_attribution(
    paths: &[String],
    roots: &crate::attribution::RootRegistry,
) -> crate::rawevent::EventAttribution {
    let unknown = crate::rawevent::EventAttribution {
        project_root: None,
        project_root_source: None,
        workspace_location: None,
    };
    let mut mapped = paths.iter().map(|path| {
        crate::parser::attribution_from_cwd(
            Some(path),
            &SourceLocation::Local,
            crate::pathnorm::HostPlatform::Windows,
            None,
            roots,
            None,
        )
    });
    let Some(first) = mapped.next() else {
        return unknown;
    };
    if first.project_root.is_some()
        && first.project_root_source.as_deref() != Some("unattributed")
        && mapped.all(|next| next == first)
    {
        first
    } else {
        unknown
    }
}

#[cfg(not(all(windows, feature = "store")))]
fn workspace_paths(_: &Path, _: &str) -> Result<Vec<String>, String> {
    Err(format!("{SUMMARY_DB} requires a Windows store build"))
}

#[cfg(all(windows, feature = "store"))]
fn workspace_paths(root: &Path, id: &str) -> Result<Vec<String>, String> {
    let Some(value) = summary::read(root, id)?.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let uris: Vec<String> = serde_json::from_str(&value)
        .map_err(|_| "Antigravity CLI workspace_uris is not a JSON array of strings")?;
    Ok(uris
        .iter()
        .map(|uri| windows_file_path(uri))
        .collect::<Option<_>>()
        .unwrap_or_default())
}

#[cfg(all(feature = "store", any(windows, test)))]
fn windows_file_path(uri: &str) -> Option<String> {
    let (authority, path) = uri.strip_prefix("file://")?.split_once('/')?;
    if path.contains(['\\', '?', '#']) {
        return None;
    }
    let mut parts = path.split('/');
    let first = parts.next()?;
    let drive = authority.is_empty() || authority.eq_ignore_ascii_case("localhost");
    let mut path = if drive {
        let letter = first
            .strip_suffix(':')
            .or_else(|| first.strip_suffix("%3A"))
            .or_else(|| first.strip_suffix("%3a"))?;
        if letter.len() != 1 || !letter.as_bytes()[0].is_ascii_alphabetic() {
            return None;
        }
        format!("{letter}:/")
    } else {
        if !authority
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
            || matches!(authority, "." | "..")
            || first.is_empty()
        {
            return None;
        }
        format!("//{authority}/{}", uri_component(first)?)
    };
    for part in parts {
        if !part.is_empty() {
            if !path.ends_with('/') {
                path.push('/');
            }
            path.push_str(&uri_component(part)?);
        }
    }
    Some(path)
}

#[cfg(all(feature = "store", any(windows, test)))]
fn uri_component(part: &str) -> Option<String> {
    let mut rest = part.as_bytes().iter().copied();
    let mut decoded = Vec::with_capacity(part.len());
    while let Some(byte) = rest.next() {
        let byte = if byte == b'%' {
            let hi = char::from(rest.next()?).to_digit(16)?;
            let lo = char::from(rest.next()?).to_digit(16)?;
            (hi * 16 + lo) as u8
        } else {
            byte
        };
        if byte.is_ascii_control()
            || matches!(
                byte,
                b'/' | b'\\' | b':' | b'<' | b'>' | b'"' | b'|' | b'?' | b'*'
            )
        {
            return None;
        }
        decoded.push(byte);
    }
    let decoded = String::from_utf8(decoded).ok()?;
    if decoded.trim_end_matches(['.', ' ']).is_empty() || decoded.ends_with(['.', ' ']) {
        return None;
    }
    Some(decoded)
}

#[cfg(all(test, feature = "store"))]
mod tests {
    use super::*;
    use crate::attribution::{RootRegistry, RootSource};

    #[test]
    fn decodes_only_absolute_windows_file_uris() {
        for (uri, expected) in [
            ("file:///C:/repo/My%20Work", "C:/repo/My Work"),
            ("file:///c%3a/%E9%A1%B9%E7%9B%AE", "c:/项目"),
            (
                "file://wsl.localhost/Distro/home/dev/repo",
                "//wsl.localhost/Distro/home/dev/repo",
            ),
            ("file://localhost/D:/", "D:/"),
        ] {
            assert_eq!(windows_file_path(uri).as_deref(), Some(expected));
        }
        for uri in [
            "file:///repo",
            "file:///C:/%2e%2e/repo",
            "file:///C:/a%5Cb",
            "file:///C:/a%2Fb",
            "file:///C:/a%00b",
            "file:///C:/a%",
            "ssh://host/a",
            "file:///C:/a#fragment",
            "file://./device",
        ] {
            assert_eq!(windows_file_path(uri), None, "{uri}");
        }
    }

    #[test]
    fn all_workspaces_must_agree_on_a_registered_root() {
        let roots = RootRegistry::from_roots([
            ("C:/repos/alpha", RootSource::Git),
            ("C:/repos/beta", RootSource::Git),
        ]);
        let attr = |paths: &[&str]| {
            workspace_attribution(
                &paths.iter().map(ToString::to_string).collect::<Vec<_>>(),
                &roots,
            )
        };
        assert_eq!(
            attr(&["C:/repos/alpha/a", "C:/repos/alpha/b"])
                .project_root
                .as_deref(),
            Some("C:/repos/alpha")
        );
        assert_eq!(
            attr(&["C:/repos/alpha", "C:/repos/beta"]).project_root,
            None
        );
        assert_eq!(attr(&["C:/repos/alpha", "C:/unknown"]).project_root, None);
        assert_eq!(attr(&["C:/unknown"]).project_root, None);
        assert_eq!(attr(&[]).project_root, None);
    }
}

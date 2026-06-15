//! `parity` —— P2 影子并跑 diff 工具（见 `docs/parity-contract.md`）。
//!
//! 比对 **QuotaBar `cache.db` 的 `usage_facts`**（冻结基线 NDJSON）与 **SessionVault
//! `svault scan-all` 输出**（NDJSON 事件流）的 `event_type=usage` 子集，证明抽取后的解析器
//! 复现 QuotaBar 计费用量字节级一致——绞杀者迁移「一致才切」的回归权威。
//!
//! 对齐策略（契约 §3 / §8）：
//! - 按**文件桶** `(provider, location, source_path)` **序数对齐**：两边各按自身 `seq` 升序，
//!   第 N 条对第 N 条。`seq` **值不可跨系统比**（坐标系不同：QuotaBar=文件行号，SessionVault=
//!   已发射事件序），只用于各自排序。
//! - **must-match**（任一不符即失败、非 0 退出）：四段 token、`model`、`effort`、`message_id`、
//!   `request_id`、`session_id`。
//! - **advisory**（记录、不判失败）：`cwd`、`project_root`（两边规范化不同源，可能是 SV 的宿主感知改进）。
//! - **增长尾**：活跃文件在快照后追加 → SessionVault 多出的尾部 usage 按 `agent_session_files.size`
//!   vs 现盘 size 归类为「增长（informational）」而非分歧。
//!
//! 用法：
//! ```text
//! cargo run --features parity --bin parity -- \
//!   --quotabar baseline/quotabar_usage_facts.ndjson \
//!   --sessionvault scan.ndjson \
//!   [--files baseline/quotabar_agent_session_files.ndjson] \
//!   [--report parity_report.json] [--max-show 20]
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use session_vault::rawevent::{EventType, RawEvent, SourceType};

#[derive(Parser)]
#[command(name = "parity", about = "QuotaBar usage_facts ⇄ SessionVault RawEvent parity diff (P2)")]
struct Cli {
    /// QuotaBar `usage_facts` 冻结基线（NDJSON，逐行一 fact）。
    #[arg(long)]
    quotabar: PathBuf,
    /// SessionVault `svault scan-all` 输出（NDJSON 事件流）。
    #[arg(long)]
    sessionvault: PathBuf,
    /// QuotaBar `agent_session_files`（NDJSON）——用于增长尾归类（可选）。
    #[arg(long)]
    files: Option<PathBuf>,
    /// 写结构化 JSON 报告到此路径（可选）。
    #[arg(long)]
    report: Option<PathBuf>,
    /// 最多打印多少条 must-match 不符示例。
    #[arg(long, default_value_t = 20)]
    max_show: usize,
}

/// QuotaBar `usage_facts` 行（基线 NDJSON）。
#[derive(Debug, Deserialize)]
struct QbFact {
    provider: String,
    location: String,
    source_path: String,
    seq: i64,
    session_id: String,
    model: Option<String>,
    effort: Option<String>,
    cwd: Option<String>,
    project_root: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    message_id: Option<String>,
    request_id: Option<String>,
}

/// QuotaBar `agent_session_files` 行（取 path→size）。
#[derive(Debug, Deserialize)]
struct QbFile {
    path: String,
    size: i64,
}

/// 归一比对单元（两边投影到同一形状）。
#[derive(Debug, Clone)]
struct Fact {
    seq: i64,
    session_id: String,
    model: Option<String>,
    effort: Option<String>,
    cwd: Option<String>,
    project_root: Option<String>,
    input: i64,
    output: i64,
    cache_creation: i64,
    cache_read: i64,
    message_id: Option<String>,
    request_id: Option<String>,
}

impl From<QbFact> for Fact {
    fn from(q: QbFact) -> Self {
        Fact {
            seq: q.seq,
            session_id: q.session_id,
            model: q.model,
            effort: q.effort,
            cwd: q.cwd,
            project_root: q.project_root,
            input: q.input_tokens,
            output: q.output_tokens,
            cache_creation: q.cache_creation_tokens,
            cache_read: q.cache_read_tokens,
            message_id: q.message_id,
            request_id: q.request_id,
        }
    }
}

/// 桶键：`(provider, location, source_path)`。
type Key = (String, String, String);

#[derive(Debug, Default, Serialize)]
struct ComboStat {
    qb_total: usize,
    sv_total: usize,
    prefix_compared: usize,
    must_mismatch: usize,
    advisory_diff: usize,
    sv_growth_tail: usize,
    sv_extra_unknown: usize,
    qb_extra: usize,
}

#[derive(Debug, Serialize)]
struct FieldDiff {
    provider: String,
    location: String,
    source_path: String,
    ordinal: usize,
    field: String,
    qb: String,
    sv: String,
}

#[derive(Debug, Serialize)]
struct Report {
    quotabar_facts: usize,
    sessionvault_usage: usize,
    combos: BTreeMap<String, ComboStat>,
    total_must_mismatch: usize,
    total_advisory_diff: usize,
    total_sv_growth_tail: usize,
    total_sv_extra_unknown: usize,
    total_qb_extra: usize,
    must_examples: Vec<FieldDiff>,
    advisory_examples: Vec<FieldDiff>,
}

fn provider_key(t: SourceType) -> &'static str {
    match t {
        SourceType::ClaudeCode => "claude",
        SourceType::Codex => "codex",
        SourceType::Cursor => "cursor",
        SourceType::Gemini => "gemini",
        SourceType::Jsonl => "jsonl",
    }
}

fn os(o: &Option<String>) -> String {
    o.clone().unwrap_or_else(|| "∅".to_string())
}

/// 比较一对已对齐的 fact，返回 (must-match 不符, advisory 差异)。
fn cmp_facts(key: &Key, ordinal: usize, qb: &Fact, sv: &Fact) -> (Vec<FieldDiff>, Vec<FieldDiff>) {
    let mut must = Vec::new();
    let mut adv = Vec::new();
    let mk = |field: &str, a: String, b: String| FieldDiff {
        provider: key.0.clone(),
        location: key.1.clone(),
        source_path: key.2.clone(),
        ordinal,
        field: field.to_string(),
        qb: a,
        sv: b,
    };
    if qb.session_id != sv.session_id {
        must.push(mk("session_id", qb.session_id.clone(), sv.session_id.clone()));
    }
    if qb.model != sv.model {
        must.push(mk("model", os(&qb.model), os(&sv.model)));
    }
    if qb.effort != sv.effort {
        must.push(mk("effort", os(&qb.effort), os(&sv.effort)));
    }
    if qb.input != sv.input {
        must.push(mk("input_tokens", qb.input.to_string(), sv.input.to_string()));
    }
    if qb.output != sv.output {
        must.push(mk("output_tokens", qb.output.to_string(), sv.output.to_string()));
    }
    if qb.cache_creation != sv.cache_creation {
        must.push(mk("cache_creation_tokens", qb.cache_creation.to_string(), sv.cache_creation.to_string()));
    }
    if qb.cache_read != sv.cache_read {
        must.push(mk("cache_read_tokens", qb.cache_read.to_string(), sv.cache_read.to_string()));
    }
    if qb.message_id != sv.message_id {
        must.push(mk("message_id", os(&qb.message_id), os(&sv.message_id)));
    }
    if qb.request_id != sv.request_id {
        must.push(mk("request_id", os(&qb.request_id), os(&sv.request_id)));
    }
    if qb.cwd != sv.cwd {
        adv.push(mk("cwd", os(&qb.cwd), os(&sv.cwd)));
    }
    if qb.project_root != sv.project_root {
        adv.push(mk("project_root", os(&qb.project_root), os(&sv.project_root)));
    }
    (must, adv)
}

/// 去掉文件首的 UTF-8 BOM（PowerShell `Out-File -Encoding utf8` 会写 BOM）。
fn strip_bom(s: String) -> String {
    s.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(s)
}

fn load_quotabar(path: &PathBuf) -> std::io::Result<BTreeMap<Key, Vec<Fact>>> {
    let text = strip_bom(std::fs::read_to_string(path)?);
    let mut buckets: BTreeMap<Key, Vec<Fact>> = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let q: QbFact = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("quotabar line {}: {e}", i + 1));
        let key = (q.provider.clone(), q.location.clone(), q.source_path.clone());
        buckets.entry(key).or_default().push(q.into());
    }
    for v in buckets.values_mut() {
        v.sort_by_key(|f| f.seq);
    }
    Ok(buckets)
}

fn load_sessionvault(path: &PathBuf) -> std::io::Result<(BTreeMap<Key, Vec<Fact>>, usize)> {
    let text = strip_bom(std::fs::read_to_string(path)?);
    let mut buckets: BTreeMap<Key, Vec<Fact>> = BTreeMap::new();
    let mut usage_total = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // svault NDJSON 是 Out 包络：只取 kind=event 的 usage 事件。
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("kind").and_then(Value::as_str) != Some("event") {
            continue;
        }
        let Some(ev) = v.get("event") else { continue };
        let re: RawEvent = match serde_json::from_value(ev.clone()) {
            Ok(re) => re,
            Err(_) => continue,
        };
        if re.event_type != EventType::Usage {
            continue;
        }
        usage_total += 1;
        let u = re.usage.unwrap_or_default();
        let fact = Fact {
            seq: re.seq as i64,
            session_id: re.source_session_id.clone(),
            model: re.model.clone(),
            effort: re.effort.clone(),
            cwd: re.cwd.clone(),
            project_root: re.project_root.clone(),
            input: u.input as i64,
            output: u.output as i64,
            cache_creation: u.cache_creation as i64,
            cache_read: u.cache_read as i64,
            message_id: re.message_id.clone(),
            request_id: re.request_id.clone(),
        };
        let key = (
            provider_key(re.source_type).to_string(),
            re.source_location.as_key(),
            re.source_path.clone(),
        );
        buckets.entry(key).or_default().push(fact);
    }
    for v in buckets.values_mut() {
        v.sort_by_key(|f| f.seq);
    }
    Ok((buckets, usage_total))
}

fn load_files(path: &Option<PathBuf>) -> BTreeMap<String, i64> {
    let mut map = BTreeMap::new();
    let Some(p) = path else { return map };
    let Ok(text) = std::fs::read_to_string(p) else {
        return map;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(f) = serde_json::from_str::<QbFile>(line) {
            map.insert(f.path, f.size);
        }
    }
    map
}

/// 增长尾归类：QuotaBar 记录 size < 现盘 size（append-only 增长）→ growth；否则 unknown。
/// WSL Linux 路径在 Windows 上无法 stat → unknown（实测 WSL combo 无尾，安全）。
fn is_growth(path: &str, qb_files: &BTreeMap<String, i64>) -> bool {
    let Some(&qb_size) = qb_files.get(path) else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    qb_size < meta.len() as i64
}

fn combo_label(key: &Key) -> String {
    format!("{}|{}", key.0, key.1)
}

fn main() {
    let cli = Cli::parse();

    let qb = load_quotabar(&cli.quotabar).expect("read quotabar baseline");
    let (sv, sv_usage_total) = load_sessionvault(&cli.sessionvault).expect("read sessionvault ndjson");
    let qb_files = load_files(&cli.files);
    let qb_total: usize = qb.values().map(Vec::len).sum();

    // 所有桶键的并集。
    let mut keys: Vec<Key> = qb.keys().cloned().collect();
    for k in sv.keys() {
        if !qb.contains_key(k) {
            keys.push(k.clone());
        }
    }
    keys.sort();

    let mut combos: BTreeMap<String, ComboStat> = BTreeMap::new();
    let mut must_examples: Vec<FieldDiff> = Vec::new();
    let mut advisory_examples: Vec<FieldDiff> = Vec::new();
    let empty: Vec<Fact> = Vec::new();

    for key in &keys {
        let qf = qb.get(key).unwrap_or(&empty);
        let sf = sv.get(key).unwrap_or(&empty);
        let stat = combos.entry(combo_label(key)).or_default();
        stat.qb_total += qf.len();
        stat.sv_total += sf.len();

        let common = qf.len().min(sf.len());
        stat.prefix_compared += common;
        for i in 0..common {
            let (must, adv) = cmp_facts(key, i, &qf[i], &sf[i]);
            stat.must_mismatch += must.len();
            stat.advisory_diff += adv.len();
            for d in must {
                if must_examples.len() < cli.max_show {
                    must_examples.push(d);
                }
            }
            for d in adv {
                if advisory_examples.len() < cli.max_show {
                    advisory_examples.push(d);
                }
            }
        }

        // 尾部归类。
        if sf.len() > qf.len() {
            let extra = sf.len() - qf.len();
            if is_growth(&key.2, &qb_files) {
                stat.sv_growth_tail += extra;
            } else {
                stat.sv_extra_unknown += extra;
            }
        }
        if qf.len() > sf.len() {
            stat.qb_extra += qf.len() - sf.len();
        }
    }

    let report = Report {
        quotabar_facts: qb_total,
        sessionvault_usage: sv_usage_total,
        total_must_mismatch: combos.values().map(|c| c.must_mismatch).sum(),
        total_advisory_diff: combos.values().map(|c| c.advisory_diff).sum(),
        total_sv_growth_tail: combos.values().map(|c| c.sv_growth_tail).sum(),
        total_sv_extra_unknown: combos.values().map(|c| c.sv_extra_unknown).sum(),
        total_qb_extra: combos.values().map(|c| c.qb_extra).sum(),
        combos,
        must_examples,
        advisory_examples,
    };

    print_human(&report);

    let mut report_io_failed = false;
    if let Some(rp) = &cli.report {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => {
                if let Err(e) = std::fs::write(rp, s) {
                    eprintln!("write report failed: {e}");
                    report_io_failed = true;
                }
            }
            Err(e) => {
                eprintln!("serialize report failed: {e}");
                report_io_failed = true;
            }
        }
    }

    std::process::exit(exit_code(&report, report_io_failed));
}

/// 退出码判定（纯函数，便于测试）。
///
/// 红线（判败 → 1）：
///   - `total_must_mismatch` > 0：must-match 字段不符（parity-contract §3/§6）。
///   - `total_qb_extra` > 0：SV 漏发了 QB 有的 usage（整文件 / 桶缺失也落这里）。
///     空的 SV 输出会让每个 combo 全进 qb_extra → 判败，避免被自动化误当绿灯。
///   - `total_sv_extra_unknown` > 0：SV 多发且非增长尾、未归类的 usage（疑似重复 / 过度提取）。
/// 非判败：`sv_growth_tail`（SV 扫到冻结基线之后的新增数据，合法）/ advisory（ts/cwd/root）。
///
/// `report_io_failed`：传了 `--report` 但序列化 / 写入失败 → 2，避免调用方误以为报告已生成。
/// parity 红线优先于 report I/O。
fn exit_code(report: &Report, report_io_failed: bool) -> i32 {
    let parity_failed = report.total_must_mismatch > 0
        || report.total_qb_extra > 0
        || report.total_sv_extra_unknown > 0;
    if parity_failed {
        1
    } else if report_io_failed {
        2
    } else {
        0
    }
}

fn print_human(r: &Report) {
    println!("== parity: QuotaBar usage_facts ⇄ SessionVault RawEvent(usage) ==");
    println!("quotabar_facts={}  sessionvault_usage={}", r.quotabar_facts, r.sessionvault_usage);
    println!();
    println!(
        "{:<34} {:>6} {:>6} {:>7} {:>5} {:>4} {:>7} {:>6} {:>5}",
        "combo", "qb", "sv", "prefix", "MUST", "adv", "growth", "sv_ext", "qb_ex"
    );
    for (label, c) in &r.combos {
        println!(
            "{:<34} {:>6} {:>6} {:>7} {:>5} {:>4} {:>7} {:>6} {:>5}",
            label,
            c.qb_total,
            c.sv_total,
            c.prefix_compared,
            c.must_mismatch,
            c.advisory_diff,
            c.sv_growth_tail,
            c.sv_extra_unknown,
            c.qb_extra,
        );
    }
    println!();
    println!(
        "TOTAL must_mismatch={} advisory_diff={} sv_growth_tail={} sv_extra_unknown={} qb_extra={}",
        r.total_must_mismatch,
        r.total_advisory_diff,
        r.total_sv_growth_tail,
        r.total_sv_extra_unknown,
        r.total_qb_extra,
    );
    if !r.must_examples.is_empty() {
        println!("\n-- must-match 不符示例（最多 {} 条）--", r.must_examples.len());
        for d in &r.must_examples {
            println!(
                "[{}|{}] {} #{} {}: qb={} sv={}",
                d.provider,
                d.location,
                shorten(&d.source_path),
                d.ordinal,
                d.field,
                d.qb,
                d.sv,
            );
        }
    }
    if !r.advisory_examples.is_empty() {
        println!("\n-- advisory 差异示例（最多 {} 条）--", r.advisory_examples.len());
        for d in &r.advisory_examples {
            println!(
                "[{}|{}] {} #{} {}: qb={} sv={}",
                d.provider,
                d.location,
                shorten(&d.source_path),
                d.ordinal,
                d.field,
                d.qb,
                d.sv,
            );
        }
    }
    let verdict = if r.total_must_mismatch == 0 {
        "PASS (must-match 全绿；增长尾/advisory 见上)"
    } else {
        "FAIL (存在 must-match 不符)"
    };
    println!("\n结论：{verdict}");
}

fn shorten(path: &str) -> String {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact() -> Fact {
        Fact {
            seq: 0,
            session_id: "s".into(),
            model: Some("m".into()),
            effort: None,
            cwd: Some("/raw".into()),
            project_root: Some("/raw".into()),
            input: 10,
            output: 5,
            cache_creation: 0,
            cache_read: 2,
            message_id: Some("mid".into()),
            request_id: Some("rid".into()),
        }
    }
    fn key() -> Key {
        ("codex".into(), "wsl:D".into(), "/p.jsonl".into())
    }

    #[test]
    fn identical_facts_no_diff() {
        let (must, adv) = cmp_facts(&key(), 0, &fact(), &fact());
        assert!(must.is_empty());
        assert!(adv.is_empty());
    }

    #[test]
    fn token_mismatch_is_must() {
        let mut sv = fact();
        sv.input = 11;
        let (must, adv) = cmp_facts(&key(), 0, &fact(), &sv);
        assert_eq!(must.len(), 1);
        assert_eq!(must[0].field, "input_tokens");
        assert!(adv.is_empty());
    }

    #[test]
    fn cwd_diff_is_advisory_not_must() {
        // SV 存原始 cwd、QB 存规范化 cwd —— 设计差异，应进 advisory 而非 must-match。
        let mut sv = fact();
        sv.cwd = Some("wsl:D:/raw".into());
        let (must, adv) = cmp_facts(&key(), 0, &fact(), &sv);
        assert!(must.is_empty(), "cwd 差异不该判 must-match 失败");
        assert_eq!(adv.len(), 1);
        assert_eq!(adv[0].field, "cwd");
    }

    #[test]
    fn model_and_ids_are_must() {
        let mut sv = fact();
        sv.model = Some("other".into());
        sv.message_id = None;
        let (must, _adv) = cmp_facts(&key(), 0, &fact(), &sv);
        let fields: Vec<&str> = must.iter().map(|d| d.field.as_str()).collect();
        assert!(fields.contains(&"model"));
        assert!(fields.contains(&"message_id"));
    }

    fn report_with(must: usize, qb_extra: usize, sv_extra_unknown: usize, growth: usize) -> Report {
        Report {
            quotabar_facts: 0,
            sessionvault_usage: 0,
            combos: BTreeMap::new(),
            total_must_mismatch: must,
            total_advisory_diff: 0,
            total_sv_growth_tail: growth,
            total_sv_extra_unknown: sv_extra_unknown,
            total_qb_extra: qb_extra,
            must_examples: vec![],
            advisory_examples: vec![],
        }
    }

    #[test]
    fn clean_report_passes() {
        assert_eq!(exit_code(&report_with(0, 0, 0, 0), false), 0);
    }

    #[test]
    fn growth_tail_does_not_fail() {
        // 增长尾即便很大也不判败：SV 扫到冻结基线之后的新增数据是合法的。
        assert_eq!(exit_code(&report_with(0, 0, 0, 999), false), 0);
    }

    #[test]
    fn must_mismatch_fails() {
        assert_eq!(exit_code(&report_with(1, 0, 0, 0), false), 1);
    }

    #[test]
    fn qb_extra_fails_so_empty_sv_is_not_green() {
        // SV 漏发 QB 有的 fact（空 SV 输出 → 全进 qb_extra）必须判败，否则被自动化误当绿灯。
        assert_eq!(exit_code(&report_with(0, 3, 0, 0), false), 1);
    }

    #[test]
    fn unknown_sv_extra_fails() {
        // 非增长尾、未归类的 SV 多发（疑似重复 / 过度提取）判败。
        assert_eq!(exit_code(&report_with(0, 0, 2, 0), false), 1);
    }

    #[test]
    fn report_io_failure_fails_with_distinct_code() {
        // parity 干净但 --report 写失败 → 2（区别于 parity 红线的 1）。
        assert_eq!(exit_code(&report_with(0, 0, 0, 0), true), 2);
    }

    #[test]
    fn parity_failure_takes_precedence_over_report_io() {
        assert_eq!(exit_code(&report_with(1, 0, 0, 0), true), 1);
    }
}

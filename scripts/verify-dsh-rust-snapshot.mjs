#!/usr/bin/env node
/** 用全新合成副本对拍 Rust example、Python 帧审计和原生 v4 位置。
 * node scripts/verify-dsh-rust-snapshot.mjs --dsh-package <package.json> --rust-reader <example-binary>
 * 只写项目 target，不读真实日志、不启动 Host/LLM、不写总库；保留运行物。
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  lstat,
  mkdtemp,
  readFile,
  realpath,
  writeFile,
} from "node:fs/promises";
import { isAbsolute, join, posix, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { constants, zstdCompressSync, zstdDecompressSync } from "node:zlib";
import {
  loadDshInstallation,
  SUPPORTED_DSH_VERSION,
} from "./dsh-installed.mjs";
import limits from "./dsh-snapshot-limits.json" with { type: "json" };

const digest = (value) => createHash("sha256").update(value).digest("hex");
const line = (value) => Buffer.from(`${JSON.stringify(value)}\n`);
const checks = {};
let stage = "arguments";

function inside(root, path) {
  const rel = relative(root, path);
  assert(
    rel &&
      !isAbsolute(rel) &&
      rel !== ".." &&
      !rel.startsWith("../") &&
      !rel.startsWith("..\\"),
  );
}

async function main() {
  const { values } = parseArgs({
    options: {
      "dsh-package": { type: "string" },
      "rust-reader": { type: "string" },
      "wsl-reader": { type: "string" },
    },
  });
  assert(values["dsh-package"]);
  assert.equal(
    Number(Boolean(values["rust-reader"])) +
      Number(Boolean(values["wsl-reader"])),
    1,
  );
  const inWsl = Boolean(values["wsl-reader"]);
  const reader = inWsl ? values["wsl-reader"] : resolve(values["rust-reader"]);
  const wsl = (args) => {
    const child = spawnSync("wsl.exe", ["--exec", ...args], {
      encoding: "utf8",
      timeout: limits.audit_timeout_ms,
      maxBuffer: limits.audit_output_bytes,
    });
    assert.ifError(child.error);
    assert.equal(child.status, 0);
    return child.stdout.trim();
  };
  if (inWsl) {
    assert.equal(process.platform, "win32");
    assert(posix.isAbsolute(reader));
    wsl(["test", "-x", reader]);
  } else {
    const executable = await lstat(reader);
    assert(executable.isFile() && !executable.isSymbolicLink());
  }
  const { load } = loadDshInstallation(values["dsh-package"]);
  const format = await load("@deepseek-ai/dsh-session-format-v3-to-v4");
  const session = await load("@deepseek-ai/dsh-session");
  const codec = format.releasedV4SessionFormatCodec;
  const artifact = JSON.parse(
    await readFile(
      new URL("../tests/fixtures/dsh-v4/complete-turn.json", import.meta.url),
      "utf8",
    ),
  );
  artifact.events.find(
    (event) => event.type === "user/message",
  ).data.content[0].text = "核对中文字节位置。";
  const parts = [
    codec.encodeHeader(artifact.header, artifact.inheritedEventCount),
    ...artifact.events.map((event) => codec.encodeEvent(event)),
  ].map(line);
  const repo = await realpath(fileURLToPath(new URL("..", import.meta.url)));
  const target = join(repo, "target");
  const info = await lstat(target);
  assert(info.isDirectory() && !info.isSymbolicLink());
  inside(repo, await realpath(target));
  const run = await mkdtemp(join(target, "dsh-rust-snapshot-"));
  inside(target, await realpath(run));
  // 只换算本脚本新建的夹具根，不接收会话 cwd 或其他消费者路径。
  const wslRun = inWsl ? wsl(["wslpath", "-u", run]) : undefined;
  if (inWsl) assert(posix.isAbsolute(wslRun));
  const pack = (bytes, checksum = true, contentSize = true) =>
    zstdCompressSync(bytes, {
      params: {
        [constants.ZSTD_c_checksumFlag]: Number(checksum),
        [constants.ZSTD_c_contentSizeFlag]: Number(contentSize),
      },
    });
  const encode = (rows, compression) =>
    Buffer.concat(
      rows.map((row) => (compression === "zstd" ? pack(row) : row)),
    );
  const cases = [];
  const add = async (name, compression, bytes, expected, options = {}) => {
    const path = join(run, `${name}.bin`);
    await writeFile(path, bytes, { flag: "wx" });
    cases.push({
      name,
      compression,
      path,
      bytes,
      expected: [expected].flat(),
      ...options,
    });
  };
  for (const compression of ["none", "zstd"]) {
    const addRows = async (name, rows, expected, options = {}) =>
      add(
        `${compression}_${name}`,
        compression,
        encode(rows, compression),
        expected,
        options,
      );
    await addRows("native_complete", parts, "physically_validated", {
      native: true,
    });
    await addRows("header_only", parts.slice(0, 1), "physically_validated", {
      native: true,
    });
    await addRows(
      "whitespace_utf8",
      parts.map((row) =>
        Buffer.concat([
          Buffer.from(" \t"),
          row.subarray(0, -1),
          Buffer.from(" \r\n"),
        ]),
      ),
      "physically_validated",
      { native: true },
    );
    await addRows(
      "unknown_required_event",
      [
        ...parts,
        line({
          type: "fixture/unknown",
          seq: artifact.events.length,
          time: 25,
          data: {},
        }),
      ],
      "physically_validated",
      { native: false },
    );
    const broken = structuredClone(artifact);
    const message = broken.events.find((event) => event.type === "tool/result")
      .data.message;
    message.toolCallId = message.source.callId = "other";
    await addRows(
      "broken_call_relation",
      [
        parts[0],
        ...broken.events.map((event) => line(codec.encodeEvent(event))),
      ],
      "physically_validated",
      { native: false },
    );
    await addRows(
      "future_header",
      [line({ ...JSON.parse(parts[0]), version: 5 }), ...parts.slice(1)],
      "unsupported",
    );
    await addRows(
      "sequence_hole",
      [parts[0], line({ ...JSON.parse(parts[1]), seq: 1 })],
      "rejected",
    );
    await addRows(
      "sequence_missing",
      [parts[0], line({ type: "turn/start" })],
      "rejected",
    );
    await addRows(
      "duplicate_sequence",
      [parts[0], parts[1], parts[1]],
      "rejected",
    );
    await addRows(
      "invalid_json",
      [parts[0], Buffer.from("invalid JSON\n")],
      "invalid",
    );
    await addRows(
      "invalid_utf8",
      [parts[0], Buffer.from('{"type":"x","seq":0,"text":"\xff"}\n', "latin1")],
      "invalid",
    );
    await addRows("non_object", [parts[0], Buffer.from("[]\n")], "invalid");
    await add(
      `${compression}_empty`,
      compression,
      Buffer.alloc(0),
      "incomplete",
    );
    const complete = encode(parts, compression);
    await add(
      `${compression}_missing_final_byte`,
      compression,
      complete.subarray(0, -1),
      compression === "none" ? "incomplete" : ["incomplete", "unverified"],
    );
  }
  const header = pack(parts[0]);
  const eventFrames = parts.slice(1).map((row) => pack(row));
  const compressed = Buffer.concat([header, ...eventFrames]);
  await add(
    "zstd_repacked_events",
    "zstd",
    Buffer.concat([header, pack(Buffer.concat(parts.slice(1)))]),
    "physically_validated",
    { native: true },
  );
  await add(
    "zstd_empty_event_frames",
    "zstd",
    Buffer.concat([
      header,
      pack(Buffer.alloc(0)),
      ...eventFrames,
      pack(Buffer.alloc(0)),
    ]),
    "physically_validated",
    { native: true },
  );
  await add("zstd_empty_header", "zstd", pack(Buffer.alloc(0)), "invalid");
  await add(
    "zstd_mixed_header_frame",
    "zstd",
    Buffer.concat([
      pack(Buffer.concat(parts.slice(0, 2))),
      ...eventFrames.slice(1),
    ]),
    "invalid",
  );
  await add(
    "zstd_split_record",
    "zstd",
    Buffer.concat([
      header,
      pack(parts[1].subarray(0, 5)),
      pack(parts[1].subarray(5)),
    ]),
    "invalid",
  );
  await add(
    "zstd_unterminated_record",
    "zstd",
    Buffer.concat([header, pack(parts[1].subarray(0, -1))]),
    "invalid",
  );
  await add(
    "zstd_without_checksum",
    "zstd",
    Buffer.concat(parts.map((row) => pack(row, false))),
    "unsupported",
  );
  await add(
    "zstd_partial_magic",
    "zstd",
    Buffer.concat([compressed, Buffer.from([0x28, 0xb5])]),
    "incomplete",
  );
  await add(
    "zstd_dictionary",
    "zstd",
    Buffer.from([0x28, 0xb5, 0x2f, 0xfd, 0x25, 1, 0]),
    "unsupported",
  );
  await add(
    "zstd_window_budget",
    "zstd",
    Buffer.from([0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x80]),
    "budget_exceeded",
  );
  await add(
    "zstd_reserved_header_bit",
    "zstd",
    Buffer.from([0x28, 0xb5, 0x2f, 0xfd, 0x2c, 0]),
    "unverified",
  );
  await add(
    "zstd_skippable_frame",
    "zstd",
    Buffer.from([0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0]),
    "unsupported",
  );
  await add(
    "none_input_budget",
    "none",
    Buffer.alloc(limits.input_bytes + 1, 0x20),
    "budget_exceeded",
  );
  const large = Buffer.alloc(limits.decoded_bytes, 0x20);
  for (const contentSize of [true, false]) {
    await add(
      `zstd_decoded_budget_${contentSize ? "declared" : "streaming"}`,
      "zstd",
      Buffer.concat([header, pack(large, true, contentSize)]),
      "budget_exceeded",
    );
  }
  for (let index = 1; index <= 4; index++) {
    const corrupt = Buffer.from(compressed);
    corrupt[corrupt.length - index] ^= 1;
    await add(`zstd_checksum_byte_${index}`, "zstd", corrupt, "unverified");
  }
  const lastFrame = eventFrames.at(-1);
  const prefix = Buffer.concat([header, ...eventFrames.slice(0, -1)]);
  for (let cut = 1; cut < lastFrame.length; cut++) {
    await add(
      `zstd_final_frame_cut_${cut}`,
      "zstd",
      Buffer.concat([prefix, lastFrame.subarray(0, cut)]),
      ["incomplete", "unverified"],
    );
  }
  stage = "independent_audit";
  const audit = spawnSync(
    "uv",
    ["run", join(repo, "scripts/check-dsh-snapshot.py"), "--batch"],
    {
      cwd: repo,
      input: JSON.stringify(
        cases.map(({ path, compression }) => ({ path, compression })),
      ),
      encoding: "utf8",
      timeout: limits.audit_timeout_ms,
      maxBuffer: limits.audit_output_bytes,
    },
  );
  assert.ifError(audit.error);
  assert.equal(audit.status, 0);
  const audited = JSON.parse(audit.stdout);
  assert.equal(audited.zstandard_version, "0.25.0");
  assert.equal(audited.results.length, cases.length);
  const reports = {};
  const invoke = (path, compression, positions = true) => {
    if (path !== run) inside(run, path);
    const sourcePath = inWsl
      ? posix.join(wslRun, relative(run, path).split(sep).join("/"))
      : path;
    const child = spawnSync(
      inWsl ? "wsl.exe" : reader,
      [
        ...(inWsl ? ["--exec", reader] : []),
        "--path",
        sourcePath,
        "--compression",
        compression,
        ...(positions ? ["--positions"] : []),
      ],
      {
        cwd: repo,
        encoding: "utf8",
        timeout: limits.audit_timeout_ms,
        maxBuffer: limits.audit_output_bytes,
      },
    );
    assert.ifError(child.error);
    return { code: child.status, report: JSON.parse(child.stdout) };
  };
  const nativeAdmission = (plain) => {
    try {
      const rows = plain
        .toString("utf8")
        .trimEnd()
        .split("\n")
        .map((row) => JSON.parse(row));
      const decoder = codec.createDecoder(rows.shift(), "strict");
      const events = [];
      const context = {
        emitEvent: (event) => events.push(event),
        emitRun: () => assert.fail("unexpected packed run"),
      };
      for (const row of rows) decoder.decodeRow(row, context);
      const restored = {
        header: decoder.header,
        events,
        inheritedEventCount: decoder.finish(context),
      };
      format.restoreReleasedV4Artifact(
        restored,
        session.KNOWN_SESSION_EVENT_TYPES,
      );
      session.Session.create(
        restored.header.id,
        events,
        restored.header,
        restored.inheritedEventCount,
      );
      return true;
    } catch {
      return false;
    }
  };
  for (const [index, item] of cases.entries()) {
    stage = item.name;
    const { report, code } = invoke(item.path, item.compression);
    assert(
      item.expected.includes(report.kind),
      `${stage}: ${JSON.stringify(report)}`,
    );
    const evidence = audited.results[index];
    if (report.kind === "physically_validated") {
      assert.equal(code, 0);
      assert.equal(evidence.status, "complete");
      assert.equal(report.snapshot_sha256, digest(item.bytes));
      assert.equal(report.snapshot_sha256, evidence.snapshot_sha256);
      assert.equal(report.physical_bytes, item.bytes.length);
      assert.equal(report.decoded_bytes, evidence.decoded_bytes);
      assert.equal(report.records, evidence.records);
      assert.equal(report.frame_spans.length, evidence.frames);
      assert.equal(evidence.checksummed_frames, evidence.frames);
      const spans = report.frame_spans.map((frame) => [
        frame.physical_start,
        frame.physical_end,
      ]);
      assert.deepEqual(spans, evidence.frame_spans);
      const chunks = spans.map(([start, end]) =>
        zstdDecompressSync(item.bytes.subarray(start, end)),
      );
      const plain =
        item.compression === "none" ? item.bytes : Buffer.concat(chunks);
      assert.equal(report.jsonl_sha256, digest(plain));
      assert.equal(report.jsonl_sha256, evidence.verified_jsonl_sha256);
      assert.equal(report.production_advance_permit, false);
      assert.equal(report.semantic_admission_validated, false);
      assert.equal(report.source_observation_validated, false);
      assert.equal(report.normalized_positions_available, false);
      assert.equal(report.atomic_source_snapshot_proven, false);
      assert.equal(report.native_events, report.record_positions.length);
      let start = 0;
      const rows = plain.toString("utf8").split("\n");
      assert.equal(rows.pop(), "");
      assert.equal(report.header_sha256, digest(Buffer.from(`${rows[0]}\n`)));
      rows.forEach((row, recordIndex) => {
        const end = start + Buffer.byteLength(row) + 1;
        if (recordIndex > 0) {
          const position = report.record_positions[recordIndex - 1];
          assert.equal(position.native_seq, JSON.parse(row).seq);
          assert.equal(position.record_index, recordIndex);
          assert.deepEqual(
            [position.jsonl_start, position.jsonl_end],
            [start, end],
          );
          assert.equal(
            position.record_fingerprint,
            digest(row.trim()).slice(0, 16),
          );
          if (item.compression === "none") {
            assert.equal(position.frame_index, null);
          } else {
            const frame = report.frame_spans[position.frame_index];
            assert(frame.jsonl_start <= start && frame.jsonl_end >= end);
          }
        }
        start = end;
      });
      let logicalOffset = 0;
      chunks.forEach((chunk, frameIndex) => {
        const span = report.frame_spans[frameIndex];
        assert.deepEqual(
          [span.jsonl_start, span.jsonl_end],
          [logicalOffset, logicalOffset + chunk.length],
        );
        logicalOffset += chunk.length;
      });
      if (item.native !== undefined)
        assert.equal(nativeAdmission(plain), item.native);
    } else {
      assert.equal(code, 2);
      assert.equal(typeof report.reason, "string");
      assert.equal(report.snapshot_sha256, undefined);
      assert.equal(report.record_positions, undefined);
    }
    assert.equal(digest(await readFile(item.path)), digest(item.bytes));
    reports[item.name] = report;
    checks[item.name] = "passed";
  }
  stage = "repacking_preserves_record_positions";
  const baseline = reports.zstd_native_complete;
  const repacked = reports.zstd_repacked_events;
  assert.notEqual(baseline.snapshot_sha256, repacked.snapshot_sha256);
  assert.equal(baseline.jsonl_sha256, repacked.jsonl_sha256);
  assert.deepEqual(
    baseline.record_positions.map(({ frame_index, ...position }) => position),
    repacked.record_positions.map(({ frame_index, ...position }) => position),
  );
  assert.notDeepEqual(
    baseline.record_positions.map((position) => position.frame_index),
    repacked.record_positions.map((position) => position.frame_index),
  );
  checks[stage] = "passed";
  stage = "summary_omits_positions";
  const summary = invoke(cases[0].path, "none", false);
  assert.equal(summary.code, 0);
  assert.equal(summary.report.record_positions, undefined);
  checks[stage] = "passed";
  stage = "missing_path_does_not_create_source";
  const missing = join(run, "missing", "session.jsonl");
  const result = invoke(missing, "none");
  assert.equal(result.code, 2);
  assert.equal(result.report.kind, "unavailable");
  await assert.rejects(lstat(join(run, "missing")), { code: "ENOENT" });
  checks[stage] = "passed";
  stage = "directory_is_not_empty_session";
  const directory = invoke(run, "none");
  assert.equal(directory.code, 2);
  assert.equal(directory.report.kind, "unavailable");
  checks[stage] = "passed";
  console.log(
    JSON.stringify(
      {
        version: SUPPORTED_DSH_VERSION,
        node: process.version,
        rust_runtime: inWsl ? "wsl-linux" : process.platform,
        zstandard: audited.zstandard_version,
        run: relative(repo, run),
        checks,
        checked_snapshots: cases.length,
        final_frame_truncation_points: lastFrame.length - 1,
        checksum_mutations: 4,
        same_buffer_physical_validation: true,
        native_record_positions_compared: true,
        rust_semantic_admission_integrated: false,
        rust_source_observation_integrated: false,
        normalized_positions_available: false,
        production_advance_gate_integrated: false,
        provider_registered: false,
      },
      null,
      2,
    ),
  );
}

main().catch((error) => {
  console.error(
    JSON.stringify({
      ok: false,
      stage,
      error: error.name,
      detail: error.code === "ERR_ASSERTION" ? error.message : undefined,
    }),
  );
  process.exitCode = 1;
});

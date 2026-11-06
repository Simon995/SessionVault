#!/usr/bin/env node
/**
 * 在新建的 target 目录核对同一份合成快照的物理完整性与原生 admission。
 * node scripts/verify-dsh-snapshot.mjs --dsh-package <package.json>
 * 不读取真实日志，不启动 Host/LLM，不写总库；保留运行物。
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  writeFile,
} from "node:fs/promises";
import { dirname, isAbsolute, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { constants, zstdCompressSync } from "node:zlib";
import {
  loadDshInstallation,
  SUPPORTED_DSH_VERSION,
} from "./dsh-installed.mjs";

const AUDIT_TIMEOUT_MS = 45_000;
const counts = {};
let stage = "arguments";
let observation;
const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
const line = (value) => Buffer.from(`${JSON.stringify(value)}\n`);

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
    options: { "dsh-package": { type: "string" } },
  });
  assert(values["dsh-package"]);
  const { load } = loadDshInstallation(values["dsh-package"]);
  const { Context } = await load("@deepseek-ai/cordis");
  const backend = await load("@deepseek-ai/dsh-session-persistence-jsonl");
  const format = await load("@deepseek-ai/dsh-session-format-v3-to-v4");
  const session = await load("@deepseek-ai/dsh-session");
  const codec = format.releasedV4SessionFormatCodec;
  const artifact = JSON.parse(
    await readFile(
      new URL("../tests/fixtures/dsh-v4/complete-turn.json", import.meta.url),
      "utf8",
    ),
  );
  const repo = await realpath(fileURLToPath(new URL("..", import.meta.url)));
  const target = join(repo, "target");
  const info = await lstat(target);
  assert(info.isDirectory() && !info.isSymbolicLink());
  inside(repo, await realpath(target));
  const run = await mkdtemp(join(target, "dsh-snapshot-"));
  inside(target, await realpath(run));

  const rows = (value) =>
    [
      codec.encodeHeader(value.header, value.inheritedEventCount),
      ...value.events.map((event) => codec.encodeEvent(event)),
    ].map(line);
  const pack = (bytes, checksum = true) =>
    zstdCompressSync(bytes, {
      params: { [constants.ZSTD_c_checksumFlag]: Number(checksum) },
    });
  const encode = (parts, compression) =>
    Buffer.concat(
      parts.map((part) => (compression === "zstd" ? pack(part) : part)),
    );
  const cases = [];
  const withService = async (root, compression, action) => {
    const ctx = new Context();
    try {
      const fiber = ctx.plugin(backend, { root, compression });
      await fiber.await();
      const service = ctx.get("sessionPersistence");
      assert(service);
      return await action(service);
    } finally {
      await ctx.fiber.dispose();
    }
  };
  const add = async (
    name,
    compression,
    content,
    status,
    verified,
    options = {},
  ) => {
    stage = name;
    const root = join(run, name);
    await mkdir(root);
    const path = await withService(root, compression, async (service) => {
      const current = service.locate(artifact.header).path;
      inside(root, current);
      const path = options.filename
        ? join(dirname(current), options.filename)
        : current;
      inside(root, path);
      await mkdir(dirname(path), { recursive: true });
      await writeFile(path, content, { flag: "wx" });
      return path;
    });
    cases.push({ name, root, path, compression, status, verified, ...options });
  };

  for (const compression of ["none", "zstd"]) {
    const parts = rows(artifact);
    const complete = encode(parts, compression);
    await add(
      `${compression}_complete`,
      compression,
      complete,
      "complete",
      true,
      { nativeEvents: artifact.events.length },
    );
    await add(
      `${compression}_missing_final_byte`,
      compression,
      complete.subarray(0, -1),
      "incomplete",
      false,
      {
        nativeEvents: artifact.events.length - Number(compression === "none"),
      },
    );
    const partial =
      compression === "none"
        ? Buffer.from('{"type":')
        : pack(
            line(
              codec.encodeEvent({
                type: "turn/start",
                seq: artifact.events.length,
                time: 26,
                data: { turn: 2 },
              }),
            ),
          ).subarray(0, -1);
    await add(
      `${compression}_appended_partial`,
      compression,
      Buffer.concat([complete, partial]),
      "incomplete",
      false,
    );
    const bad = [...parts];
    bad[bad.length - 2] = Buffer.from("invalid JSON\n");
    await add(
      `${compression}_bad_json`,
      compression,
      encode(bad, compression),
      "invalid",
      false,
    );
    const utf8 = parts.map((part) => Buffer.from(part));
    // 在正文字符串内损坏一个 UTF-8 字节，保留 JSON 标点与记录边界。
    const textRow = utf8.find((part) =>
      part.includes("Follow the fixture instructions."),
    );
    assert(textRow);
    textRow[textRow.indexOf("Follow")] = 0xff;
    await add(
      `${compression}_invalid_utf8`,
      compression,
      encode(utf8, compression),
      "invalid",
      false,
      { nativeEvents: artifact.events.length },
    );
    const spaced = parts.map((part) =>
      Buffer.concat([
        Buffer.from(" "),
        part.subarray(0, -1),
        Buffer.from(" \n"),
      ]),
    );
    await add(
      `${compression}_whitespace_records`,
      compression,
      encode(spaced, compression),
      "complete",
      true,
      { nativeEvents: artifact.events.length },
    );
    const unknown = [
      ...parts,
      line({
        type: "fixture/unknown",
        seq: artifact.events.length,
        time: 25,
        data: {},
      }),
    ];
    await add(
      `${compression}_unknown_required_event`,
      compression,
      encode(unknown, compression),
      "complete",
      false,
    );
    const brokenRelation = structuredClone(artifact);
    const resultMessage = brokenRelation.events.find(
      (event) => event.type === "tool/result",
    ).data.message;
    resultMessage.toolCallId = "other";
    resultMessage.source.callId = "other";
    await add(
      `${compression}_broken_call_relation`,
      compression,
      encode(rows(brokenRelation), compression),
      "complete",
      false,
    );
    const changed = structuredClone(artifact);
    changed.events.find(
      (event) => event.type === "user/message",
    ).data.content[0].text = "Read the changed.";
    await add(
      `${compression}_same_count_rewrite`,
      compression,
      encode(rows(changed), compression),
      "complete",
      true,
      { nativeEvents: artifact.events.length },
    );
  }
  const parts = rows(artifact);
  const compressed = encode(parts, "zstd");
  const corrupt = Buffer.from(compressed);
  corrupt[corrupt.length - 1] ^= 1;
  await add("zstd_bad_checksum", "zstd", corrupt, "invalid", false);
  const noChecksum = Buffer.concat(parts.map((part) => pack(part, false)));
  await add("zstd_without_checksum", "zstd", noChecksum, "unsupported", false, {
    nativeEvents: artifact.events.length,
  });
  await add(
    "zstd_header_shares_frame",
    "zstd",
    Buffer.concat([
      pack(Buffer.concat(parts.slice(0, 2))),
      ...parts.slice(2).map((part) => pack(part)),
    ]),
    "invalid",
    false,
  );
  const unterminated = [...parts];
  unterminated[unterminated.length - 1] = parts.at(-1).subarray(0, -1);
  await add(
    "zstd_complete_frame_unterminated_line",
    "zstd",
    encode(unterminated, "zstd"),
    "invalid",
    false,
  );
  await add(
    "zstd_partial_next_magic",
    "zstd",
    Buffer.concat([compressed, Buffer.from([0x28, 0xb5])]),
    "incomplete",
    false,
    { nativeEvents: artifact.events.length },
  );
  await add(
    "zstd_repacked_events",
    "zstd",
    Buffer.concat([pack(parts[0]), pack(Buffer.concat(parts.slice(1)))]),
    "complete",
    true,
    { nativeEvents: artifact.events.length },
  );
  await add(
    "future_physical_generation",
    "none",
    encode(parts, "none"),
    "complete",
    false,
    { filename: "session.v5.jsonl" },
  );

  stage = "independent_byte_audit";
  const auditProcess = spawnSync(
    "uv",
    ["run", join(repo, "scripts/check-dsh-snapshot.py"), "--batch"],
    {
      cwd: repo,
      encoding: "utf8",
      input: JSON.stringify(
        cases.map(({ path, compression }) => ({ path, compression })),
      ),
      timeout: AUDIT_TIMEOUT_MS,
      maxBuffer: 1024 * 1024,
    },
  );
  assert.ifError(auditProcess.error);
  assert.equal(
    auditProcess.status,
    0,
    "independent auditor must complete successfully",
  );
  const audit = JSON.parse(auditProcess.stdout);
  assert.equal(audit.zstandard_version, "0.25.0");
  assert.equal(audit.results.length, cases.length);
  const observations = {};

  const boundRead = async (item, evidence) => {
    const before = await readFile(item.path);
    // 错配的摘要在 native read 前就阻断；不拼接另一个文件的完整性结论。
    if (digest(before) !== evidence.snapshot_sha256)
      return { verified: false, reason: "snapshot_mismatch" };
    let native;
    let nativeError;
    let admitted = false;
    try {
      native = await withService(
        item.root,
        item.compression,
        async (service) => {
          const handle = await service.open(artifact.header.id, "read");
          try {
            return {
              header: handle.header,
              inheritedEventCount: handle.inheritedEventCount,
              ...(await handle.read()),
            };
          } finally {
            await handle.close();
          }
        },
      );
      try {
        format.restoreReleasedV4Artifact(
          native,
          session.KNOWN_SESSION_EVENT_TYPES,
        );
        session.Session.create(
          native.header.id,
          native.events,
          native.header,
          native.inheritedEventCount,
        );
        admitted = true;
      } catch {
        // 物理完整、原生 persistence 可读仍不等于 restore 可接受。
      }
    } catch (error) {
      nativeError = error.name;
    }
    assert.equal(
      digest(await readFile(item.path)),
      evidence.snapshot_sha256,
      "read-only native path must preserve the frozen fixture",
    );
    // v4 每条物理 JSONL 记录对应一个 header 或事件。不能用合法短前缀的
    // restore 成功，代替整份快照的 admission；也不要求规范化重编码逐字相同。
    const recordsMatch =
      native !== undefined && evidence.records === native.events.length + 1;
    const verified = evidence.status === "complete" && admitted && recordsMatch;
    return {
      verified,
      reason: verified ? "verified" : "blocked",
      native,
      nativeError,
      admitted,
      recordsMatch,
    };
  };

  for (const [index, item] of cases.entries()) {
    stage = item.name;
    const evidence = audit.results[index];
    assert.equal(evidence.status, item.status);
    const observed = await boundRead(item, evidence);
    observation = {
      name: item.name,
      physical_status: evidence.status,
      native_event_count: observed.native?.events.length,
      native_error: observed.nativeError,
      admitted: observed.admitted,
      records_match: observed.recordsMatch,
      verified: observed.verified,
    };
    assert.equal(observed.verified, item.verified);
    if (item.nativeEvents !== undefined)
      assert.equal(observed.native?.events.length, item.nativeEvents);
    if (observed.verified) {
      assert.equal(evidence.verified_physical_bytes, evidence.physical_bytes);
      assert.equal(evidence.records, observed.native.events.length + 1);
    }
    observations[item.name] = { ...observed, evidence };
    counts[item.name] = "passed";
  }
  for (const compression of ["none", "zstd"]) {
    stage = `${compression}_same_count_content_evidence`;
    const old = observations[`${compression}_complete`];
    const changed = observations[`${compression}_same_count_rewrite`];
    assert.equal(old.native.events.length, changed.native.events.length);
    assert.notEqual(
      digest(JSON.stringify(old.native.events)),
      digest(JSON.stringify(changed.native.events)),
    );
    assert.notEqual(
      old.evidence.snapshot_sha256,
      changed.evidence.snapshot_sha256,
    );
    counts[stage] = "passed";
  }
  stage = "repacking_preserves_logical_evidence";
  const baseline = observations.zstd_complete.evidence;
  const repacked = observations.zstd_repacked_events.evidence;
  assert.notEqual(baseline.snapshot_sha256, repacked.snapshot_sha256);
  assert.equal(baseline.verified_jsonl_sha256, repacked.verified_jsonl_sha256);
  assert.deepEqual(
    observations.zstd_complete.native.events,
    observations.zstd_repacked_events.native.events,
  );
  counts[stage] = "passed";
  stage = "evidence_from_another_snapshot_is_refused";
  const changed = cases.find((item) => item.name === "zstd_same_count_rewrite");
  assert.deepEqual(await boundRead(changed, baseline), {
    verified: false,
    reason: "snapshot_mismatch",
  });
  counts[stage] = "passed";

  console.log(
    JSON.stringify(
      {
        version: SUPPORTED_DSH_VERSION,
        node: globalThis.process.version,
        zstandard: audit.zstandard_version,
        run: relative(repo, run),
        checks: counts,
        observations: Object.fromEntries(
          Object.entries(observations).map(([name, value]) => [
            name,
            {
              physical_status: value.evidence.status,
              reason: value.evidence.reason,
              native_read_succeeded: value.native !== undefined,
              native_event_count: value.native?.events.length,
              native_error: value.nativeError,
              semantic_admission_succeeded: value.admitted,
              same_snapshot_verified: value.verified,
              verified_physical_bytes: value.evidence.verified_physical_bytes,
              physical_bytes: value.evidence.physical_bytes,
            },
          ]),
        ),
        frozen_fixture_snapshots_validated: true,
        live_source_capture_validated: false,
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
      code: error.code,
      observation,
    }),
  );
  globalThis.process.exitCode = 1;
});

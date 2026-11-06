#!/usr/bin/env node
/** 验证 Rust 会话目录选代、同句柄捕获、并发复查、旧前缀核对与保守提交门。
 * 同时支持本机 `--rust-reader` 与 WSL Linux `--wsl-reader`。
 * 只操作项目 target 新建目录，不读真日志、不启动 Host/LLM、不写总库；保留运行物。
 */
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs/promises";
import {
  dirname,
  isAbsolute,
  join,
  posix,
  relative,
  resolve,
  sep,
} from "node:path";
import { createInterface } from "node:readline";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { constants, zstdCompressSync } from "node:zlib";
import {
  loadDshInstallation,
  SUPPORTED_DSH_VERSION,
} from "./dsh-installed.mjs";
import limits from "./dsh-snapshot-limits.json" with { type: "json" };
import {
  bounded,
  inside,
  ioWindow,
} from "../tests/fixtures/dsh-v4/io-harness.mjs";
import { seedWithTurn } from "../tests/fixtures/dsh-v4/scenarios.mjs";

const checks = {};
const observations = {};
let stage = "arguments";
const hash = (value) => createHash("sha256").update(value).digest("hex");
const line = (value) => Buffer.from(`${JSON.stringify(value)}\n`);
const pack = (bytes) =>
  zstdCompressSync(bytes, { params: { [constants.ZSTD_c_checksumFlag]: 1 } });
const suffix = (compression) => (compression === "zstd" ? ".zstd" : "");

async function main() {
  const { values } = parseArgs({
    options: {
      "dsh-package": { type: "string" },
      "rust-reader": { type: "string" },
      "wsl-reader": { type: "string" },
      "native-node": { type: "string" },
      "native-dsh-package": { type: "string" },
    },
  });
  assert(values["dsh-package"] && values["native-node"]);
  assert.equal(
    Number(Boolean(values["rust-reader"])) +
      Number(Boolean(values["wsl-reader"])),
    1,
  );
  const inWsl = Boolean(values["wsl-reader"]);
  const reader = inWsl ? values["wsl-reader"] : resolve(values["rust-reader"]);
  const nativePackage = inWsl
    ? values["native-dsh-package"]
    : resolve(values["native-dsh-package"] ?? values["dsh-package"]);
  assert((inWsl ? posix.isAbsolute : isAbsolute)(reader));
  assert((inWsl ? posix.isAbsolute : isAbsolute)(values["native-node"]));
  assert(
    nativePackage && (inWsl ? posix.isAbsolute : isAbsolute)(nativePackage),
  );
  const wsl = (args, allowed = [0]) => {
    const child = spawnSync("wsl.exe", ["--exec", ...args], {
      encoding: "utf8",
      timeout: limits.audit_timeout_ms,
      maxBuffer: limits.audit_output_bytes,
    });
    assert.ifError(child.error);
    assert(allowed.includes(child.status));
    return child;
  };
  if (inWsl) {
    assert.equal(process.platform, "win32");
    wsl(["test", "-x", reader]);
    wsl(["test", "-x", values["native-node"]]);
    wsl(["test", "-f", nativePackage]);
  } else {
    assert((await fs.lstat(reader)).isFile());
    assert((await fs.lstat(values["native-node"])).isFile());
    assert((await fs.lstat(nativePackage)).isFile());
  }

  const repo = await fs.realpath(fileURLToPath(new URL("..", import.meta.url)));
  const target = join(repo, "target");
  const info = await fs.lstat(target);
  assert(info.isDirectory() && !info.isSymbolicLink());
  inside(repo, await fs.realpath(target));
  const run = await fs.mkdtemp(join(target, "dsh-rust-source-"));
  inside(target, await fs.realpath(run));
  const wslRun = inWsl ? wsl(["wslpath", "-u", run]).stdout.trim() : undefined;
  if (inWsl) assert(posix.isAbsolute(wslRun));
  const toRuntimePath = (path) => {
    inside(run, path);
    return inWsl
      ? posix.join(wslRun, relative(run, path).split(sep).join("/"))
      : path;
  };

  const installation = loadDshInstallation(values["dsh-package"]);
  const { Context } = await installation.load("@deepseek-ai/cordis");
  const backend = await installation.load(
    "@deepseek-ai/dsh-session-persistence-jsonl",
  );
  const format = await installation.load(
    "@deepseek-ai/dsh-session-format-v3-to-v4",
  );
  const codec = format.releasedV4SessionFormatCodec;
  const formatEntryUrl = pathToFileURL(
    installation.require.resolve("@deepseek-ai/dsh-session-format-v3-to-v4"),
  ).href;
  const sessionEntryUrl = pathToFileURL(
    installation.require.resolve("@deepseek-ai/dsh-session"),
  ).href;
  let wslFormatUrl;
  let wslSessionUrl;
  if (inWsl) {
    const resolved = JSON.parse(
      wsl([
        values["native-node"],
        "--input-type=module",
        "-e",
        [
          'import { createRequire } from "node:module";',
          'import { pathToFileURL } from "node:url";',
          "const req = createRequire(process.argv[1]);",
          "process.stdout.write(JSON.stringify({",
          '  format: pathToFileURL(req.resolve("@deepseek-ai/dsh-session-format-v3-to-v4")).href,',
          '  session: pathToFileURL(req.resolve("@deepseek-ai/dsh-session")).href,',
          "}));",
        ].join("\n"),
        nativePackage,
      ]).stdout,
    );
    wslFormatUrl = resolved.format;
    wslSessionUrl = resolved.session;
  }

  const artifact = JSON.parse(
    await fs.readFile(
      new URL("../tests/fixtures/dsh-v4/complete-turn.json", import.meta.url),
      "utf8",
    ),
  );
  const child = seedWithTurn(artifact, "child");
  const reseeded = seedWithTurn(child, "reseeded");
  reseeded.header = structuredClone(child.header);
  const own = child.events
    .slice(artifact.events.length + 1)
    .map((event, index) => ({ ...event, seq: artifact.events.length + index }));
  const grown = {
    ...structuredClone(artifact),
    events: [...artifact.events, ...own],
  };
  const rows = (value) =>
    [
      codec.encodeHeader(value.header, value.inheritedEventCount),
      ...value.events.map((event) => codec.encodeEvent(event)),
    ].map(line);
  const encode = (value, compression) =>
    Buffer.concat(
      rows(value).map((row) => (compression === "zstd" ? pack(row) : row)),
    );

  const runOnce = (
    sessionDir,
    compression,
    {
      sourceInstance = "fixture-source",
      sessionId = artifact.header.id,
      dshPackage = nativePackage,
      compareDir,
      compareInstance,
    } = {},
  ) => {
    const args = [
      ...(inWsl ? ["--exec", reader] : []),
      "--session-dir",
      toRuntimePath(sessionDir),
      "--source-instance",
      sourceInstance,
      "--compression",
      compression,
      "--positions",
      "--node",
      values["native-node"],
      "--dsh-package",
      dshPackage,
      ...(sessionId ? ["--session-id", sessionId] : []),
      ...(compareDir
        ? ["--compare-session-dir", toRuntimePath(compareDir)]
        : []),
      ...(compareInstance
        ? ["--compare-source-instance", compareInstance]
        : []),
    ];
    const proc = spawnSync(inWsl ? "wsl.exe" : reader, args, {
      cwd: repo,
      encoding: "utf8",
      timeout: limits.audit_timeout_ms,
      maxBuffer: limits.audit_output_bytes,
    });
    assert.ifError(proc.error);
    return {
      code: proc.status,
      report: JSON.parse(proc.stdout.trim()),
    };
  };

  const runTwoStep = async (
    sessionDir,
    compression,
    betweenSteps,
    { sourceInstance = "fixture-source", sessionId = artifact.header.id } = {},
  ) => {
    const args = [
      ...(inWsl ? ["--exec", reader] : []),
      "--session-dir",
      toRuntimePath(sessionDir),
      "--source-instance",
      sourceInstance,
      "--session-id",
      sessionId,
      "--compression",
      compression,
      "--positions",
      "--node",
      values["native-node"],
      "--dsh-package",
      nativePackage,
      "--follow-stdin",
    ];
    const proc = spawn(inWsl ? "wsl.exe" : reader, args, {
      cwd: repo,
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    proc.stderr.resume();
    const rl = createInterface({ input: proc.stdout });
    const iterator = rl[Symbol.asyncIterator]();
    const exited = new Promise((accept, reject) => {
      proc.once("error", reject);
      proc.once("close", (code) => accept(code));
    });
    try {
      const firstLine = await bounded(iterator.next());
      assert(!firstLine.done, `${stage}: missing first line`);
      const first = JSON.parse(firstLine.value);
      await bounded(Promise.resolve().then(() => betweenSteps(first)));
      proc.stdin.end("\n");
      const secondLine = await bounded(iterator.next());
      assert(!secondLine.done, `${stage}: missing second line`);
      const second = JSON.parse(secondLine.value);
      const code = await bounded(exited);
      assert.equal(code, 0, `${stage}: exit ${code}`);
      return { first, second };
    } finally {
      rl.close();
      if (proc.exitCode === null) proc.kill();
    }
  };

  // 构造包装 DSH 包：在 Rust 完成文件读取、进入 Node admission 时对目标文件执行动作，随后转发至真实 DSH 包。
  const makeHookedPackage = async (name, hookBody) => {
    const root = join(run, name);
    await fs.mkdir(root);
    const anchor = join(root, "package.json");
    await fs.writeFile(
      anchor,
      JSON.stringify({
        name: "@deepseek-ai/dsh",
        version: SUPPORTED_DSH_VERSION,
      }),
      { flag: "wx" },
    );
    const fmtTarget = inWsl ? wslFormatUrl : formatEntryUrl;
    const sesTarget = inWsl ? wslSessionUrl : sessionEntryUrl;
    for (const [mod, body] of [
      [
        "dsh-session-format-v3-to-v4",
        `${hookBody}\nexport * from ${JSON.stringify(fmtTarget)};\n`,
      ],
      ["dsh-session", `export * from ${JSON.stringify(sesTarget)};\n`],
    ]) {
      const dir = join(root, "node_modules", "@deepseek-ai", mod);
      await fs.mkdir(dir, { recursive: true });
      await fs.writeFile(
        join(dir, "package.json"),
        JSON.stringify({
          name: `@deepseek-ai/${mod}`,
          type: "module",
          exports: "./index.mjs",
        }),
        { flag: "wx" },
      );
      await fs.writeFile(join(dir, "index.mjs"), body, { flag: "wx" });
    }
    return toRuntimePath(anchor);
  };

  const withFixtureSession = async (name, compression, initial, fn) => {
    stage = `${compression}_${name}`;
    const root = join(run, stage);
    await fs.mkdir(root);
    const ctx = new Context();
    try {
      await ctx.plugin(backend, { root, compression }).await();
      const service = ctx.get("sessionPersistence");
      const content = encode(initial, compression);
      const path = service.locate(initial.header).path;
      const sessionDir = dirname(path);
      inside(root, path);
      await fs.mkdir(sessionDir, { recursive: true });
      await fs.writeFile(path, content, { flag: "wx" });
      await fn({ root, sessionDir, path, content, service });
      checks[stage] = "passed";
    } finally {
      await ctx.fiber.dispose();
    }
  };

  for (const compression of ["none", "zstd"]) {
    await withFixtureSession(
      "stable_source_capture_and_recheck",
      compression,
      artifact,
      async ({ sessionDir, content }) => {
        // 放入非规范名与旧代 v0，确认按最高规范代选择 v4。
        await fs.writeFile(
          join(sessionDir, `session.v0.jsonl${suffix(compression)}`),
          Buffer.from("ignored"),
          { flag: "wx" },
        );
        await fs.writeFile(
          join(sessionDir, `session.v4.jsonl${suffix(compression)}.tmp`),
          Buffer.from("ignored"),
          { flag: "wx" },
        );
        await fs.writeFile(
          join(sessionDir, `session.jsonl${suffix(compression)}`),
          Buffer.from("old-v0"),
          { flag: "wx" },
        );
        const { first, second } = await runTwoStep(
          sessionDir,
          compression,
          async (report) => {
            assert.equal(report.kind, "source_observed_snapshot");
            assert.equal(report.semantic_admission_validated, true);
            assert.equal(report.source_observation_validated, true);
            assert.equal(report.normalized_positions_available, false);
            assert.equal(report.atomic_source_snapshot_proven, false);
            assert.equal(report.production_advance_permit, false);
            assert.equal(report.snapshot_sha256, hash(content));
            assert.equal(
              report.source_observation.selected_file,
              `session.v4.jsonl${suffix(compression)}`,
            );
            assert.equal(report.source_observation.physical_generation, 4);
            assert.deepEqual(report.commit_gate, {
              decision: "initial_projection_candidate",
              verified_events: artifact.events.length,
              inherited_event_count: 0,
            });
          },
        );
        assert.equal(second.kind, "source_step_evaluated");
        assert.deepEqual(second.prior_recheck, { kind: "matches_observation" });
        assert.equal(second.relation, "prefix_preserved");
        assert.deepEqual(second.commit_gate, {
          decision: "no_op_unchanged",
          verified_events: artifact.events.length,
        });
        assert.equal(second.production_advance_permit, false);
        observations[stage] = {
          selected_file: first.source_observation.selected_file,
          initial_decision: first.commit_gate.decision,
          unchanged_decision: second.commit_gate.decision,
        };
      },
    );

    await withFixtureSession(
      "native_append_and_old_prefix",
      compression,
      artifact,
      async ({ sessionDir, service }) => {
        const { second } = await runTwoStep(
          sessionDir,
          compression,
          async () => {
            const writer = await service.open(artifact.header.id, "write");
            try {
              await writer.append(own);
              await writer.flush();
            } finally {
              await writer.close();
            }
          },
        );
        assert.equal(second.kind, "source_step_evaluated");
        assert.equal(second.prior_recheck.kind, "changed");
        assert.equal(second.relation, "prefix_preserved");
        assert.deepEqual(second.relation_detail, {
          relation: "prefix_preserved",
          prior_events: artifact.events.length,
          current_events: grown.events.length,
          appended_events: own.length,
        });
        assert.deepEqual(second.commit_gate, {
          decision: "incremental_append_candidate",
          prior_events: artifact.events.length,
          verified_events: grown.events.length,
          appended_events: own.length,
        });
      },
    );

    await withFixtureSession(
      "same_count_rewrite",
      compression,
      artifact,
      async ({ sessionDir, path }) => {
        const changed = structuredClone(artifact);
        const targetIndex = changed.events.findIndex(
          (event) => event.type === "user/message",
        );
        changed.events[targetIndex].data.content[0].text = "Read the changed.";
        const { second } = await runTwoStep(
          sessionDir,
          compression,
          async () => {
            await delay(20);
            await fs.writeFile(path, encode(changed, compression));
          },
        );
        assert.equal(second.kind, "source_step_evaluated");
        assert.equal(second.prior_recheck.kind, "changed");
        assert.equal(second.relation, "prefix_rewritten");
        assert.deepEqual(second.commit_gate, {
          decision: "rebuild_projection_candidate",
          prior_events: artifact.events.length,
          verified_events: artifact.events.length,
          invalidates_prior_credential: true,
          reason: {
            rebuild_kind: "prefix_rewritten",
            first_diff_seq: targetIndex,
          },
        });
      },
    );

    await withFixtureSession(
      "header_lifecycle_change",
      compression,
      artifact,
      async ({ sessionDir, path }) => {
        const changed = structuredClone(artifact);
        changed.header.createdAt += 1;
        const { second } = await runTwoStep(
          sessionDir,
          compression,
          async () => {
            await delay(20);
            await fs.writeFile(path, encode(changed, compression));
          },
        );
        assert.equal(second.kind, "source_step_evaluated");
        assert.equal(second.relation, "lifecycle_changed");
        assert.deepEqual(second.commit_gate, {
          decision: "rebuild_projection_candidate",
          prior_events: artifact.events.length,
          verified_events: artifact.events.length,
          invalidates_prior_credential: true,
          reason: { rebuild_kind: "lifecycle_changed" },
        });
      },
    );

    await withFixtureSession(
      "seeded_inherited_cut_lifecycle_change",
      compression,
      child,
      async ({ sessionDir, path }) => {
        const { first, second } = await runTwoStep(
          sessionDir,
          compression,
          async () => {
            await delay(20);
            await fs.writeFile(path, encode(reseeded, compression));
          },
          { sessionId: child.header.id },
        );
        assert.equal(first.header_sha256, second.current.header_sha256);
        assert.equal(second.relation, "lifecycle_changed");
        assert.deepEqual(second.commit_gate, {
          decision: "rebuild_projection_candidate",
          prior_events: child.events.length,
          verified_events: reseeded.events.length,
          invalidates_prior_credential: true,
          reason: { rebuild_kind: "lifecycle_changed" },
        });
      },
    );

    await withFixtureSession(
      "native_repair_intermediate_and_recovered",
      compression,
      artifact,
      async ({ sessionDir, path, content, service }) => {
        let intermediateStep;
        const { second: afterRecovery } = await runTwoStep(
          sessionDir,
          compression,
          async () => {
            const bad =
              compression === "zstd"
                ? content.subarray(0, -1)
                : Buffer.concat([content, Buffer.from('{"type":')]);
            await delay(20);
            await fs.writeFile(path, bad);
            const writer = await service.open(artifact.header.id, "write");
            try {
              await ioWindow(
                "truncate",
                path,
                async (hit) => {
                  assert.equal(hit, 1);
                  // 在修尾刚完成 truncate、恢复记录尚未写回时，对比初始 10 事件与当前中间态。
                  const initialCopy = join(run, `${stage}_prior_copy`);
                  await fs.mkdir(initialCopy);
                  await fs.writeFile(
                    join(initialCopy, `session.v4.jsonl${suffix(compression)}`),
                    content,
                    { flag: "wx" },
                  );
                  // 用同一个 sessionDir 跑两步：先在 initialCopy 放原内容，再切到修尾中间态。
                  const middleBytes = await fs.readFile(path);
                  await fs.writeFile(path, content);
                  const step = await runTwoStep(
                    sessionDir,
                    compression,
                    async () => {
                      await delay(20);
                      await fs.writeFile(path, middleBytes);
                    },
                  );
                  intermediateStep = step.second;
                },
                async () => {
                  await writer.append(own);
                  await writer.flush();
                },
              );
            } finally {
              await writer.close();
            }
          },
        );
        assert(intermediateStep);
        if (compression === "zstd") {
          assert.equal(intermediateStep.relation, "prefix_shrank");
          assert.deepEqual(intermediateStep.commit_gate, {
            decision: "hold_prior_projection",
            prior_events: artifact.events.length,
            reason: {
              hold_kind: "prefix_shrank",
              current_events: artifact.events.length - 1,
              shared_prefix_intact: true,
            },
          });
        } else {
          assert.equal(intermediateStep.relation, "prefix_preserved");
          assert.deepEqual(intermediateStep.commit_gate, {
            decision: "no_op_unchanged",
            verified_events: artifact.events.length,
          });
        }
        assert.equal(afterRecovery.relation, "prefix_preserved");
        assert.deepEqual(afterRecovery.commit_gate, {
          decision: "incremental_append_candidate",
          prior_events: artifact.events.length,
          verified_events: grown.events.length,
          appended_events: own.length,
        });
        observations[stage] = {
          intermediate_relation: intermediateStep.relation,
          intermediate_gate: intermediateStep.commit_gate.decision,
          recovered_gate: afterRecovery.commit_gate.decision,
        };
      },
    );

    await withFixtureSession(
      "incomplete_tail_holds_prior",
      compression,
      artifact,
      async ({ sessionDir, path, content }) => {
        const { second } = await runTwoStep(
          sessionDir,
          compression,
          async () => {
            await delay(20);
            await fs.writeFile(path, content.subarray(0, -1));
          },
        );
        assert.equal(second.kind, "source_step_blocked");
        assert.equal(second.prior_recheck.kind, "changed");
        assert.equal(second.commit_gate.decision, "hold_prior_projection");
        assert.equal(second.commit_gate.prior_events, artifact.events.length);
        assert.equal(second.commit_gate.reason.hold_kind, "capture_blocked");
      },
    );

    await withFixtureSession(
      "semantic_rejection_holds_prior",
      compression,
      artifact,
      async ({ sessionDir, path, content }) => {
        const unknown = line({
          type: "fixture/unknown",
          seq: artifact.events.length,
          time: 25,
          data: {},
        });
        const { second } = await runTwoStep(
          sessionDir,
          compression,
          async () => {
            await delay(20);
            await fs.writeFile(
              path,
              Buffer.concat([
                content,
                compression === "zstd" ? pack(unknown) : unknown,
              ]),
            );
          },
        );
        assert.equal(second.kind, "source_step_blocked");
        assert.deepEqual(second.blocked, {
          kind: "rejected",
          reason: "native_semantic_admission",
        });
        assert.deepEqual(second.commit_gate, {
          decision: "hold_prior_projection",
          prior_events: artifact.events.length,
          reason: {
            hold_kind: "capture_blocked",
            blocked: {
              kind: "rejected",
              reason: "native_semantic_admission",
            },
          },
        });
      },
    );

    await withFixtureSession(
      "future_v5_recheck_and_hold_prior",
      compression,
      artifact,
      async ({ sessionDir }) => {
        const future = line({
          ...codec.encodeHeader(artifact.header, 0),
          version: 5,
        });
        const { second } = await runTwoStep(
          sessionDir,
          compression,
          async () => {
            await fs.writeFile(
              join(sessionDir, `session.v5.jsonl${suffix(compression)}`),
              compression === "zstd" ? pack(future) : future,
              { flag: "wx" },
            );
          },
        );
        assert.equal(second.kind, "source_step_blocked");
        assert.deepEqual(second.prior_recheck, {
          kind: "unsupported",
          reason: "physical_generation",
        });
        assert.deepEqual(second.commit_gate, {
          decision: "hold_prior_projection",
          prior_events: artifact.events.length,
          reason: {
            hold_kind: "capture_blocked",
            blocked: {
              kind: "unsupported",
              reason: "physical_generation",
            },
          },
        });
      },
    );

    await withFixtureSession(
      "mutation_during_capture_append",
      compression,
      artifact,
      async ({ sessionDir, path }) => {
        const extra = Buffer.concat(
          own.map((event) =>
            compression === "zstd"
              ? pack(line(codec.encodeEvent(event)))
              : line(codec.encodeEvent(event)),
          ),
        );
        const runtimeFile = toRuntimePath(path);
        const hooked = await makeHookedPackage(
          `hook_append_${compression}`,
          `import { appendFileSync } from "node:fs";\nappendFileSync(${JSON.stringify(runtimeFile)}, Buffer.from(${JSON.stringify(extra.toString("base64"))}, "base64"));`,
        );
        const { code, report } = runOnce(sessionDir, compression, {
          dshPackage: hooked,
        });
        assert.equal(code, 2);
        assert.equal(report.kind, "changed");
        assert.equal(report.reason, "file_changed");
        assert.equal(report.commit_gate.decision, "reject_initial");
      },
    );

    await withFixtureSession(
      "mutation_during_capture_overwrite",
      compression,
      artifact,
      async ({ sessionDir, path, content }) => {
        const changed = structuredClone(artifact);
        changed.events.find(
          (event) => event.type === "user/message",
        ).data.content[0].text = "Read the changed.";
        const replacement = encode(changed, compression);
        assert.equal(replacement.length, content.length);
        const runtimeFile = toRuntimePath(path);
        const hooked = await makeHookedPackage(
          `hook_overwrite_${compression}`,
          `import { writeFileSync } from "node:fs";\nAtomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 25);\nwriteFileSync(${JSON.stringify(runtimeFile)}, Buffer.from(${JSON.stringify(replacement.toString("base64"))}, "base64"), { flag: "r+" });`,
        );
        const { code, report } = runOnce(sessionDir, compression, {
          dshPackage: hooked,
        });
        assert.equal(code, 2);
        assert.equal(report.kind, "changed");
        assert.equal(report.reason, "file_changed");
      },
    );

    await withFixtureSession(
      "mutation_during_capture_aba_replace",
      compression,
      artifact,
      async ({ sessionDir, path, content }) => {
        const runtimeFile = toRuntimePath(path);
        const staged1 = `${runtimeFile}.aba-1`;
        const staged2 = `${runtimeFile}.aba-2`;
        const other = encode(grown, compression);
        const hooked = await makeHookedPackage(
          `hook_aba_${compression}`,
          [
            'import { renameSync, writeFileSync } from "node:fs";',
            "Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 25);",
            `writeFileSync(${JSON.stringify(staged1)}, Buffer.from(${JSON.stringify(other.toString("base64"))}, "base64"), { flag: "wx" });`,
            `renameSync(${JSON.stringify(staged1)}, ${JSON.stringify(runtimeFile)});`,
            `writeFileSync(${JSON.stringify(staged2)}, Buffer.from(${JSON.stringify(content.toString("base64"))}, "base64"), { flag: "wx" });`,
            `renameSync(${JSON.stringify(staged2)}, ${JSON.stringify(runtimeFile)});`,
          ].join("\n"),
        );
        const { code, report } = runOnce(sessionDir, compression, {
          dshPackage: hooked,
        });
        assert.equal(code, 2);
        assert.equal(report.kind, "changed");
        assert.equal(report.reason, "file_changed");
        assert.equal(hash(await fs.readFile(path)), hash(content));
      },
    );

    await withFixtureSession(
      "mutation_during_capture_publish_v5",
      compression,
      artifact,
      async ({ sessionDir }) => {
        const future = line({
          ...codec.encodeHeader(artifact.header, 0),
          version: 5,
        });
        const v5Bytes = compression === "zstd" ? pack(future) : future;
        const v5Path = toRuntimePath(
          join(sessionDir, `session.v5.jsonl${suffix(compression)}`),
        );
        const hooked = await makeHookedPackage(
          `hook_v5_${compression}`,
          `import { writeFileSync } from "node:fs";\nwriteFileSync(${JSON.stringify(v5Path)}, Buffer.from(${JSON.stringify(v5Bytes.toString("base64"))}, "base64"), { flag: "wx" });`,
        );
        const { code, report } = runOnce(sessionDir, compression, {
          dshPackage: hooked,
        });
        assert.equal(code, 2);
        assert.equal(report.kind, "unsupported");
        assert.equal(report.reason, "physical_generation");
      },
    );

    await withFixtureSession(
      "encoding_conflict_rejected",
      compression,
      artifact,
      async ({ sessionDir }) => {
        const otherSuffix = compression === "zstd" ? "" : ".zstd";
        await fs.writeFile(
          join(sessionDir, `session.v4.jsonl${otherSuffix}`),
          Buffer.from("conflict"),
          { flag: "wx" },
        );
        const { code, report } = runOnce(sessionDir, compression);
        assert.equal(code, 2);
        assert.equal(report.kind, "unsupported");
        assert.equal(report.reason, "encoding_conflict");
      },
    );

    await withFixtureSession(
      "header_session_id_mismatch_rejected",
      compression,
      artifact,
      async ({ sessionDir }) => {
        const { code, report } = runOnce(sessionDir, compression, {
          sessionId: `${artifact.header.id}-mismatch`,
        });
        assert.equal(code, 2);
        assert.equal(report.kind, "changed");
        assert.equal(report.reason, "header_binding_changed");
      },
    );

    await withFixtureSession(
      "different_source_rejected_by_gate",
      compression,
      artifact,
      async ({ sessionDir, content }) => {
        const otherDir = join(run, `${stage}_other`);
        await fs.mkdir(otherDir);
        await fs.writeFile(
          join(otherDir, `session.v4.jsonl${suffix(compression)}`),
          content,
          { flag: "wx" },
        );
        const { code, report } = runOnce(sessionDir, compression, {
          compareDir: otherDir,
        });
        assert.equal(code, 0);
        assert.equal(report.kind, "source_step_evaluated");
        assert.equal(report.relation, "different_source");
        assert.deepEqual(report.commit_gate, {
          decision: "reject_different_source",
        });
      },
    );

    stage = `${compression}_empty_or_v0_only_directory`;
    const emptyDir = join(run, stage);
    await fs.mkdir(emptyDir);
    const lost = runOnce(emptyDir, compression);
    assert.equal(lost.code, 2);
    assert.equal(lost.report.kind, "changed");
    assert.equal(lost.report.reason, "selection_lost");
    await fs.writeFile(
      join(emptyDir, `session.jsonl${suffix(compression)}`),
      Buffer.from("v0"),
      { flag: "wx" },
    );
    const oldGen = runOnce(emptyDir, compression);
    assert.equal(oldGen.code, 2);
    assert.equal(oldGen.report.kind, "unsupported");
    assert.equal(oldGen.report.reason, "physical_generation");
    checks[stage] = "passed";
  }

  await withFixtureSession(
    "reframed_events_preserve_prefix_and_noop_gate",
    "zstd",
    artifact,
    async ({ sessionDir, path, content }) => {
      const allRows = rows(artifact);
      const reframed = Buffer.concat([
        pack(allRows[0]),
        pack(Buffer.concat(allRows.slice(1))),
      ]);
      assert.notEqual(hash(reframed), hash(content));
      const { first, second } = await runTwoStep(
        sessionDir,
        "zstd",
        async () => {
          await delay(20);
          await fs.writeFile(path, reframed);
        },
      );
      assert.notEqual(first.snapshot_sha256, second.current.snapshot_sha256);
      assert.equal(first.jsonl_sha256, second.current.jsonl_sha256);
      assert.equal(second.relation, "prefix_preserved");
      assert.deepEqual(second.commit_gate, {
        decision: "no_op_unchanged",
        verified_events: artifact.events.length,
      });
    },
  );

  assert.equal(Object.keys(checks).length, 35);
  process.stdout.write(
    `${JSON.stringify(
      {
        status: "ok",
        dsh_version: SUPPORTED_DSH_VERSION,
        host_node_version: process.version,
        execution_target: inWsl ? "wsl_linux" : process.platform,
        checks_passed: Object.keys(checks).length,
        checks,
        observations,
        rust_source_selection_validated: true,
        rust_pinned_handle_identity_validated: true,
        rust_prefix_and_lifecycle_comparison_validated: true,
        rust_conservative_commit_gate_validated: true,
        normalized_positions_available: false,
        production_advance_permit: false,
        provider_registered: false,
      },
      null,
      2,
    )}\n`,
  );
}

main().catch((error) => {
  process.stderr.write(
    `${JSON.stringify({
      status: "failed",
      stage,
      code: error?.code ?? error?.name ?? "ERR",
      message: error?.message ?? String(error),
    })}\n`,
  );
  process.exit(1);
});

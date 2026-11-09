#!/usr/bin/env node
/**
 * 验证 Rust DSH v4 归一化位置映射（NormalizedDshSnapshot）、用法诚实性（重试去重 / 未知 cache 桶不填 0）
 * 以及真实 TotalStore + DshSavedState 提交规划与持久化重放幂等。
 */
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { constants, zstdCompressSync } from "node:zlib";
import { seedWithTurn, withRetry } from "../tests/fixtures/dsh-v4/scenarios.mjs";
import { loadDshInstallation, SUPPORTED_DSH_VERSION } from "./dsh-installed.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..");
const baseFixture = JSON.parse(
  fs.readFileSync(path.join(repoRoot, "tests", "fixtures", "dsh-v4", "complete-turn.json"), "utf8"),
);

function parseArgs(argv) {
  let dshPackage = null;
  let binary = null;
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === "--dsh-package" && i + 1 < argv.length) {
      dshPackage = argv[i + 1];
      i += 1;
    } else if (argv[i] === "--binary" && i + 1 < argv.length) {
      binary = argv[i + 1];
      i += 1;
    }
  }
  return { dshPackage, binary };
}

function defaultBinaryPath() {
  const exe = process.platform === "win32" ? "dsh_snapshot.exe" : "dsh_snapshot";
  return path.join(repoRoot, "target", "debug", "examples", exe);
}

function runSnapshotCli(binary, args) {
  const res = spawnSync(binary, args, {
    encoding: "utf8",
    windowsHide: true,
    timeout: 30_000,
  });
  if (res.error) {
    throw res.error;
  }
  const stdout = (res.stdout || "").trim();
  const parsed = stdout ? JSON.parse(stdout.split(/\r?\n/)[0]) : null;
  return { status: res.status, stdout, stderr: res.stderr, json: parsed };
}

function headerRecord(header) {
  return { type: "session", ...header };
}

function packFrame(text) {
  return zstdCompressSync(Buffer.from(text, "utf8"), {
    params: { [constants.ZSTD_c_checksumFlag]: 1 },
  });
}

function writePlainSession(sessionDir, header, events) {
  fs.mkdirSync(sessionDir, { recursive: true });
  const lines = [JSON.stringify(headerRecord(header)), ...events.map((ev) => JSON.stringify(ev))];
  fs.writeFileSync(path.join(sessionDir, "session.v4.jsonl"), `${lines.join("\n")}\n`, "utf8");
}

function writeZstdSession(sessionDir, header, events) {
  fs.mkdirSync(sessionDir, { recursive: true });
  const frames = [packFrame(`${JSON.stringify(headerRecord(header))}\n`)];
  if (events.length > 0) {
    const body = `${events.map((ev) => JSON.stringify(ev)).join("\n")}\n`;
    frames.push(packFrame(body));
  }
  fs.writeFileSync(path.join(sessionDir, "session.v4.jsonl.zstd"), Buffer.concat(frames));
}

function appendCompleteTurn(doc, turnIndex, userText, assistantText, usageOverride = null) {
  const next = structuredClone(doc);
  let seq = next.events.length;
  let time = (next.events.at(-1)?.time ?? 0) + 10;
  const usage = usageOverride ?? {
    inputTokens: 10,
    outputTokens: 5,
    cacheReadTokens: 0,
    cacheWriteTokens: 0,
    totalTokens: 15,
    reasoningTokens: 0,
  };
  const turnEvents = [
    { type: "turn/start", seq: seq++, time: time++, data: { turn: turnIndex } },
    { type: "step/start", seq: seq++, time: time++, data: { turn: turnIndex, step: 1 } },
    {
      type: "user/message",
      seq: seq++,
      time: time++,
      surfaceOp: "append",
      data: {
        id: `u-t${turnIndex}`,
        role: "user",
        source: { kind: "user" },
        content: [{ type: "text", text: userText }],
      },
    },
    {
      type: "assistant/message",
      seq: seq++,
      time: time++,
      surfaceOp: "append",
      data: {
        turn: turnIndex,
        step: 1,
        message: {
          id: `a-t${turnIndex}`,
          role: "assistant",
          source: { kind: "model", provider: "p0", model: "m0" },
          content: [{ type: "text", text: assistantText }],
        },
        stream: [
          { type: "text-chunks", time0: time, index: 0, dt: [], texts: [assistantText] },
          { type: "chunk", time: time + 1, chunk: { type: "usage", usage } },
          { type: "chunk", time: time + 2, chunk: { type: "finish", reason: { kind: "stop" } } },
        ],
        usage,
      },
    },
    { type: "step/end", seq: seq++, time: (time += 3), data: { turn: turnIndex, step: 1 } },
    {
      type: "turn/end",
      seq: seq++,
      time: time + 1,
      data: { turn: turnIndex, reason: { kind: "completed" } },
    },
  ];
  next.events.push(...turnEvents);
  return next;
}

function resolveDefaultDshPackage() {
  if (process.platform === "win32" && process.env.APPDATA) {
    const winCandidate = path.join(
      process.env.APPDATA,
      "npm",
      "node_modules",
      "@deepseek-ai",
      "dsh",
      "package.json",
    );
    if (fs.existsSync(winCandidate)) {
      return winCandidate;
    }
  }
  const npmCmd = process.platform === "win32" ? "npm.cmd" : "npm";
  const npmRoot = spawnSync(npmCmd, ["root", "-g"], {
    encoding: "utf8",
    windowsHide: true,
  });
  if (npmRoot.status === 0 && npmRoot.stdout) {
    const candidate = path.join(npmRoot.stdout.trim(), "@deepseek-ai", "dsh", "package.json");
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  throw new Error("Please pass --dsh-package <path/to/@deepseek-ai/dsh/package.json>");
}

async function main() {
  const { dshPackage: cliPkg, binary: cliBin } = parseArgs(process.argv.slice(2));
  const pkgPath = cliPkg ? path.resolve(cliPkg) : resolveDefaultDshPackage();
  const installation = loadDshInstallation(pkgPath);
  const binary = cliBin ? path.resolve(cliBin) : defaultBinaryPath();
  if (!fs.existsSync(binary)) {
    throw new Error(`dsh_snapshot binary not found at ${binary}; build with --features store,dsh-experimental first`);
  }

  const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), "sv-dsh-norm-store-"));
  const checks = [];
  const check = (name, cond, detail = {}) => {
    checks.push({ name, ok: Boolean(cond), detail });
    if (!cond) {
      throw new Error(`Check failed: ${name} -> ${JSON.stringify(detail)}`);
    }
  };

  try {
    // 1. 验证带配对重试 + 部分 cache 桶未知的会话（withRetry + partial cache bucket）
    const retryDoc = withRetry(baseFixture);
    const partialTurnDoc = appendCompleteTurn(retryDoc, 2, "second user turn", "second assistant reply", {
      inputTokens: 20,
      outputTokens: 8,
      totalTokens: 30,
    });
    const retryDir = path.join(tmpRoot, "retry-session");
    writePlainSession(retryDir, partialTurnDoc.header, partialTurnDoc.events);

    const retryRes = runSnapshotCli(binary, [
      "--session-dir",
      retryDir,
      "--source-instance",
      "inst-retry",
      "--compression",
      "none",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--normalize",
      "--positions",
    ]);
    check("retry_exit_0", retryRes.status === 0, retryRes);
    check("retry_positions_available", retryRes.json.normalized_positions_available === true, retryRes.json);
    check(
      "retry_usage_summary",
      retryRes.json.normalization.usage_summary.total_native_usage_records === 3 &&
        retryRes.json.normalization.usage_summary.emitted_exact_four_bucket === 2 &&
        retryRes.json.normalization.usage_summary.partial_or_unreconciled === 1 &&
        retryRes.json.normalization.usage_summary.superseded_by_same_step === 0,
      retryRes.json.normalization.usage_summary,
    );
    const failedAttemptEntry = retryRes.json.usage_ledger.find((u) => u.source_kind === "assistant/attempt");
    check(
      "retry_failed_attempt_keeps_own_null_model",
      failedAttemptEntry &&
        failedAttemptEntry.status === "exact_four_bucket" &&
        failedAttemptEntry.model === null,
      failedAttemptEntry,
    );

    // 2. 验证 child 会话（seedWithTurn）抑制 inherited_event_count 前缀
    const childDoc = seedWithTurn(baseFixture, "child-01");
    const childDir = path.join(tmpRoot, "child-session");
    writePlainSession(childDir, childDoc.header, childDoc.events);
    const childRes = runSnapshotCli(binary, [
      "--session-dir",
      childDir,
      "--source-instance",
      "inst-child",
      "--compression",
      "none",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--normalize",
      "--positions",
    ]);
    check("child_exit_0", childRes.status === 0, childRes);
    check(
      "child_inherited_suppressed",
      childRes.json.normalization.inherited_event_count === childDoc.inheritedEventCount &&
        childRes.json.normalization.suppressed_inherited_records === childDoc.inheritedEventCount &&
        childRes.json.normalization.emitted_raw_events === 3 &&
        childRes.json.normalized_event_spans.every((s) => s.native_seq >= childDoc.inheritedEventCount),
      childRes.json,
    );

    // 3. 验证真实 TotalStore + DshSavedState 的增量、两次追加后改第 0 段、崩溃重放幂等、修尾缩短保持、生命周期变化与未来代阻断
    const storeDb = path.join(tmpRoot, "total_store.db");
    const stateFile = path.join(tmpRoot, "dsh_state.json");
    const liveDir = path.join(tmpRoot, "live-session");
    const docTurn1 = structuredClone(baseFixture);
    writeZstdSession(liveDir, docTurn1.header, docTurn1.events);

    const runStoreStep = (extraArgs = []) =>
      runSnapshotCli(binary, [
        "--session-dir",
        liveDir,
        "--source-instance",
        "inst-store",
        "--compression",
        "zstd",
        "--node",
        process.execPath,
        "--dsh-package",
        installation.anchor,
        "--store-db",
        storeDb,
        "--state-file",
        stateFile,
        ...extraArgs,
      ]);

    // Step A: Initial projection (complete-turn.json 产出 8 条 RawEvent: sys, dev, user, thinking, assistant msg, usage, tool call, tool result)
    const stepA = runStoreStep();
    check("stepA_exit_0", stepA.status === 0, stepA);
    check(
      "stepA_initial_append",
      stepA.json.commit_gate.decision === "initial_projection_candidate" &&
        stepA.json.plan.store === "project_append" &&
        stepA.json.projection_stats.appended === 8 &&
        stepA.json.store_current_events === 8 &&
        stepA.json.persisted_state_file === true,
      stepA.json,
    );

    // Step B: No-op unchanged
    const stepB = runStoreStep();
    check(
      "stepB_noop",
      stepB.json.commit_gate.decision === "no_op_unchanged" &&
        stepB.json.observed_events_in_batch === 0 &&
        stepB.json.projection_stats.appended === 0 &&
        stepB.json.store_current_events === 8,
      stepB.json,
    );

    // Step C: Append #1 (Turn 2: +3 RawEvents -> user msg, assistant msg, usage)
    const docTurn2 = appendCompleteTurn(docTurn1, 2, "turn 2 user", "turn 2 assistant");
    writeZstdSession(liveDir, docTurn2.header, docTurn2.events);
    const stepC = runStoreStep();
    check(
      "stepC_append_1",
      stepC.json.commit_gate.decision === "incremental_append_candidate" &&
        stepC.json.plan.index === "append_facts" &&
        stepC.json.observed_events_in_batch === 3 &&
        stepC.json.projection_stats.appended === 3 &&
        stepC.json.store_current_events === 11,
      stepC.json,
    );

    // Step D: Append #2 (Turn 3: +3 RawEvents -> total 14 RawEvents)
    const docTurn3 = appendCompleteTurn(docTurn2, 3, "turn 3 user", "turn 3 assistant");
    writeZstdSession(liveDir, docTurn3.header, docTurn3.events);
    const stepD = runStoreStep();
    check(
      "stepD_append_2",
      stepD.json.commit_gate.decision === "incremental_append_candidate" &&
        stepD.json.observed_events_in_batch === 3 &&
        stepD.json.projection_stats.appended === 3 &&
        stepD.json.store_current_events === 14,
      stepD.json,
    );

    // Step E: 两次追加后改写第 0 段（修改 seq 4 user/message 的正文，总事件数不变），同时模拟写库后未保存 state_file 崩溃
    const docRewrittenSeg0 = structuredClone(docTurn3);
    docRewrittenSeg0.events[4].data.content[0].text = "rewritten first user message in segment 0";
    writeZstdSession(liveDir, docRewrittenSeg0.header, docRewrittenSeg0.events);
    const stepECrash = runStoreStep(["--simulate-crash-before-save-state"]);
    check(
      "stepE_rewrite_seg0_detected_and_rolled_back",
      stepECrash.json.relation === "prefix_rewritten" &&
        stepECrash.json.commit_gate.decision === "rebuild_projection_candidate" &&
        stepECrash.json.plan.store === "project_rollback" &&
        stepECrash.json.projection_stats.source_revision === 1 &&
        stepECrash.json.projection_stats.head_moved === true &&
        stepECrash.json.persisted_state_file === false,
      stepECrash.json,
    );

    // Step F: 崩溃重放（state_file 仍停留在 Step D）：ProjectionToken 必须幂等，不新开第 2 代
    const stepFReplay = runStoreStep();
    check(
      "stepF_crash_replay_idempotent",
      stepFReplay.json.commit_gate.decision === "rebuild_projection_candidate" &&
        stepFReplay.json.plan.store === "project_rollback" &&
        stepFReplay.json.projection_stats.source_revision === 1 &&
        stepFReplay.json.projection_stats.head_moved === false &&
        stepFReplay.json.persisted_state_file === true &&
        stepFReplay.json.store_current_events === 14,
      stepFReplay.json,
    );

    // Step G: zstd 修尾中间态（回退到仅含前 2 轮，事件数变少）：即使传 --force 也必须 HoldPriorProjection + Preserve + Freeze
    const docTurn2Rewritten = structuredClone(docTurn2);
    docTurn2Rewritten.events[4].data.content[0].text = "rewritten first user message in segment 0";
    writeZstdSession(liveDir, docTurn2Rewritten.header, docTurn2Rewritten.events);
    const stepGShrank = runStoreStep(["--force"]);
    check(
      "stepG_prefix_shrank_holds_prior_projection",
      stepGShrank.json.relation === "prefix_shrank" &&
        stepGShrank.json.commit_gate.decision === "hold_prior_projection" &&
        stepGShrank.json.plan.store === "preserve" &&
        stepGShrank.json.plan.cursor === "freeze" &&
        stepGShrank.json.advanced_saved_state === false &&
        stepGShrank.json.store_current_events === 14 &&
        stepGShrank.json.store_head.source_revision === 1,
      stepGShrank.json,
    );

    // Step H: 生命周期变化（同一 session_id 下触发 seedWithTurn 产生新的 lifecycle_sha256）-> Rollback 开第 2 代
    const docSeededSameId = seedWithTurn(docTurn2Rewritten, docTurn1.header.id);
    writeZstdSession(liveDir, docSeededSameId.header, docSeededSameId.events);
    const stepHLifecycle = runStoreStep();
    check(
      "stepH_lifecycle_changed_rolls_back",
      stepHLifecycle.json.relation === "lifecycle_changed" &&
        stepHLifecycle.json.commit_gate.decision === "rebuild_projection_candidate" &&
        stepHLifecycle.json.plan.store === "project_rollback" &&
        stepHLifecycle.json.projection_stats.source_revision === 2 &&
        stepHLifecycle.json.projection_stats.head_moved === true &&
        stepHLifecycle.json.store_current_events === 3,
      stepHLifecycle.json,
    );

    // Step I: 出现未来代 session.v5.jsonl.zstd：选代阻断 -> HoldPriorProjection + Preserve + Freeze
    fs.writeFileSync(
      path.join(liveDir, "session.v5.jsonl.zstd"),
      fs.readFileSync(path.join(liveDir, "session.v4.jsonl.zstd")),
    );
    const stepIFutureGen = runStoreStep(["--force"]);
    check(
      "stepI_future_generation_holds_prior_projection",
      stepIFutureGen.json.commit_gate.decision === "hold_prior_projection" &&
        stepIFutureGen.json.plan.store === "preserve" &&
        stepIFutureGen.json.plan.cursor === "freeze" &&
        stepIFutureGen.json.advanced_saved_state === false &&
        stepIFutureGen.json.store_head.source_revision === 2 &&
        stepIFutureGen.json.store_current_events === 3,
      stepIFutureGen.json,
    );

    console.log(
      JSON.stringify({
        ok: true,
        platform: process.platform,
        dsh_version: SUPPORTED_DSH_VERSION,
        total_checks: checks.length,
        passed_checks: checks.filter((c) => c.ok).length,
      }),
    );
  } finally {
    fs.rmSync(tmpRoot, { recursive: true, force: true });
  }
}

await main();

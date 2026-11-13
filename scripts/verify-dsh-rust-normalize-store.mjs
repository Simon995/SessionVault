#!/usr/bin/env node
/**
 * 验证 Rust DSH v4 归一化位置映射（NormalizedDshSnapshot）、用法诚实性（重试去重 / 未知 cache 桶不填 0）
 * 以及真实 TotalStore + DshSavedState 提交规划与持久化重放幂等。
 */
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import { createProtectedRun } from "../tests/fixtures/dsh-v4/io-harness.mjs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { constants, zstdCompressSync } from "node:zlib";
import {
  seedWithTurn,
  unseededSubagentChild,
  withCompactionAndSubagentCatalog,
  withRetry,
} from "../tests/fixtures/dsh-v4/scenarios.mjs";
import { loadDshInstallation } from "./dsh-installed.mjs";

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

  const tmpRoot = createProtectedRun(repoRoot, "dsh-norm-store-");
  const statusPath = path.join(tmpRoot, "status.json");
  const status = { status: "running", dsh_version: installation.version, binary, checks: [] };
  const saveStatus = () => fs.writeFileSync(statusPath, JSON.stringify(status, null, 2));
  saveStatus();
  console.error(JSON.stringify({ run: path.relative(repoRoot, tmpRoot) }));
  let invocation = 0;
  const runSnapshotCli = (binary, args) => {
    const result = spawnSync(binary, args, {
      encoding: "utf8", windowsHide: true, timeout: 30_000,
    });
    const stem = path.join(tmpRoot, `invocation-${++invocation}`);
    fs.writeFileSync(`${stem}.stdout.log`, result.stdout ?? "", { flag: "wx" });
    fs.writeFileSync(`${stem}.stderr.log`, result.stderr ?? "", { flag: "wx" });
    fs.writeFileSync(`${stem}.json`, JSON.stringify({ args, rc: result.status, signal: result.signal, error: result.error?.code }, null, 2), { flag: "wx" });
    if (result.error) throw result.error;
    const stdout = (result.stdout || "").trim();
    const parsed = stdout ? JSON.parse(stdout.split(/\r?\n/)[0]) : null;
    return { status: result.status, stdout, stderr: result.stderr, json: parsed };
  };
  const checks = [];
  const check = (name, cond, detail = {}) => {
    checks.push({ name, ok: Boolean(cond), detail });
    status.checks = checks;
    saveStatus();
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

    // Step J: 真实 @deepseek-ai/dsh v4 runtime 下的 Compaction + surfaceOp:"replace" 表面折叠 + SubagentCatalog + 控制面绑定 InputOrigin 保真
    const complexDir = path.join(tmpRoot, "complex-compact-session");
    const complexDoc = withCompactionAndSubagentCatalog(baseFixture, "session-compact-v4");
    writeZstdSession(complexDir, complexDoc.header, complexDoc.events);
    const complexStoreDb = path.join(tmpRoot, "complex_store.db");
    const complexStateFile = path.join(tmpRoot, "complex_state.json");
    const complexBindingsFile = path.join(tmpRoot, "complex_bindings.json");
    fs.writeFileSync(
      complexBindingsFile,
      JSON.stringify({
        schemaVersion: 1,
        bindings: [
          {
            schemaVersion: 1,
            owner: { namespace: "tumechat.user", id: "u_main" },
            host: { namespace: "tumechat.host", id: "h_win" },
            session: { namespace: "tumechat.session", id: "s_compact" },
            assignment: { namespace: "tumechat.assignment", id: "a_compact" },
            controller: "product-adapter",
            nativeSource: {
              backend: "dsh-native",
              sourceLocation: "local",
              sourcePath: complexDir,
              nativeSessionId: complexDoc.header.id,
            },
            temporary: false,
            readMemory: false,
            verifiedHumanInput: true,
            recordedAt: "2026-11-09T09:00:00Z",
          },
        ],
      }),
      "utf8",
    );
    const stepJComplex = runSnapshotCli(binary, [
      "--session-dir",
      complexDir,
      "--source-instance",
      "live-complex-instance",
      "--compression",
      "zstd",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--store-db",
      complexStoreDb,
      "--state-file",
      complexStateFile,
      "--bindings-file",
      complexBindingsFile,
      "--positions",
    ]);
    const jJson = stepJComplex.json;
    const jSpans = jJson?.normalized_event_spans || [];
    const jOrigins = jJson?.observed_user_input_origins || [];
    // 在 baseFixture 中，表面节点为 seq 2 (system), 3 (developer), 4 (user), 5 (assistant), 7 (tool/result)。
    // checkpoint 位于 seq 13（surfaceOp: { op: "replace", startSeq: 3, endSeq: 7 }），
    // seq 2 是 system/message 受保护，被折叠替换的是 [3, 4, 5, 7]，保留 seq 2！
    const spanSeq2 = jSpans.find((s) => s.native_seq === 2);
    const spanSeq4 = jSpans.find((s) => s.native_seq === 4);
    const spanSeq13 = jSpans.find((s) => s.native_seq === 13);
    check(
      "stepJ_compaction_surface_fold_and_input_origins",
      stepJComplex.status === 0 &&
        jJson.admission_status === "persistent_bound" &&
        jJson.subagent_catalog.length === 1 &&
        jJson.subagent_catalog[0].child_id === "session-compact-v4-sub-1" &&
        jJson.compaction_records.length === 3 &&
        jJson.surface_replacements.length === 1 &&
        JSON.stringify(jJson.surface_replacements[0].replaced_surface_seqs) ===
          JSON.stringify([3, 4, 5, 7]) &&
        spanSeq2?.active_on_surface === true &&
        spanSeq4?.active_on_surface === false &&
        spanSeq4?.shadowed_by_native_seq === 13 &&
        spanSeq13?.active_on_surface === true &&
        spanSeq13?.surface_op === "replace" &&
        jOrigins.length === 5 &&
        // 第 1 条 user/message（seq 4，source.kind="user"）在受控会话中升级为 human_verified
        jOrigins[0].sender_kind === "human_verified" &&
        jOrigins[0].trust_level === "human_verified" &&
        // 第 2 条（compact-checkpoint）、第 3 条（subagent-settled）、第 4 条（goal）、第 5 条（team-message）绝不升级为 human_verified
        jOrigins[1].sender_kind === "subagent_or_runtime" &&
        jOrigins[1].basis === "dsh_compaction_checkpoint" &&
        jOrigins[2].sender_kind === "subagent_or_runtime" &&
        jOrigins[2].basis === "dsh_subagent_settlement" &&
        jOrigins[2].sender_session === "session-compact-v4-sub-1" &&
        jOrigins[3].sender_kind === "coordinator_dispatch" &&
        jOrigins[3].basis === "dsh_source_coordinator" &&
        jOrigins[4].sender_kind === "peer_session" &&
        jOrigins[4].basis === "dsh_source_peer",
      jJson,
    );

    // Step K: 无 seed 独立子代理（isSeeded: false, origin: "subagent", subagent/descriptor）、多层 seed fork（s0 -> s1 -> s2）及父临时会话血缘继承排除
    const unseededSubDir = path.join(tmpRoot, "unseeded-subagent-dir");
    const unseededSubDoc = unseededSubagentChild(
      baseFixture.header,
      "session-compact-v4",
      "session-compact-v4-sub-1",
    );
    writeZstdSession(unseededSubDir, unseededSubDoc.header, unseededSubDoc.events);
    const stepKUnseeded = runSnapshotCli(binary, [
      "--session-dir",
      unseededSubDir,
      "--source-instance",
      "subagent-instance-1",
      "--compression",
      "zstd",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--normalize",
      "--positions",
    ]);
    const kNorm = stepKUnseeded.json?.normalization;
    check(
      "stepK_unseeded_subagent_descriptor_and_origin",
      stepKUnseeded.status === 0 &&
        kNorm?.is_seeded === false &&
        kNorm?.session_origin === "subagent" &&
        kNorm?.parent_session_id === "session-compact-v4" &&
        kNorm?.inherited_event_count === 0 &&
        kNorm?.subagent_descriptor_count === 1 &&
        stepKUnseeded.json.subagent_descriptors[0].mode === "continuable" &&
        stepKUnseeded.json.subagent_descriptors[0].label === "audit",
      stepKUnseeded.json,
    );

    // 多层 seed fork（s0 -> mid1 -> grandchild2）
    const mid1Doc = seedWithTurn(baseFixture, "session-mid-1");
    const grandchild2Doc = seedWithTurn(mid1Doc, "session-grandchild-2");
    const grandchildDir = path.join(tmpRoot, "grandchild-fork-dir");
    writeZstdSession(grandchildDir, grandchild2Doc.header, grandchild2Doc.events);
    const stepKGrandchild = runSnapshotCli(binary, [
      "--session-dir",
      grandchildDir,
      "--source-instance",
      "grandchild-instance",
      "--compression",
      "zstd",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--normalize",
      "--positions",
    ]);
    check(
      "stepK_multilevel_seed_fork_suppresses_all_ancestors",
      stepKGrandchild.status === 0 &&
        stepKGrandchild.json.normalization.is_seeded === true &&
        stepKGrandchild.json.normalization.parent_session_id === "session-mid-1" &&
        stepKGrandchild.json.normalization.inherited_event_count === mid1Doc.events.length &&
        stepKGrandchild.json.normalization.suppressed_inherited_records === mid1Doc.events.length &&
        stepKGrandchild.json.normalization.emitted_raw_events === 3 &&
        stepKGrandchild.json.normalized_event_spans.every(
          (s) => s.native_seq >= mid1Doc.events.length,
        ),
      stepKGrandchild.json,
    );

    // 父会话标为 temporary: true 时，子代理会话通过 parentSession 自动继承 temporary_excluded，不写库、不推进状态
    const tempBindingsFile = path.join(tmpRoot, "temp_parent_bindings.json");
    fs.writeFileSync(
      tempBindingsFile,
      JSON.stringify({
        schemaVersion: 1,
        bindings: [
          {
            schemaVersion: 1,
            owner: { namespace: "tumechat.user", id: "u_main" },
            host: { namespace: "tumechat.host", id: "h_win" },
            session: { namespace: "tumechat.session", id: "s_temp_parent" },
            controller: "product-adapter",
            nativeSource: {
              backend: "dsh-native",
              sourceLocation: "local",
              nativeSessionId: "session-compact-v4",
            },
            temporary: true,
            readMemory: true,
            recordedAt: "2026-11-09T09:05:00Z",
          },
        ],
      }),
      "utf8",
    );
    const tempSubStoreDb = path.join(tmpRoot, "temp_sub_store.db");
    const tempSubStateFile = path.join(tmpRoot, "temp_sub_state.json");
    const stepKTempInherited = runSnapshotCli(binary, [
      "--session-dir",
      unseededSubDir,
      "--source-instance",
      "subagent-instance-1",
      "--compression",
      "zstd",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--store-db",
      tempSubStoreDb,
      "--state-file",
      tempSubStateFile,
      "--bindings-file",
      tempBindingsFile,
    ]);
    check(
      "stepK_subagent_inherits_parent_temporary_exclusion",
      stepKTempInherited.status === 0 &&
        stepKTempInherited.json.admission_status === "temporary_excluded" &&
        stepKTempInherited.json.events_excluded_temporary === 4 &&
        stepKTempInherited.json.advanced_saved_state === false &&
        stepKTempInherited.json.store_current_events === 0,
      stepKTempInherited.json,
    );

    // Step L: DSH 会话根目录批量发现（--sessions-root）：v4 Ready、v0..v3 拒绝、v5+ 拒绝不回退、编码冲突拒绝
    const discRoot = path.join(tmpRoot, "dsh-sessions-root");
    writeZstdSession(path.join(discRoot, "01-ready-zstd"), baseFixture.header, baseFixture.events);
    writePlainSession(
      path.join(discRoot, "02-ready-plain"),
      baseFixture.header,
      baseFixture.events,
    );
    const legacyDir = path.join(discRoot, "03-legacy-v3");
    fs.mkdirSync(legacyDir, { recursive: true });
    fs.writeFileSync(path.join(legacyDir, "session.v3.jsonl.zstd"), "legacy-v3");
    const futureDir = path.join(discRoot, "04-future-v5");
    writeZstdSession(futureDir, baseFixture.header, baseFixture.events);
    fs.writeFileSync(path.join(futureDir, "session.v5.jsonl.zstd"), "future-v5");
    const conflictDir = path.join(discRoot, "05-encoding-conflict");
    writeZstdSession(conflictDir, baseFixture.header, baseFixture.events);
    writePlainSession(conflictDir, baseFixture.header, baseFixture.events);

    const stepLDiscover = runSnapshotCli(binary, [
      "--sessions-root",
      discRoot,
      "--compression",
      "zstd",
    ]);
    const lEntries = stepLDiscover.json?.entries || [];
    check(
      "stepL_sessions_root_discovery_v4_only_contract",
      stepLDiscover.status === 0 &&
        stepLDiscover.json.kind === "dsh_sessions_root_discovered" &&
        stepLDiscover.json.ready_v4_count === 2 &&
        stepLDiscover.json.blocked_count === 3 &&
        lEntries[0]?.outcome?.status === "ready_v4" &&
        lEntries[0]?.outcome?.compression === "zstd" &&
        lEntries[1]?.outcome?.status === "ready_v4" &&
        lEntries[1]?.outcome?.compression === "none" &&
        lEntries[2]?.outcome?.status === "blocked" &&
        lEntries[2]?.outcome?.blocked?.reason === "physical_generation" &&
        lEntries[3]?.outcome?.status === "blocked" &&
        lEntries[3]?.outcome?.blocked?.reason === "physical_generation" &&
        lEntries[4]?.outcome?.status === "blocked" &&
        lEntries[4]?.outcome?.blocked?.reason === "encoding_conflict",
      stepLDiscover.json,
    );

    // Step M: DSH 会话根目录批量扫描与入库（scan_dsh_sessions_root_to_store）：首轮入库 2 个 v4 会话（16 条事件），第 2 轮幂等 no-op，第 3 轮出现 v5 时 hold_prior_projection
    const rootStoreDb = path.join(tmpRoot, "root_scan_store.db");
    const rootStateFile = path.join(tmpRoot, "root_scan_states.json");
    const stepM1 = runSnapshotCli(binary, [
      "--sessions-root",
      discRoot,
      "--compression",
      "zstd",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--store-db",
      rootStoreDb,
      "--state-file",
      rootStateFile,
    ]);
    check(
      "stepM1_sessions_root_batch_scan_initial_commit",
      stepM1.status === 0 &&
        stepM1.json.kind === "dsh_sessions_root_scanned_and_committed" &&
        stepM1.json.ready_v4_count === 2 &&
        stepM1.json.blocked_count === 3 &&
        stepM1.json.projected_sessions === 2 &&
        stepM1.json.preserved_sessions === 3 &&
        stepM1.json.total_events_appended === 16 &&
        stepM1.json.store_current_events === 16,
      stepM1.json,
    );

    // 第 2 轮：无变化，2 个 v4 会话均 no_op_unchanged，0 条新增
    const stepM2 = runSnapshotCli(binary, [
      "--sessions-root",
      discRoot,
      "--compression",
      "zstd",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--store-db",
      rootStoreDb,
      "--state-file",
      rootStateFile,
    ]);
    check(
      "stepM2_sessions_root_batch_scan_noop_idempotent",
      stepM2.status === 0 &&
        stepM2.json.entries[0]?.commit_gate?.decision === "no_op_unchanged" &&
        stepM2.json.entries[1]?.commit_gate?.decision === "no_op_unchanged" &&
        stepM2.json.total_events_appended === 0 &&
        stepM2.json.store_current_events === 16,
      stepM2.json,
    );

    // 第 3 轮：在已入库的 01-ready-zstd 下出现未来代 session.v5.jsonl.zstd -> 触发 hold_prior_projection，保持库内 16 条事件不丢
    fs.writeFileSync(
      path.join(discRoot, "01-ready-zstd", "session.v5.jsonl.zstd"),
      "future-v5",
    );
    const stepM3 = runSnapshotCli(binary, [
      "--sessions-root",
      discRoot,
      "--compression",
      "zstd",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--store-db",
      rootStoreDb,
      "--state-file",
      rootStateFile,
      "--force",
    ]);
    check(
      "stepM3_sessions_root_future_v5_holds_prior_v4_projection",
      stepM3.status === 0 &&
        stepM3.json.ready_v4_count === 1 &&
        stepM3.json.blocked_count === 4 &&
        stepM3.json.entries[0]?.commit_gate?.decision === "hold_prior_projection" &&
        stepM3.json.store_current_events === 16,
      stepM3.json,
    );

    // Step N: 真实 @deepseek-ai/dsh v4 runtime 下的细粒度用量保真（五项事实 + TotalStore 持久化汇总）
    // 包含：
    // - Turn 1 (withRetry): attempt 0 (stream usage=17, UnknownNotReported) + attempt 1 (ExactFourBucket=15, Reported)
    // - Turn 2: derive_total_from_known_buckets (10+5+2+3 -> DerivedFromCompleteBuckets, exact_total=20)
    // - Turn 3: exact_total_unknown_cache (input=10, output=5, total=20 -> ExactTotalWithUnknownCache, exact_total=20, cache=null)
    // - Turn 4: unknown_cache_without_total (input=10, output=5 -> PartialInsufficientForTotal, diagnostic=unknown_cache_without_total)
    // - Turn 5: total_below_known_buckets (input=10, output=5, total=14 -> InvalidOrContradictory, diagnostic=total_below_known_buckets)
    let fidelityDoc = withRetry(baseFixture);
    const fidelityTurn2 = appendCompleteTurn(fidelityDoc, 2, "u2", "a2", {
      inputTokens: 10,
      outputTokens: 5,
      cacheReadTokens: 2,
      cacheWriteTokens: 3,
    });
    const fidelityTurn3 = appendCompleteTurn(fidelityTurn2, 3, "u3", "a3", {
      inputTokens: 10,
      outputTokens: 5,
      totalTokens: 20,
    });
    const fidelityTurn4 = appendCompleteTurn(fidelityTurn3, 4, "u4", "a4", {
      inputTokens: 10,
      outputTokens: 5,
    });
    const fidelityTurn5 = appendCompleteTurn(fidelityTurn4, 5, "u5", "a5", {
      inputTokens: 10,
      outputTokens: 5,
      totalTokens: 14,
    });
    const fidelityDir = path.join(tmpRoot, "usage-fidelity-session");
    const fidelityStoreDb = path.join(tmpRoot, "usage_fidelity_store.db");
    const fidelityStateFile = path.join(tmpRoot, "usage_fidelity_state.json");
    writeZstdSession(fidelityDir, fidelityTurn5.header, fidelityTurn5.events);

    const stepNFidelity = runSnapshotCli(binary, [
      "--session-dir",
      fidelityDir,
      "--source-instance",
      "inst-fidelity",
      "--compression",
      "zstd",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--store-db",
      fidelityStoreDb,
      "--state-file",
      fidelityStateFile,
      "--positions",
    ]);
    const nJson = stepNFidelity.json;
    const nLedger = nJson?.usage_ledger || [];
    const nSummary = nJson?.store_usage_summary;
    check(
      "stepN_usage_fidelity_five_facts_and_store_summary",
      stepNFidelity.status === 0 &&
        nLedger.length === 6 &&
        nLedger[0].availability === "exact_four_bucket" &&
        nLedger[0].route_status?.status === "unknown_not_reported" &&
        nLedger[0].selected_sample_origin?.kind === "stream_chunk" &&
        nLedger[1].availability === "exact_four_bucket" &&
        nLedger[1].attempt_index === 1 &&
        nLedger[1].overridden_sample_count === 2 &&
        nLedger[2].availability === "exact_four_bucket" &&
        nLedger[2].total_provenance === "derived_from_complete_buckets" &&
        nLedger[2].exact_total_tokens === 20 &&
        nLedger[3].availability === "exact_total_with_unknown_cache" &&
        nLedger[3].exact_total_tokens === 20 &&
        nLedger[3].cache_read_tokens === null &&
        nLedger[3].cache_write_tokens === null &&
        nLedger[4].availability === "partial_insufficient_for_total" &&
        nLedger[4].diagnostic_reasons.includes("unknown_cache_without_total") &&
        nLedger[5].availability === "invalid_or_contradictory" &&
        nLedger[5].diagnostic_reasons.includes("total_below_known_buckets") &&
        nSummary?.total_evidence_records === 6 &&
        nSummary?.selected_for_settlement_count === 4 &&
        nSummary?.exact_four_bucket_count === 3 &&
        nSummary?.exact_total_with_unknown_cache_count === 1 &&
        nSummary?.partial_insufficient_for_total_count === 1 &&
        nSummary?.invalid_or_contradictory_count === 1 &&
        nSummary?.unknown_route_settled_count === 1 &&
        nSummary?.has_unknown_cache_buckets_in_settlement === true &&
        // 17 (attempt 0) + 20 (turn 1 msg) + 20 (turn 2 derived) + 20 (turn 3 unknown cache) = 77
        nSummary?.settled_exact_total_tokens === 77,
      nJson,
    );

    console.log(
      JSON.stringify({
        ok: true,
        platform: process.platform,
        dsh_version: installation.version,
        run: path.relative(repoRoot, tmpRoot),
        total_checks: checks.length,
        passed_checks: checks.filter((c) => c.ok).length,
      }),
    );
    status.status = "passed";
    status.rc = 0;
  } catch (error) {
    status.status = error.code === "ETIMEDOUT" ? "timed_out" : "failed";
    status.rc = 1;
    status.error = { code: error.code, message: error.message };
    throw error;
  } finally {
    saveStatus();
  }
}

await main();

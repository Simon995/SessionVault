#!/usr/bin/env node
/**
 * 使用显式安装版校验公开的合成夹具；不打开真实日志、不启动 Host/LLM。
 * node scripts/verify-dsh-v4.mjs --dsh-package <package.json>
 * native_restore 与 stream/usage 检查分层；这里只验证夹具涉及的字段子集。
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { parseArgs } from "node:util";
import { pathToFileURL } from "node:url";
import { constants, zstdCompressSync, zstdDecompressSync } from "node:zlib";
import {
  seedWithTurn,
  withRetry,
} from "../tests/fixtures/dsh-v4/scenarios.mjs";
import { loadDshInstallation } from "./dsh-installed.mjs";
const FIXTURE = new URL(
  "../tests/fixtures/dsh-v4/complete-turn.json",
  import.meta.url,
);
const counts = {};
let stage = "arguments";

function check(name, action) {
  stage = name;
  action();
  counts[name] = "passed";
}

function count(value) {
  assert(Number.isSafeInteger(value) && value >= 0);
}

function usageFields(usage) {
  count(usage.inputTokens);
  count(usage.outputTokens);
  for (const name of [
    "cacheReadTokens",
    "cacheWriteTokens",
    "reasoningTokens",
    "totalTokens",
  ]) {
    if (usage[name] !== undefined) count(usage[name]);
  }
  if (usage.reasoningTokens !== undefined)
    assert(usage.reasoningTokens <= usage.outputTokens);
}

function contentBlock(block) {
  assert(block && typeof block === "object");
  if (block.type === "text" || block.type === "reasoning") {
    assert.equal(typeof block.text, "string");
  } else {
    assert.equal(block.type, "tool-call");
    for (const name of ["id", "name", "arguments"])
      assert.equal(typeof block[name], "string");
  }
}

function fixtureChunk(chunk) {
  assert(chunk && typeof chunk === "object");
  if (
    [
      "block-start",
      "block-end",
      "text-delta",
      "reasoning-delta",
      "tool-call-delta",
    ].includes(chunk.type)
  ) {
    count(chunk.index);
  }
  switch (chunk.type) {
    case "block-start":
      assert(["text", "reasoning", "tool-call"].includes(chunk.blockType));
      break;
    case "block-end":
      contentBlock(chunk.block);
      break;
    case "text-delta":
    case "reasoning-delta":
      assert.equal(typeof chunk.text, "string");
      break;
    case "tool-call-delta":
      assert.equal(typeof chunk.id, "string");
      assert.equal(typeof chunk.argumentsDelta, "string");
      if (chunk.name !== undefined) assert.equal(typeof chunk.name, "string");
      break;
    case "usage":
      usageFields(chunk.usage);
      break;
    case "finish":
      assert(
        ["stop", "tool-calls", "max-tokens", "error", "aborted"].includes(
          chunk.reason.kind,
        ),
      );
      if (["error", "aborted"].includes(chunk.reason.kind)) {
        assert.equal(typeof chunk.reason.failure.message, "string");
        assert.equal(typeof chunk.reason.failure.code, "string");
      }
      break;
    default:
      assert.fail("unsupported fixture chunk");
  }
}

async function main() {
  const { values } = parseArgs({
    options: { "dsh-package": { type: "string" } },
  });
  assert(values["dsh-package"]);
  const { require: req, load, version } = loadDshInstallation(values["dsh-package"]);
  const format = await load("@deepseek-ai/dsh-session-format-v3-to-v4");
  const session = await load("@deepseek-ai/dsh-session");
  const llm = await load("@deepseek-ai/dsh-llm/assistant-stream");
  const llmEntry = await load("@deepseek-ai/dsh-llm");
  const meterDir = dirname(req.resolve("@deepseek-ai/dsh-token-meter"));
  const meter = await import(
    pathToFileURL(join(meterDir, "types/turn-usage.js")).href
  );
  const projection = (
    await import(
      pathToFileURL(join(meterDir, "types/usage-projection.js")).href
    )
  ).tokenUsageProjectionDefinition;
  const artifact = JSON.parse(readFileSync(FIXTURE, "utf8"));
  const assistant = artifact.events.find(
    (event) => event.type === "assistant/message",
  );
  assert(assistant);
  const usage = assistant.data.usage;
  const nativeRestore = (value) => {
    format.restoreReleasedV4Artifact(value, session.KNOWN_SESSION_EVENT_TYPES);
    return session.Session.create(
      value.header.id,
      value.events,
      value.header,
      value.inheritedEventCount,
    );
  };
  const assemble = (stream) => {
    const accumulator = new llmEntry.BlockAssembler();
    for (const timed of llm.expandAssistantStream(stream)) {
      fixtureChunk(timed.chunk);
      accumulator.push(timed.chunk);
    }
    return accumulator;
  };
  const validateFixture = (value) => {
    nativeRestore(value);
    for (const event of value.events) {
      if (!["assistant/message", "assistant/attempt"].includes(event.type))
        continue;
      const data = event.data;
      const assembled = assemble(data.stream);
      if (data.usage !== undefined) usageFields(data.usage);
      if (event.type === "assistant/message") {
        data.message.content.forEach(contentBlock);
        if (data.stream.length > 0) {
          assert.deepEqual(
            data.message.content,
            data.interrupted === true
              ? assembled.interruptedBlocks()
              : assembled.blocks(),
          );
          if (data.usage !== undefined)
            assert.deepEqual(data.usage, assembled.usage);
        }
      }
    }
  };
  const clone = () => structuredClone(artifact);
  const codec = format.releasedV4SessionFormatCodec;
  const encodeRows = (value) =>
    value.events.map((event) =>
      JSON.parse(JSON.stringify(codec.encodeEvent(event))),
    );
  const decodeRows = (value, rows = encodeRows(value), recovery = "strict") => {
    const decoder = codec.createDecoder(
      codec.encodeHeader(value.header, value.inheritedEventCount),
      recovery,
    );
    const events = [];
    const context = {
      emitEvent: (event) => events.push(event),
      emitRun: () => assert.fail("v4 packed top-level run"),
    };
    for (const row of rows) decoder.decodeRow(row, context);
    return {
      header: decoder.header,
      events,
      inheritedEventCount: decoder.finish(context),
    };
  };
  const retryArtifact = withRetry(artifact);
  const child = seedWithTurn(artifact, "s1");
  let concatenatedZstdRead;
  let tornZstdRead;
  const usageCases = JSON.parse(
    readFileSync(
      new URL("../tests/fixtures/dsh-v4/usage-cases.json", import.meta.url),
      "utf8",
    ),
  );

  check("native_complete_turn", () => validateFixture(clone()));
  check("physical_codec_roundtrip", () => {
    const decoded = decodeRows(artifact);
    assert.deepEqual(decoded, artifact);
    validateFixture(decoded);
  });
  check("stream_codec_roundtrip", () => {
    const expanded = llm.expandAssistantStream(assistant.data.stream);
    const accumulator = new llm.AssistantStreamAccumulator();
    expanded.forEach((timed) => accumulator.push(timed));
    assert.deepEqual(
      llm.expandAssistantStream(accumulator.snapshot()),
      expanded,
    );
    assert.deepEqual(
      assemble(assistant.data.stream).blocks(),
      assistant.data.message.content,
    );
    assert.deepEqual(assemble(assistant.data.stream).usage, usage);
  });
  check("one_usage_per_settlement", () => {
    const state = projection.apply(projection.init(), assistant);
    assert.equal(state.totals.uncachedInputTokens, usage.inputTokens);
    assert.equal(state.totals.outputTokens, usage.outputTokens);
    assert.equal(
      meter.deriveTurnTokenUsage(artifact.events).totalTokens,
      usage.totalTokens,
    );
  });
  check("message_usage_precedes_stream_for_reducer", () => {
    const changed = structuredClone(assistant);
    changed.data.usage = { ...usage, inputTokens: 11, totalTokens: 21 };
    assert.equal(
      projection.apply(projection.init(), changed).totals.uncachedInputTokens,
      11,
    );
    delete changed.data.usage;
    assert.equal(
      projection.apply(projection.init(), changed).totals.uncachedInputTokens,
      usage.inputTokens,
    );
  });
  check("missing_usage_is_unavailable", () => {
    const changed = clone();
    const data = changed.events.find(
      (event) => event.type === "assistant/message",
    ).data;
    delete data.usage;
    data.stream = data.stream.filter(
      (record) => record.chunk?.type !== "usage",
    );
    validateFixture(changed);
    assert.equal(meter.deriveTurnTokenUsage(changed.events), undefined);
    assert.equal(
      meter.deriveTurnTokenUsage(artifact.events.slice(0, -1)),
      undefined,
    );
  });
  check("unknown_cache_does_not_become_zero", () => {
    const changed = clone();
    const data = changed.events.find(
      (event) => event.type === "assistant/message",
    ).data;
    const partial = { inputTokens: 10, outputTokens: 5, totalTokens: 20 };
    data.usage = partial;
    data.stream = data.stream.filter(
      (record) => record.chunk?.type !== "usage",
    );
    data.stream.splice(-1, 0, {
      type: "chunk",
      time: 16,
      chunk: { type: "usage", usage: partial },
    });
    validateFixture(changed);
    const exact = meter.deriveTurnTokenUsage(changed.events);
    assert.equal(exact.totalTokens, 20);
    assert(
      !Object.hasOwn(exact, "cacheReadTokens") &&
        !Object.hasOwn(exact, "cacheWriteTokens"),
    );
    assert.equal(exact.uncachedInputTokens + exact.outputTokens, 15);
  });
  check("native_complete_retry", () => {
    validateFixture(retryArtifact);
    const decoded = decodeRows(retryArtifact);
    assert.deepEqual(decoded, retryArtifact);
    validateFixture(decoded);
    const failed = decoded.events.find(
      (event) => event.type === "assistant/attempt",
    );
    const started = decoded.events.find(
      (event) => event.type === "llm/retry-started",
    );
    const failedState = projection.apply(projection.init(), failed);
    assert.equal(
      projection.apply(failedState, assistant).totals.uncachedInputTokens,
      10,
    );
    assert.equal(
      projection.apply(projection.apply(failedState, started), assistant).totals
        .uncachedInputTokens,
      17,
    );
    const total = meter.deriveTurnTokenUsage(decoded.events);
    assert.deepEqual(total, {
      uncachedInputTokens: 17,
      outputTokens: 10,
      cacheReadTokens: 4,
      cacheWriteTokens: 6,
      reasoningTokens: 2,
      totalTokens: 37,
    });
    // 失败 attempt 没有模型路由；不能替它补成成功消息的 route。
    assert(!Object.hasOwn(total, "routes"));
    assert.equal(
      meter.deriveTurnTokenUsage(
        decoded.events.filter((event) => event.type !== "llm/retry-started"),
      ),
      undefined,
    );
  });
  check("native_retry_relationship_rejections", () => {
    for (const [type, field, value, reason] of [
      ["llm/retry", "provider", "other", /provider does not match/],
      ["llm/retry", "retry", 2, /policy attempt sequence/],
      ["llm/retry-started", "retryId", "other", /no prior scheduled attempt/],
      ["llm/retry-started", "step", 2, /changes scheduled coordinates/],
    ]) {
      const changed = structuredClone(retryArtifact);
      changed.events.find((event) => event.type === type).data[field] = value;
      assert.throws(() => nativeRestore(changed), reason);
    }
  });
  check("seed_prefix_and_own_usage", () => {
    validateFixture(child);
    assert.deepEqual(decodeRows(child), child);
    assert.deepEqual(nativeRestore(child).snapshotEvents(), child.events);
    const cut = child.inheritedEventCount;
    assert.equal(cut, artifact.events.length);
    assert.deepEqual(child.events.slice(0, cut), artifact.events);
    assert.equal(child.events[cut].type, "session/end-seed");
    assert.equal(child.events[cut].data.inherited, true);
    assert.equal(
      meter.deriveTurnTokenUsage(child.events.slice(0, cut)).totalTokens,
      20,
    );
    assert.equal(
      meter.deriveTurnTokenUsage(child.events.slice(cut + 1)).totalTokens,
      5,
    );
    assert.equal(meter.deriveTurnTokenUsage(child.events), undefined);
  });
  check("seed_last_marker_and_parent_only", () => {
    const grandchild = seedWithTurn(child, "s2");
    validateFixture(grandchild);
    assert.deepEqual(decodeRows(grandchild), grandchild);
    assert.deepEqual(
      grandchild.events.slice(0, grandchild.inheritedEventCount),
      child.events,
    );
    assert.equal(
      grandchild.events.filter(
        (event) =>
          event.type === "session/end-seed" && event.data.inherited === true,
      ).length,
      2,
    );
    const wrongCut = structuredClone(grandchild);
    wrongCut.inheritedEventCount = child.inheritedEventCount;
    assert.throws(
      () => nativeRestore(wrongCut),
      /last inherited end-seed marker/,
    );
    const unseeded = structuredClone(child);
    unseeded.header.isSeeded = false;
    unseeded.inheritedEventCount = 0;
    assert.throws(() => nativeRestore(unseeded), /unseeded Session contains/);
    const noMarker = structuredClone(child);
    noMarker.events[child.inheritedEventCount].data = {};
    assert.throws(
      () => nativeRestore(noMarker),
      /last inherited end-seed marker/,
    );
    const parentOnly = clone();
    parentOnly.header.parentSession = "parent";
    validateFixture(parentOnly);
    assert.deepEqual(decodeRows(parentOnly), parentOnly);
    assert.equal(parentOnly.inheritedEventCount, 0);
  });
  check("codec_recoverable_suffix_is_incomplete", () => {
    const rows = encodeRows(artifact);
    rows.at(-1).time = "invalid";
    assert.throws(() => decodeRows(artifact, rows));
    const prefix = decodeRows(artifact, rows, "recoverable");
    assert.equal(prefix.events.length, rows.length - 1);
    assert.deepEqual(prefix.events, artifact.events.slice(0, -1));
    validateFixture(prefix);
    assert.equal(meter.deriveTurnTokenUsage(prefix.events), undefined);
    const afterClosedTurn = [
      ...encodeRows(artifact),
      {
        type: "fixture/unknown",
        seq: artifact.events.length,
        time: "invalid",
        data: {},
      },
      {
        type: "fixture/unknown",
        seq: artifact.events.length + 1,
        time: 26,
        data: {},
        ignorable: true,
      },
    ];
    const recovered = decodeRows(artifact, afterClosedTurn, "recoverable");
    assert.deepEqual(recovered, artifact);
    assert(recovered.events.length < afterClosedTurn.length);
    assert.equal(meter.deriveTurnTokenUsage(recovered.events).totalTokens, 20);
  });
  check("codec_corruption_before_turn_end_is_rejected", () => {
    const malformed = encodeRows(artifact);
    malformed.at(-2).time = "invalid";
    assert.throws(() => decodeRows(artifact, malformed, "recoverable"));
    const gap = encodeRows(artifact);
    gap[1].seq += 1;
    assert.throws(() => decodeRows(artifact, gap, "recoverable"));
    const retired = [
      ...encodeRows(artifact),
      {
        type: "request/header",
        seq: artifact.events.length,
        time: 25,
        data: {
          header: {
            config: { provider: "p0", model: "m0" },
            system: "retired",
          },
        },
      },
    ];
    assert.throws(
      () => decodeRows(artifact, retired, "recoverable"),
      /header.system/,
    );
  });
  check("codec_needs_semantic_admission", () => {
    const rows = [
      ...encodeRows(artifact),
      {
        type: "fixture/unknown",
        seq: artifact.events.length,
        time: 25,
        data: {},
      },
    ];
    const required = decodeRows(artifact, rows);
    assert.equal(required.events.length, rows.length);
    assert.throws(() => nativeRestore(required), /unknown event type/);
    rows.at(-1).ignorable = true;
    validateFixture(decodeRows(artifact, rows));
    const brokenRelation = clone();
    brokenRelation.events.find(
      (event) => event.type === "tool/call",
    ).data.arguments = "{}";
    const decoded = decodeRows(brokenRelation);
    assert.deepEqual(decoded, brokenRelation);
    assert.throws(
      () => nativeRestore(decoded),
      /does not match one advertised tool call/,
    );
  });
  check("zstd_primitives_multiframe_checksum_and_torn_tail", () => {
    // 仅验证 Node zstd 原语；没有调用 DSH backend 的恢复或修尾入口。
    const chunks = [
      codec.encodeHeader(artifact.header, 0),
      ...encodeRows(artifact),
    ].map((value) => Buffer.from(`${JSON.stringify(value)}\n`));
    const frames = chunks.map((chunk) =>
      zstdCompressSync(chunk, {
        params: { [constants.ZSTD_c_checksumFlag]: 1 },
      }),
    );
    const compressed = Buffer.concat(frames);
    const expected = Buffer.concat(chunks);
    // 已知帧边界来自本夹具的压缩调用，不是对任意文件实现帧扫描器。
    assert.deepEqual(
      Buffer.concat(frames.map((frame) => zstdDecompressSync(frame))),
      expected,
    );
    const decompressed = zstdDecompressSync(compressed);
    concatenatedZstdRead = decompressed.equals(expected)
      ? "all_frames"
      : "first_frame_only";
    assert.deepEqual(
      decompressed,
      concatenatedZstdRead === "all_frames" ? expected : chunks[0],
    );
    const corrupt = Buffer.from(frames.at(-1));
    corrupt[corrupt.length - 1] ^= 1;
    stage = "zstd_primitives:corrupt_checksum";
    assert.throws(() => zstdDecompressSync(corrupt));
    const lastFrame = frames.at(-1);
    assert((lastFrame[4] & 4) !== 0); // 本夹具确实开启 frame checksum。
    const torn = lastFrame.subarray(0, -1);
    stage = "zstd_primitives:torn_tail";
    let tornPayload;
    try {
      tornPayload = zstdDecompressSync(torn);
    } catch (error) {
      assert.equal(typeof error.code, "string");
    }
    if (tornPayload === undefined) tornZstdRead = "rejected";
    else {
      assert.deepEqual(tornPayload, chunks.at(-1));
      tornZstdRead = "full_payload_without_complete_checksum";
    }
    stage = "zstd_primitives:prefix";
    const prefix = zstdDecompressSync(torn, {
      finishFlush: constants.ZSTD_e_flush,
    });
    assert.deepEqual(prefix, chunks.at(-1));
    stage = "zstd_primitives:corrupt_prefix";
    assert.throws(() =>
      zstdDecompressSync(corrupt, { finishFlush: constants.ZSTD_e_flush }),
    );
  });
  check("usage_vectors_preserve_unknown_and_reject_invalid", () => {
    assert.equal(
      new Set(usageCases.map((item) => item.name)).size,
      usageCases.length,
    );
    for (const item of usageCases) {
      stage = `usage_vectors:${item.name}`;
      const value = clone();
      const data = value.events.find(
        (event) => event.type === "assistant/message",
      ).data;
      data.stream = data.stream.filter(
        (record) => record.chunk?.type !== "usage",
      );
      delete data.usage;
      if (item.usage !== undefined) {
        data.usage = structuredClone(item.usage);
        data.stream.splice(-1, 0, {
          type: "chunk",
          time: 16,
          chunk: { type: "usage", usage: structuredClone(item.usage) },
        });
      }
      const exact = meter.deriveTurnTokenUsage(value.events);
      if (item.expected === null) assert.equal(exact, undefined, item.name);
      else
        assert.deepEqual(
          exact,
          { ...item.expected, routes: [{ provider: "p0", model: "m0" }] },
          item.name,
        );
      assert.deepEqual(
        item.usage === undefined ? undefined : data.usage,
        item.usage,
      );
    }
  });
  check("negative_dt_and_first_close", () => {
    const timed = llm.expandAssistantStream([
      { type: "text-chunks", time0: 10, index: 5, dt: [-3], texts: ["a", "b"] },
    ]);
    assert.deepEqual(
      timed.map((item) => item.time),
      [10, 7],
    );
    const state = new llmEntry.BlockAssembler();
    for (const chunk of [
      { type: "text-delta", index: 5, text: "partial" },
      { type: "reasoning-delta", index: 2, text: "plan" },
      { type: "block-end", index: 5, block: { type: "text", text: "final" } },
      { type: "text-delta", index: 5, text: "ignored" },
      { type: "block-end", index: 5, block: { type: "text", text: "ignored" } },
    ])
      state.push(chunk);
    assert.deepEqual(state.blocks(), [
      { type: "text", text: "final" },
      { type: "reasoning", text: "plan" },
    ]);
  });
  check("interruption_and_max_tokens_drop_tool_calls", () => {
    const state = assemble(assistant.data.stream);
    assert(
      state.interruptedBlocks().every((block) => block.type !== "tool-call"),
    );
    state.push({ type: "finish", reason: { kind: "max-tokens" } });
    assert(state.blocks().every((block) => block.type !== "tool-call"));
  });
  check("native_relationship_rejections", () => {
    const gap = clone();
    gap.events[1].seq = 20;
    assert.throws(() => nativeRestore(gap));
    const call = clone();
    call.events.find(
      (event) => event.type === "tool/result",
    ).data.message.toolCallId = "other";
    assert.throws(() => nativeRestore(call));
    const unknown = clone();
    unknown.events.push({
      type: "fixture/unknown",
      seq: unknown.events.length,
      time: 25,
      data: {},
    });
    assert.throws(() => nativeRestore(unknown));
    unknown.events.at(-1).ignorable = true;
    nativeRestore(unknown);
  });
  check("native_validation_gaps_are_explicit", () => {
    const malformed = clone();
    malformed.events.find(
      (event) => event.type === "assistant/message",
    ).data.message.content[1].text = 123;
    nativeRestore(malformed);
    assert.throws(() => validateFixture(malformed));
    const unknownRaw = [
      { type: "chunk", time: 0, chunk: { type: "fixture/unknown" } },
    ];
    assert.equal(llm.expandAssistantStream(unknownRaw).length, 1);
    assert.throws(() => assemble(unknownRaw));
    const negative = clone();
    negative.events.find(
      (event) => event.type === "assistant/message",
    ).data.usage.inputTokens = -1;
    nativeRestore(negative);
    assert.equal(meter.deriveTurnTokenUsage(negative.events), undefined);
    assert.throws(() => validateFixture(negative));
  });
  console.log(
    JSON.stringify(
      {
        version,
        scope: "synthetic_fixture_and_offline_folds",
        fixture: "complete-turn",
        events: artifact.events.length,
        retry_events: retryArtifact.events.length,
        seeded_events: child.events.length,
        usage_case_count: usageCases.length,
        backend_recovery_validated: false,
        node_version: process.version,
        synchronous_zstd_concat_read: concatenatedZstdRead,
        synchronous_zstd_torn_read: tornZstdRead,
        message_roles: artifact.events
          .filter((event) => event.surfaceOp === "append")
          .map((event) =>
            event.type === "user/message"
              ? event.data.role
              : event.data.message.role,
          ),
        checks: counts,
        check_count: Object.keys(counts).length,
        original_logs_read: false,
        host_started: false,
        store_written: false,
      },
      null,
      2,
    ),
  );
}

main().catch((error) => {
  const kind =
    error instanceof assert.AssertionError
      ? "assertion"
      : "module_or_fixture_error";
  console.error(JSON.stringify({ status: "verification_failed", stage, kind }));
  process.exitCode = 2;
});

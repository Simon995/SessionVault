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
import {
  loadDshInstallation,
  SUPPORTED_DSH_VERSION as VERSION,
} from "./dsh-installed.mjs";
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
  const { require: req, load } = loadDshInstallation(values["dsh-package"]);
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

  check("native_complete_turn", () => validateFixture(clone()));
  check("physical_codec_roundtrip", () => {
    const codec = format.releasedV4SessionFormatCodec;
    const physicalHeader = codec.encodeHeader(
      artifact.header,
      artifact.inheritedEventCount,
    );
    const decoder = codec.createDecoder(physicalHeader, "strict");
    const events = [];
    const context = {
      emitEvent: (event) => events.push(event),
      emitRun: () => assert.fail("v4 packed top-level run"),
    };
    for (const event of artifact.events)
      decoder.decodeRow(
        JSON.parse(JSON.stringify(codec.encodeEvent(event))),
        context,
      );
    decoder.finish(context);
    assert.deepEqual(decoder.header, artifact.header);
    assert.deepEqual(events, artifact.events);
    validateFixture({ ...artifact, events });
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
  check("retry_usage_fold_only", () => {
    // reducer 单元样本，只给 fold 读的字段；不称为合法 v4 日志。
    const failedUsage = {
      inputTokens: 7,
      outputTokens: 5,
      cacheReadTokens: 2,
      cacheWriteTokens: 3,
      totalTokens: 17,
      reasoningTokens: 0,
    };
    const failed = {
      type: "assistant/attempt",
      data: {
        turn: 1,
        step: 1,
        stream: [
          {
            type: "chunk",
            time: 1,
            chunk: { type: "usage", usage: failedUsage },
          },
        ],
      },
    };
    const retry = { type: "llm/retry", data: { turn: 1, step: 1 } };
    const started = { type: "llm/retry-started", data: { turn: 1, step: 1 } };
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
    const events = [
      artifact.events[0],
      artifact.events[1],
      failed,
      retry,
      started,
      assistant,
      ...artifact.events.slice(-2),
    ];
    const total = meter.deriveTurnTokenUsage(events);
    assert.equal(total.uncachedInputTokens, 17);
    assert.equal(total.outputTokens, 10);
    assert.equal(total.cacheReadTokens, 4);
    assert.equal(total.cacheWriteTokens, 6);
    assert.equal(total.reasoningTokens, 2);
    assert.equal(total.totalTokens, 37);
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
        version: VERSION,
        scope: "synthetic_fixture_and_offline_folds",
        fixture: "complete-turn",
        events: artifact.events.length,
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

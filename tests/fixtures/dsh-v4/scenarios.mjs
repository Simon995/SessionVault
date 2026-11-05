import assert from "node:assert/strict";

// 从共享逻辑夹具派生；不读取源日志、不模拟 Host 执行。
export function withRetry(base) {
  const value = structuredClone(base);
  const index = value.events.findIndex(
    (event) => event.type === "assistant/message",
  );
  assert(index >= 0);
  const failure = { message: "Synthetic transport failure", code: "NETWORK" };
  const failedUsage = {
    inputTokens: 7,
    outputTokens: 5,
    cacheReadTokens: 2,
    cacheWriteTokens: 3,
    totalTokens: 17,
    reasoningTokens: 0,
  };
  value.events.splice(
    index,
    0,
    {
      type: "request/header",
      time: 5,
      data: {
        header: { config: { provider: "p0", model: "m0" } },
        reason: "initial",
      },
    },
    {
      type: "assistant/attempt",
      time: 8,
      data: {
        turn: 1,
        step: 1,
        stream: [
          {
            type: "chunk",
            time: 6,
            chunk: { type: "usage", usage: failedUsage },
          },
          {
            type: "chunk",
            time: 7,
            chunk: { type: "finish", reason: { kind: "error", failure } },
          },
        ],
      },
    },
    {
      type: "llm/retry",
      time: 9,
      data: {
        retryId: "retry-0",
        turn: 1,
        step: 1,
        provider: "p0",
        mode: "normal",
        policyKey: "fixture-network",
        retry: 1,
        maxRetries: 2,
        delayMs: 0,
        failure,
      },
    },
    {
      type: "llm/retry-started",
      time: 10,
      data: { retryId: "retry-0", turn: 1, step: 1, retry: 1 },
    },
  );
  value.events.forEach((event, seq) => {
    event.seq = seq;
  });
  const call = value.events.find((event) => event.type === "tool/call");
  const result = value.events.find((event) => event.type === "tool/result");
  assert(call && result);
  result.sourceEventSeqs = [call.seq];
  return value;
}

export function seedWithTurn(base, id) {
  const cut = base.events.length;
  const time = base.events.at(-1).time + 1;
  const turn =
    base.events.filter((event) => event.type === "turn/start").length + 1;
  const usage = {
    inputTokens: 3,
    outputTokens: 2,
    cacheReadTokens: 0,
    cacheWriteTokens: 0,
    totalTokens: 5,
    reasoningTokens: 0,
  };
  const suffix = [
    { type: "session/end-seed", data: { inherited: true } },
    { type: "turn/start", data: { turn } },
    { type: "step/start", data: { turn, step: 1 } },
    {
      type: "user/message",
      surfaceOp: "append",
      data: {
        id: `${id}-user`,
        role: "user",
        source: { kind: "user" },
        content: [{ type: "text", text: "Continue the fixture." }],
      },
    },
    {
      type: "assistant/message",
      surfaceOp: "append",
      data: {
        turn,
        step: 1,
        message: {
          id: `${id}-assistant`,
          role: "assistant",
          source: { kind: "model", provider: "p0", model: "m0" },
          content: [{ type: "text", text: "child" }],
        },
        stream: [
          {
            type: "text-chunks",
            time0: time + 3,
            index: 0,
            dt: [],
            texts: ["child"],
          },
          { type: "chunk", time: time + 4, chunk: { type: "usage", usage } },
          {
            type: "chunk",
            time: time + 4,
            chunk: { type: "finish", reason: { kind: "stop" } },
          },
        ],
        usage,
      },
    },
    { type: "step/end", data: { turn, step: 1 } },
    { type: "turn/end", data: { turn, reason: { kind: "completed" } } },
  ];
  return {
    header: {
      ...structuredClone(base.header),
      id,
      parentSession: base.header.id,
      isSeeded: true,
      createdAt: time,
    },
    inheritedEventCount: cut,
    events: [
      ...structuredClone(base.events),
      ...suffix.map((event, index) => ({
        ...event,
        seq: cut + index,
        time: time + index,
      })),
    ],
  };
}

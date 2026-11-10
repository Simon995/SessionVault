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

/**
 * 在基础 v4 会话之上追加 subagent/catalog、compaction 事务（含 surfaceOp: "replace" 检查点）
 * 以及非人类 source.kind（subagent-settled、goal、team-message）的 user/message。
 */
export function withCompactionAndSubagentCatalog(base, id = base.header.id) {
  const startSeq = base.events.length;
  const baseTime = base.events.at(-1).time + 10;
  const turn =
    base.events.filter((event) => event.type === "turn/start").length + 1;
  const usage = {
    inputTokens: 11,
    outputTokens: 6,
    cacheReadTokens: 3,
    cacheWriteTokens: 1,
    totalTokens: 21,
    reasoningTokens: 0,
  };
  const suffix = [
    {
      type: "subagent/catalog",
      data: {
        version: 1,
        childId: `${id}-sub-1`,
        childCreatedAt: baseTime,
        mode: "continuable",
        label: "audit",
      },
    },
    {
      type: "compaction/start",
      data: { compactionId: "cmp-0", turn: null },
    },
    {
      type: "compaction/summary",
      data: {
        compactionId: "cmp-0",
        summary: [{ type: "text", text: "Compacted summary." }],
        shadowedRange: { start: 3, end: 7 },
        shadowedSeqs: [3, 4, 5, 7],
        shadowedTokenCount: 42,
        provider: "p0",
        model: "m0",
      },
    },
    {
      type: "user/message",
      surfaceOp: { op: "replace", startSeq: 3, endSeq: 7 },
      sourceEventSeqs: [3, 4, 5, 7],
      data: {
        id: `${id}-compact-cp`,
        role: "user",
        source: { kind: "compact-checkpoint", compactionId: "cmp-0" },
        content: [{ type: "text", text: "Compacted checkpoint summary." }],
      },
    },
    {
      type: "compaction/end",
      data: { compactionId: "cmp-0", turn: null },
    },
    { type: "turn/start", data: { turn } },
    { type: "step/start", data: { turn, step: 1 } },
    {
      type: "user/message",
      surfaceOp: "append",
      data: {
        id: `${id}-settled`,
        role: "user",
        source: {
          kind: "subagent-settled",
          form: "notice",
          summary: "Subagent finished audit.",
          senderSessionId: `${id}-sub-1`,
        },
        content: [{ type: "text", text: "Subagent finished audit." }],
      },
    },
    {
      type: "user/message",
      surfaceOp: "append",
      data: {
        id: `${id}-goal`,
        role: "user",
        source: { kind: "goal", goalId: "goal-0", revision: 1, round: 1 },
        content: [{ type: "text", text: "Proceed with next step." }],
      },
    },
    {
      type: "user/message",
      surfaceOp: "append",
      data: {
        id: `${id}-team`,
        role: "user",
        source: {
          kind: "team-message",
          teamId: "team-0",
          messageId: "msg-0",
          senderId: "peer-alpha",
          senderName: "Peer Alpha",
        },
        content: [{ type: "text", text: "Peer status update." }],
      },
    },
    {
      type: "assistant/message",
      surfaceOp: "append",
      data: {
        turn,
        step: 1,
        message: {
          id: `${id}-post-compact-assistant`,
          role: "assistant",
          source: { kind: "model", provider: "p0", model: "m0" },
          content: [{ type: "text", text: "Post-compaction response." }],
        },
        stream: [
          {
            type: "text-chunks",
            time0: baseTime + 11,
            index: 0,
            dt: [],
            texts: ["Post-compaction response."],
          },
          { type: "chunk", time: baseTime + 12, chunk: { type: "usage", usage } },
          {
            type: "chunk",
            time: baseTime + 12,
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
    },
    inheritedEventCount: base.inheritedEventCount ?? 0,
    events: [
      ...structuredClone(base.events),
      ...suffix.map((event, idx) => ({
        ...event,
        seq: startSeq + idx,
        time: baseTime + idx,
      })),
    ],
  };
}

/**
 * 构造无 seed 的独立子代理 v4 会话（`isSeeded: false, origin: "subagent", parentSession`），
 * 含 `subagent/descriptor` 与初始委托 `user/message`。
 */
export function unseededSubagentChild(baseHeader, parentId, id) {
  const usage = {
    inputTokens: 9,
    outputTokens: 4,
    cacheReadTokens: 1,
    cacheWriteTokens: 0,
    totalTokens: 14,
    reasoningTokens: 0,
  };
  const rawEvents = [
    {
      type: "subagent/descriptor",
      data: {
        version: 3,
        provider: "p0",
        mode: "continuable",
        label: "audit",
      },
    },
    { type: "turn/start", data: { turn: 1 } },
    { type: "step/start", data: { turn: 1, step: 1 } },
    {
      type: "system/message",
      surfaceOp: "append",
      data: {
        turn: 1,
        step: 1,
        message: {
          id: `${id}-sys`,
          role: "system",
          source: { kind: "system-prompt" },
          content: [{ type: "text", text: "Subagent system prompt." }],
        },
      },
    },
    {
      type: "user/message",
      surfaceOp: "append",
      data: {
        id: `${id}-delegated-user`,
        role: "user",
        source: { kind: "user" },
        content: [{ type: "text", text: "Run subagent audit task." }],
      },
    },
    {
      type: "assistant/message",
      surfaceOp: "append",
      data: {
        turn: 1,
        step: 1,
        message: {
          id: `${id}-assistant`,
          role: "assistant",
          source: { kind: "model", provider: "p0", model: "m0" },
          content: [{ type: "text", text: "Audit complete." }],
        },
        stream: [
          {
            type: "text-chunks",
            time0: 205,
            index: 0,
            dt: [],
            texts: ["Audit complete."],
          },
          { type: "chunk", time: 206, chunk: { type: "usage", usage } },
          {
            type: "chunk",
            time: 206,
            chunk: { type: "finish", reason: { kind: "stop" } },
          },
        ],
        usage,
      },
    },
    { type: "step/end", data: { turn: 1, step: 1 } },
    { type: "turn/end", data: { turn: 1, reason: { kind: "completed" } } },
  ];

  return {
    header: {
      ...structuredClone(baseHeader),
      id,
      parentSession: parentId,
      isSeeded: false,
      origin: "subagent",
      delegationDepth: 1,
      agentPreset: "auditor",
      createdAt: 200,
    },
    inheritedEventCount: 0,
    events: rawEvents.map((ev, idx) => ({
      ...ev,
      seq: idx,
      time: 200 + idx,
    })),
  };
}

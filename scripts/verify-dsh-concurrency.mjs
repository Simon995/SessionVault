#!/usr/bin/env node
/**
 * 双进程 + 公开 Node I/O 屏障，只在本仓 target 的新目录核 DSH 并发读取。
 * 不改安装源码或私有状态；不启动 Host/LLM，不打开真实日志或总库。
 */
import assert from "node:assert/strict";
import { fork } from "node:child_process";
import fs from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { constants, zstdCompressSync } from "node:zlib";
import {
  loadDshInstallation,
  SUPPORTED_DSH_VERSION,
} from "./dsh-installed.mjs";
import { seedWithTurn } from "../tests/fixtures/dsh-v4/scenarios.mjs";

const DEADLINE_MS = 15_000;
const counts = {};
const observations = {};
let stage = "arguments";

function inside(root, path) {
  const rel = relative(root, path);
  assert(
    rel !== "" &&
      !isAbsolute(rel) &&
      rel !== ".." &&
      !rel.startsWith("../") &&
      !rel.startsWith("..\\"),
  );
}

async function bounded(promise) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(
          () => reject(new Error("fixture deadline exceeded")),
          DEADLINE_MS,
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function startPeer(run, packagePath) {
  const child = fork(
    new URL("../tests/fixtures/dsh-v4/persistence-peer.mjs", import.meta.url),
    ["--run", run, "--dsh-package", packagePath],
    { stdio: ["ignore", "ignore", "pipe", "ipc"] },
  );
  const pending = new Map();
  let next = 0;
  let ready;
  let readyFailed;
  const startup = new Promise((accept, reject) => {
    ready = accept;
    readyFailed = reject;
  });
  const exited = new Promise((accept) => {
    child.once("exit", (code, signal) => {
      const error = new Error(`fixture peer exited (${code ?? signal})`);
      readyFailed(error);
      for (const waiter of pending.values()) waiter.reject(error);
      pending.clear();
      accept({ code, signal });
    });
  });
  child.on("error", (error) => {
    readyFailed(error);
    for (const waiter of pending.values()) waiter.reject(error);
  });
  // 排空 stderr，错误类型由 IPC 或进程退出码报告，不输出安装路径或正文。
  child.stderr.resume();
  child.on("message", (message) => {
    if (message.ready) return ready(message.pid);
    const waiter = pending.get(message.id);
    if (!waiter) return;
    pending.delete(message.id);
    if (message.ok) waiter.resolve(message.result);
    else
      waiter.reject(
        Object.assign(
          new Error("fixture peer operation failed"),
          message.error,
        ),
      );
  });
  const call = async (op, data = {}) => {
    const requestId = next++;
    try {
      return await bounded(
        new Promise((accept, reject) => {
          pending.set(requestId, { resolve: accept, reject });
          child.send({ ...data, op, requestId }, (error) => {
            if (error) reject(error);
          });
        }),
      );
    } finally {
      pending.delete(requestId);
    }
  };
  try {
    const pid = await bounded(startup);
    assert.notEqual(pid, process.pid);
    return {
      call,
      async stop() {
        try {
          await call("stop");
          assert.equal((await bounded(exited)).code, 0);
        } finally {
          if (child.exitCode === null && child.signalCode === null) {
            child.kill();
            await bounded(exited);
          }
        }
      },
    };
  } catch (error) {
    child.kill();
    await bounded(exited);
    throw error;
  }
}

/** 在真实 I/O 返回后、backend 获得结果前让另一进程完成指定动作。 */
async function ioWindow(operation, path, onHit, action) {
  const original = fs[operation];
  const trace = [];
  fs[operation] = async (...args) => {
    const result = await original(...args);
    if (typeof args[0] === "string" && resolve(args[0]) === resolve(path)) {
      trace.push({ call: trace.length + 1, bytes: result?.byteLength });
      await onHit(trace.length);
    }
    return result;
  };
  syncBuiltinESMExports();
  try {
    const value = await bounded(Promise.resolve().then(action));
    assert(
      trace.length > 0,
      "the intended native I/O boundary was not reached",
    );
    return { value, trace };
  } finally {
    fs[operation] = original;
    syncBuiltinESMExports();
  }
}

async function main() {
  const { values } = parseArgs({
    options: { "dsh-package": { type: "string" } },
  });
  assert(values["dsh-package"]);
  const { load } = loadDshInstallation(values["dsh-package"]);
  const { Context } = await load("@deepseek-ai/cordis");
  const backend = await load("@deepseek-ai/dsh-session-persistence-jsonl");
  const errors = await load("@deepseek-ai/dsh-session-persistence");
  const {
    releasedV4SessionFormatCodec: codec,
    releasedV3SessionFormatCodec: codec3,
  } = await load("@deepseek-ai/dsh-session-format-v3-to-v4");
  const artifact = JSON.parse(
    await fs.readFile(
      new URL("../tests/fixtures/dsh-v4/complete-turn.json", import.meta.url),
      "utf8",
    ),
  );
  const repo = await fs.realpath(fileURLToPath(new URL("..", import.meta.url)));
  const target = join(repo, "target");
  await fs.mkdir(target).catch((error) => {
    if (error.code !== "EEXIST") throw error;
  });
  const targetStat = await fs.lstat(target);
  assert(targetStat.isDirectory() && !targetStat.isSymbolicLink());
  inside(repo, await fs.realpath(target));
  const run = await fs.mkdtemp(join(target, "dsh-concurrency-"));
  inside(target, await fs.realpath(run));
  const peer = await startPeer(run, values["dsh-package"]);
  const line = (value) => Buffer.from(`${JSON.stringify(value)}\n`);
  const pack = (bytes) =>
    zstdCompressSync(bytes, {
      params: { [constants.ZSTD_c_checksumFlag]: 1 },
    });
  const encode = (value, compression) =>
    Buffer.concat(
      [
        line(codec.encodeHeader(value.header, value.inheritedEventCount)),
        ...value.events.map((event) => line(codec.encodeEvent(event))),
      ].map((part) => (compression === "zstd" ? pack(part) : part)),
    );
  const extend = (value, id) => {
    const child = seedWithTurn(value, id);
    const own = child.events
      .slice(value.events.length + 1)
      .map((event, index) => ({
        ...event,
        seq: value.events.length + index,
      }));
    return { ...structuredClone(value), events: [...value.events, ...own] };
  };
  const batch = (before, after, compression) => {
    const plain = Buffer.concat(
      after.events
        .slice(before.events.length)
        .map((event) => line(codec.encodeEvent(event))),
    );
    return compression === "zstd" ? pack(plain) : plain;
  };
  const mutate = (op, path, bytes) =>
    peer.call(op, {
      path,
      bytes: bytes.toString("base64"),
    });
  const put = async (service, root, value, content, name) => {
    const current = service.locate(value.header).path;
    inside(root, current);
    const path = name ? join(dirname(current), name) : current;
    inside(root, path);
    await fs.mkdir(dirname(path), { recursive: true });
    await fs.writeFile(path, content, { flag: "wx" });
    return path;
  };
  const refresh = async (path, content) => {
    const before = await fs.stat(path, { bigint: true });
    await mutate("replace", path, content);
    const after = await fs.stat(path, { bigint: true });
    assert(before.ino !== after.ino || before.ctimeNs !== after.ctimeNs);
  };
  const check = async (name, compression, action) => {
    stage = name;
    const root = join(run, name);
    await fs.mkdir(root);
    const ctx = new Context();
    try {
      await ctx.plugin(backend, { root, compression }).await();
      await peer.call("init", { root, compression });
      await action(ctx.get("sessionPersistence"), root);
      counts[name] = "passed";
    } finally {
      try {
        await peer.call("close");
      } finally {
        await ctx.fiber.dispose();
      }
    }
  };

  try {
    for (const compression of ["none", "zstd"]) {
      await check(
        `${compression}_partial_append_during_read`,
        compression,
        async (service, root) => {
          const complete = encode(artifact, compression);
          const path = await put(service, root, artifact, complete);
          const handle = await service.open(artifact.header.id, "read");
          const grown = extend(artifact, "partial");
          const extra = batch(artifact, grown, compression);
          // 只提交下一条记录/帧的极短前缀，保证尚无完整新增事件。
          const cut = 2;
          try {
            await refresh(path, complete);
            const { value, trace } = await ioWindow(
              "readFile",
              path,
              async (hit) => {
                if (hit === 1)
                  await mutate("append", path, extra.subarray(0, cut));
              },
              () => handle.read(),
            );
            assert.equal(trace.length, 2);
            assert.deepEqual(value.events, artifact.events);
            assert.deepEqual(
              await fs.readFile(path),
              Buffer.concat([complete, extra.subarray(0, cut)]),
            );
            await mutate("append", path, extra.subarray(cut));
            assert.deepEqual((await handle.read()).events, grown.events);
            assert.deepEqual(
              (await handle.read(artifact.events.length)).events,
              grown.events.slice(artifact.events.length),
            );
            observations[stage] = {
              read_attempts: trace.length,
              incomplete_tail_hidden: true,
            };
          } finally {
            await handle.close();
          }
        },
      );

      await check(
        `${compression}_both_reads_overlap_append`,
        compression,
        async (service, root) => {
          const complete = encode(artifact, compression);
          const path = await put(service, root, artifact, complete);
          const handle = await service.open(artifact.header.id, "read");
          const first = extend(artifact, "first");
          const second = extend(first, "second");
          try {
            await refresh(path, complete);
            const { value, trace } = await ioWindow(
              "readFile",
              path,
              async (hit) => {
                if (hit === 1)
                  await mutate(
                    "append",
                    path,
                    batch(artifact, first, compression),
                  );
                if (hit === 2)
                  await mutate(
                    "append",
                    path,
                    batch(first, second, compression),
                  );
              },
              () => handle.read(),
            );
            assert.equal(trace.length, 2);
            assert.deepEqual(value.events, first.events);
            assert.deepEqual(
              (await handle.read(first.events.length)).events,
              second.events.slice(first.events.length),
            );
            assert.deepEqual((await handle.read()).events, second.events);
            observations[stage] = {
              read_attempts: trace.length,
              returned_events: value.events.length,
              settled_events: second.events.length,
              public_result_keys: Object.keys(value).sort(),
            };
          } finally {
            await handle.close();
          }
        },
      );

      await check(
        `${compression}_native_flush_visible_to_existing_reader`,
        compression,
        async (service, root) => {
          await put(service, root, artifact, encode(artifact, compression));
          const handle = await service.open(artifact.header.id, "read");
          try {
            assert.deepEqual((await handle.read()).events, artifact.events);
            await peer.call("open", {
              id: artifact.header.id,
              access: "write",
            });
            const grown = extend(artifact, "flushed");
            await peer.call("native-append", {
              events: grown.events.slice(artifact.events.length),
            });
            assert.deepEqual((await handle.read()).events, grown.events);
            assert.deepEqual(
              (await handle.read(artifact.events.length)).events,
              grown.events.slice(artifact.events.length),
            );
          } finally {
            await handle.close();
          }
        },
      );

      await check(
        `${compression}_corruption_published_during_read`,
        compression,
        async (service, root) => {
          const complete = encode(artifact, compression);
          const path = await put(service, root, artifact, complete);
          let corrupt;
          if (compression === "zstd") {
            corrupt = Buffer.from(complete);
            corrupt[corrupt.length - 1] ^= 1;
          } else {
            const row = line(codec.encodeEvent(artifact.events.at(-2)));
            const start = complete.indexOf(row);
            assert(start >= 0);
            corrupt = Buffer.concat([
              complete.subarray(0, start),
              Buffer.from("invalid JSON\n"),
              complete.subarray(start + row.length),
            ]);
          }
          const handle = await service.open(artifact.header.id, "read");
          let reads = 0;
          try {
            await refresh(path, complete);
            await assert.rejects(
              ioWindow(
                "readFile",
                path,
                async (hit) => {
                  reads = hit;
                  if (hit === 1) await mutate("replace", path, corrupt);
                },
                () => handle.read(),
              ),
              errors.SessionPersistenceCorruptionError,
            );
            assert.equal(reads, 2);
            await mutate("replace", path, complete);
            assert.deepEqual((await handle.read()).events, artifact.events);
            observations[stage] = {
              read_attempts: reads,
              result: "corruption_error",
              next_read_recovers: true,
            };
          } finally {
            await handle.close();
          }
        },
      );

      await check(
        `${compression}_injected_io_failure_is_not_empty`,
        compression,
        async (service, root) => {
          const complete = encode(artifact, compression);
          const path = await put(service, root, artifact, complete);
          const handle = await service.open(artifact.header.id, "read");
          let hits = 0;
          try {
            await refresh(path, complete);
            await assert.rejects(
              ioWindow(
                "readFile",
                path,
                async () => {
                  hits += 1;
                  throw Object.assign(new Error("synthetic read failure"), {
                    code: "EIO",
                  });
                },
                () => handle.read(),
              ),
              (error) => error.code === "EIO",
            );
            assert.equal(hits, 1);
            assert.deepEqual((await handle.read()).events, artifact.events);
            assert.deepEqual(await fs.readFile(path), complete);
            observations[stage] = {
              fault_injected: true,
              result: "io_error",
              empty_success: false,
            };
          } finally {
            await handle.close();
          }
        },
      );

      await check(
        `${compression}_same_event_count_rewrite_needs_revision_evidence`,
        compression,
        async (service, root) => {
          const complete = encode(artifact, compression);
          const path = await put(service, root, artifact, complete);
          const changed = structuredClone(artifact);
          changed.events.find(
            (event) => event.type === "user/message",
          ).data.content[0].text = "Changed fixture!!";
          const changedBytes = encode(changed, compression);
          if (compression === "none")
            assert.equal(changedBytes.length, complete.length);
          const handle = await service.open(artifact.header.id, "read");
          try {
            const previous = await handle.read();
            assert.deepEqual(previous.events, artifact.events);
            await refresh(path, complete);
            const { value, trace } = await ioWindow(
              "readFile",
              path,
              async (hit) => {
                if (hit === 1) await mutate("replace", path, changedBytes);
              },
              () => handle.read(),
            );
            assert.equal(trace.length, 2);
            assert.deepEqual(value.events, changed.events);
            assert.equal(value.events.length, previous.events.length);
            assert.notDeepEqual(value, previous);
            observations[stage] = {
              same_physical_size: changedBytes.length === complete.length,
              same_event_count: true,
              changed_content_accepted: true,
            };
          } finally {
            await handle.close();
          }
        },
      );

      await check(
        `${compression}_native_repair_truncation_window`,
        compression,
        async (service, root) => {
          const complete = encode(artifact, compression);
          const torn =
            compression === "zstd"
              ? complete.subarray(0, -1)
              : Buffer.concat([complete, Buffer.from('{"type":')]);
          const path = await put(service, root, artifact, torn);
          await peer.call("open", { id: artifact.header.id, access: "read" });
          const initial = await peer.call("read");
          assert.deepEqual(initial.events, artifact.events);
          const writer = await service.open(artifact.header.id, "write");
          const grown = extend(artifact, "repair");
          let during;
          try {
            const { trace } = await ioWindow(
              "truncate",
              path,
              async () => {
                try {
                  during = { result: await peer.call("read") };
                } catch (error) {
                  during = {
                    error: error.name,
                    prefix_shrank: error.prefix_shrank,
                  };
                }
              },
              async () => {
                await writer.append(grown.events.slice(artifact.events.length));
                await writer.flush();
              },
            );
            assert.equal(trace.length, 1);
            if (compression === "zstd")
              assert.equal(during.prefix_shrank, true);
            else assert.deepEqual(during.result.events, artifact.events);
            assert.deepEqual((await peer.call("read")).events, grown.events);
            observations[stage] = {
              truncations: trace.length,
              in_flight:
                compression === "zstd"
                  ? "previous_prefix_temporarily_unavailable"
                  : "same_logical_prefix",
              settled_events: grown.events.length,
            };
          } finally {
            await writer.close();
          }
        },
      );

      await check(
        `${compression}_future_generation_published_during_read`,
        compression,
        async (service, root) => {
          const complete = encode(artifact, compression);
          const path = await put(service, root, artifact, complete);
          const future = join(
            dirname(path),
            compression === "zstd"
              ? "session.v5.jsonl.zstd"
              : "session.v5.jsonl",
          );
          const header = line({
            ...codec.encodeHeader(artifact.header, 0),
            version: 5,
          });
          const handle = await service.open(artifact.header.id, "read");
          try {
            await refresh(path, complete);
            const { value, trace } = await ioWindow(
              "readFile",
              path,
              async (hit) => {
                assert.equal(hit, 1);
                await mutate(
                  "publish",
                  future,
                  compression === "zstd" ? pack(header) : header,
                );
              },
              () => handle.read(),
            );
            assert.deepEqual(value.events, artifact.events);
            await assert.rejects(
              handle.read(),
              errors.SessionFormatUnsupportedError,
            );
            await assert.rejects(
              service.open(artifact.header.id, "read"),
              errors.SessionFormatUnsupportedError,
            );
            observations[stage] = {
              read_attempts: trace.length,
              in_flight: "selected_old_prefix",
              next_read: "unsupported",
            };
          } finally {
            await handle.close();
          }
        },
      );

      await check(
        `${compression}_cancellation_during_read`,
        compression,
        async (service, root) => {
          const complete = encode(artifact, compression);
          const path = await put(service, root, artifact, complete);
          const handle = await service.open(artifact.header.id, "read");
          const controller = new AbortController();
          const grown = extend(artifact, "cancel");
          try {
            await refresh(path, complete);
            await assert.rejects(
              ioWindow(
                "readFile",
                path,
                async () => {
                  await mutate(
                    "append",
                    path,
                    batch(artifact, grown, compression),
                  );
                  controller.abort(
                    new DOMException("fixture cancellation", "AbortError"),
                  );
                },
                () =>
                  handle.read(0, Number.MAX_SAFE_INTEGER, {
                    signal: controller.signal,
                  }),
              ),
              (error) => error.name === "AbortError",
            );
            assert.deepEqual((await handle.read()).events, grown.events);
          } finally {
            await handle.close();
          }
        },
      );

      await check(
        `${compression}_recovery_is_not_in_public_read_state`,
        compression,
        async (service, root) => {
          const complete = encode(artifact, compression);
          const path = await put(service, root, artifact, complete);
          const handle = await service.open(artifact.header.id, "read");
          try {
            const before = await handle.read();
            const torn =
              compression === "zstd"
                ? complete.subarray(0, -1)
                : Buffer.concat([complete, Buffer.from('{"type":')]);
            await mutate("replace", path, torn);
            const recovered = await handle.read();
            assert.deepEqual(recovered, before);
            assert.deepEqual(await fs.readFile(path), torn);
            assert.deepEqual(Object.keys(recovered).sort(), [
              "eventState",
              "events",
            ]);
            observations[stage] = {
              identical_public_result: true,
              different_physical_bytes: true,
            };
          } finally {
            await handle.close();
          }
        },
      );
    }

    await check(
      "historical_publication_during_open",
      "none",
      async (service, root) => {
        const value = structuredClone(artifact);
        value.events = value.events.filter((event) =>
          [
            "turn/start",
            "step/start",
            "user/message",
            "assistant/message",
            "step/end",
            "turn/end",
          ].includes(event.type),
        );
        value.events.forEach((event, seq) => {
          event.seq = seq;
        });
        const data = value.events.find(
          (event) => event.type === "assistant/message",
        ).data;
        data.message.content = [{ type: "text", text: "ok" }];
        data.stream = [
          { type: "text-chunks", time0: 10, index: 0, dt: [], texts: ["ok"] },
          {
            type: "chunk",
            time: 16,
            chunk: { type: "usage", usage: data.usage },
          },
          {
            type: "chunk",
            time: 19,
            chunk: { type: "finish", reason: { kind: "stop" } },
          },
        ];
        const old = Buffer.concat([
          line(codec3.encodeHeader({ ...value.header, version: 3 }, 0)),
          ...value.events.map((event) => line(codec3.encodeEvent(event))),
        ]);
        const path = await put(service, root, value, old, "session.v3.jsonl");
        const grown = extend(value, "published");
        const current = service.locate(value.header).path;
        const { value: handle, trace } = await ioWindow(
          "readFile",
          path,
          async (hit) => {
            assert.equal(hit, 1);
            await mutate("publish", current, encode(grown, "none"));
          },
          () => service.open(value.header.id, "read"),
        );
        try {
          assert.deepEqual((await handle.read()).events, grown.events);
          assert.deepEqual(await fs.readFile(path), old);
          assert.equal(
            (await service.stat(value.header.id)).formatStatus,
            "current",
          );
          observations[stage] = {
            old_read_attempts: trace.length,
            settled_events: grown.events.length,
          };
        } finally {
          await handle.close();
        }
      },
    );
  } finally {
    await peer.stop();
  }

  console.log(
    JSON.stringify(
      {
        version: SUPPORTED_DSH_VERSION,
        node_version: process.version,
        scope: "native_backend_controlled_cross_process_windows",
        run_directory: relative(repo, run).replaceAll("\\", "/"),
        checks: counts,
        check_count: Object.keys(counts).length,
        observations,
        scheduling: "public_fs_completion_barriers_with_ipc_acknowledgements",
        original_logs_read: false,
        real_session_logs_written: false,
        synthetic_logs_written: true,
        host_started: false,
        total_store_written: false,
        controlled_concurrent_windows_validated: true,
        public_physical_completeness_available: false,
        general_stress_validated: false,
        crash_durability_validated: false,
      },
      null,
      2,
    ),
  );
}

main().catch((error) => {
  console.error(
    JSON.stringify({
      status: "verification_failed",
      stage,
      kind:
        error instanceof assert.AssertionError
          ? "assertion"
          : (error?.name ?? "fixture_error"),
      assertion:
        error instanceof assert.AssertionError ? error.operator : undefined,
    }),
  );
  process.exitCode = 2;
});

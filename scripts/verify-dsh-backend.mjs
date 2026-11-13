#!/usr/bin/env node
/**
 * 在本仓 target 的独立目录运行原生 JSONL backend，仅对合成日志做读写与恢复。
 * node scripts/verify-dsh-backend.mjs --dsh-package <package.json>
 * 不加载 profile、Host 或 LLM；保留运行目录，不递归删除。
 */
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  appendFile,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  realpath,
  rename,
  utimes,
  writeFile,
} from "node:fs/promises";
import { dirname, isAbsolute, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { constants, zstdCompressSync } from "node:zlib";
import { loadDshInstallation } from "./dsh-installed.mjs";
import {
  seedWithTurn,
  withRetry,
} from "../tests/fixtures/dsh-v4/scenarios.mjs";

const counts = {};
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

async function snapshot(root) {
  const entries = [];
  async function walk(path) {
    const stat = await lstat(path, { bigint: true });
    assert(!stat.isSymbolicLink());
    const entry = {
      path: relative(root, path),
      mtime: String(stat.mtimeNs),
      ctime: String(stat.ctimeNs),
    };
    if (stat.isDirectory()) {
      entries.push({ ...entry, kind: "directory" });
      for (const name of (await readdir(path)).sort())
        await walk(join(path, name));
    } else {
      assert(stat.isFile());
      entries.push({
        ...entry,
        kind: "file",
        size: String(stat.size),
        sha256: createHash("sha256")
          .update(await readFile(path))
          .digest("hex"),
      });
    }
  }
  await walk(root);
  return entries;
}

async function main() {
  const { values } = parseArgs({
    options: { "dsh-package": { type: "string" } },
  });
  assert(values["dsh-package"]);
  const { load, version } = loadDshInstallation(values["dsh-package"]);
  const { Context } = await load("@deepseek-ai/cordis");
  const backend = await load("@deepseek-ai/dsh-session-persistence-jsonl");
  const errors = await load("@deepseek-ai/dsh-session-persistence");
  const {
    releasedV4SessionFormatCodec: codec,
    releasedV3SessionFormatCodec: codec3,
  } = await load("@deepseek-ai/dsh-session-format-v3-to-v4");
  const artifact = JSON.parse(
    await readFile(
      new URL("../tests/fixtures/dsh-v4/complete-turn.json", import.meta.url),
      "utf8",
    ),
  );
  const repo = await realpath(fileURLToPath(new URL("..", import.meta.url)));
  const target = join(repo, "target");
  await mkdir(target).catch((error) => {
    if (error.code !== "EEXIST") throw error;
  });
  const targetStat = await lstat(target);
  assert(targetStat.isDirectory() && !targetStat.isSymbolicLink());
  inside(repo, await realpath(target));
  const run = await mkdtemp(join(target, "dsh-backend-"));
  inside(target, await realpath(run));

  const line = (value) => Buffer.from(`${JSON.stringify(value)}\n`);
  const pack = (bytes) =>
    zstdCompressSync(bytes, { params: { [constants.ZSTD_c_checksumFlag]: 1 } });
  const rows = (value) => [
    codec.encodeHeader(value.header, value.inheritedEventCount),
    ...value.events.map((event) => codec.encodeEvent(event)),
  ];
  const chunks = (value) => rows(value).map(line);
  const bytes = (value, compression) =>
    Buffer.concat(
      chunks(value).map((chunk) =>
        compression === "zstd" ? pack(chunk) : chunk,
      ),
    );
  const nextTurn = (id) => {
    const child = seedWithTurn(artifact, id);
    return child.events
      .slice(child.inheritedEventCount + 1)
      .map((event, index) => ({
        ...event,
        seq: artifact.events.length + index,
      }));
  };
  const put = async (service, root, value, content, name) => {
    const current = service.locate(value.header).path;
    inside(root, current);
    const path = name === undefined ? current : join(dirname(current), name);
    inside(root, path);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, content, { flag: "wx" });
    return path;
  };
  const unchanged = async (root, action) => {
    const before = await snapshot(root);
    try {
      return await action();
    } finally {
      assert.deepEqual(await snapshot(root), before);
    }
  };
  const readAll = async (service, id) => {
    const handle = await service.open(id, "read");
    try {
      return {
        header: handle.header,
        inheritedEventCount: handle.inheritedEventCount,
        ...(await handle.read()),
      };
    } finally {
      await handle.close();
    }
  };
  const check = async (name, compression, action) => {
    stage = name;
    const root = join(run, name);
    await mkdir(root);
    const ctx = new Context();
    try {
      const fiber = ctx.plugin(backend, { root, compression });
      await fiber.await();
      const service = ctx.get("sessionPersistence");
      assert(service);
      await action(service, root);
    } finally {
      await ctx.fiber.dispose();
    }
    counts[name] = "passed";
  };

  await check("plain_complete_and_read_only", "none", async (service, root) => {
    await put(service, root, artifact, bytes(artifact, "none"));
    await unchanged(root, async () => {
      assert.equal((await service.list()).length, 1);
      assert.equal(
        (await service.stat(artifact.header.id)).formatStatus,
        "current",
      );
      const handle = await service.open(artifact.header.id, "read");
      try {
        assert.deepEqual((await handle.read()).events, artifact.events);
        assert.deepEqual(
          (await handle.read(2, 3)).events,
          artifact.events.slice(2, 5),
        );
        assert.deepEqual(
          (await handle.read(artifact.events.length)).events,
          [],
        );
        await assert.rejects(handle.append([]), errors.SessionReadOnlyError);
        await assert.rejects(handle.flush(), errors.SessionReadOnlyError);
      } finally {
        await handle.close();
      }
    });
  });
  await check("zstd_complete_multiframe", "zstd", async (service, root) => {
    await put(service, root, artifact, bytes(artifact, "zstd"));
    await unchanged(root, async () =>
      assert.deepEqual(
        (await readAll(service, artifact.header.id)).events,
        artifact.events,
      ),
    );
  });

  await check(
    "plain_complete_record_without_newline",
    "none",
    async (service, root) => {
      const content = bytes(artifact, "none");
      await put(service, root, artifact, content.subarray(0, -1));
      await unchanged(root, async () =>
        assert.deepEqual(
          (await readAll(service, artifact.header.id)).events,
          artifact.events.slice(0, -1),
        ),
      );
    },
  );
  await check("plain_incomplete_json_tail", "none", async (service, root) => {
    await put(
      service,
      root,
      artifact,
      Buffer.concat([bytes(artifact, "none"), Buffer.from('{"type":')]),
    );
    await unchanged(root, async () =>
      assert.deepEqual(
        (await readAll(service, artifact.header.id)).events,
        artifact.events,
      ),
    );
  });
  await check("plain_corruption_before_end", "none", async (service, root) => {
    const parts = chunks(artifact);
    parts[parts.length - 2] = Buffer.from("invalid JSON\n");
    await put(service, root, artifact, Buffer.concat(parts));
    await unchanged(root, async () =>
      assert.rejects(
        readAll(service, artifact.header.id),
        errors.SessionPersistenceCorruptionError,
      ),
    );
  });
  await check(
    "zstd_torn_checksum_recovers_records",
    "zstd",
    async (service, root) => {
      const content = bytes(artifact, "zstd");
      await put(service, root, artifact, content.subarray(0, -1));
      await unchanged(root, async () =>
        assert.deepEqual(
          (await readAll(service, artifact.header.id)).events,
          artifact.events,
        ),
      );
    },
  );
  await check("zstd_bad_complete_checksum", "zstd", async (service, root) => {
    const content = bytes(artifact, "zstd");
    content[content.length - 1] ^= 1;
    await put(service, root, artifact, content);
    await unchanged(root, async () =>
      assert.rejects(
        readAll(service, artifact.header.id),
        errors.SessionPersistenceCorruptionError,
      ),
    );
  });
  await check(
    "zstd_complete_frame_with_unterminated_record",
    "zstd",
    async (service, root) => {
      const parts = chunks(artifact);
      parts[parts.length - 1] = parts.at(-1).subarray(0, -1);
      await put(service, root, artifact, Buffer.concat(parts.map(pack)));
      await unchanged(root, async () =>
        assert.rejects(
          readAll(service, artifact.header.id),
          errors.SessionPersistenceCorruptionError,
        ),
      );
    },
  );
  await check(
    "zstd_header_must_own_first_frame",
    "zstd",
    async (service, root) => {
      const parts = chunks(artifact);
      await put(
        service,
        root,
        artifact,
        Buffer.concat([
          pack(Buffer.concat(parts.slice(0, 2))),
          ...parts.slice(2).map(pack),
        ]),
      );
      await unchanged(root, async () =>
        assert.rejects(
          readAll(service, artifact.header.id),
          errors.SessionPersistenceCorruptionError,
        ),
      );
    },
  );
  await check("native_retry_through_backend", "none", async (service, root) => {
    const value = withRetry(artifact);
    await put(service, root, value, bytes(value, "none"));
    await unchanged(root, async () =>
      assert.deepEqual(
        (await readAll(service, value.header.id)).events,
        value.events,
      ),
    );
  });
  await check("native_seed_through_backend", "zstd", async (service, root) => {
    const value = seedWithTurn(artifact, "s1");
    await put(service, root, value, bytes(value, "zstd"));
    await unchanged(root, async () => {
      const result = await readAll(service, value.header.id);
      assert.equal(result.inheritedEventCount, artifact.events.length);
      assert.deepEqual(result.events, value.events);
    });
  });
  await check(
    "highest_generation_ignores_newer_mtime",
    "none",
    async (service, root) => {
      await put(service, root, artifact, bytes(artifact, "none"));
      const old = await put(
        service,
        root,
        artifact,
        Buffer.from("not a readable historical log\n"),
        "session.v3.jsonl",
      );
      await utimes(
        old,
        new Date("2030-01-01T00:00:00Z"),
        new Date("2030-01-01T00:00:00Z"),
      );
      await unchanged(root, async () =>
        assert.deepEqual(
          (await readAll(service, artifact.header.id)).events,
          artifact.events,
        ),
      );
    },
  );
  await check(
    "historical_read_and_current_publication",
    "none",
    async (service, root) => {
      // V3 没有本夹具的 developer/tool-role 形状，单独构造两代都合法的纯文本 turn。
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
      const header = { ...value.header, version: 3 };
      const old = Buffer.concat([
        line(codec3.encodeHeader(header, 0)),
        ...value.events.map((event) => line(codec3.encodeEvent(event))),
      ]);
      await put(service, root, value, old, "session.v3.jsonl");
      let handle;
      try {
        await unchanged(root, async () => {
          assert.equal(
            (await service.stat(value.header.id)).formatStatus,
            "migration-required",
          );
          handle = await service.open(value.header.id, "read");
          assert.deepEqual((await handle.read()).events, value.events);
        });
        // 用夹具写入者模拟新代原子发布；backend 只持有 read 句柄。
        const current = service.locate(value.header).path;
        const staged = `${current}.fixture-stage`;
        await writeFile(staged, bytes(value, "none"), { flag: "wx" });
        await rename(staged, current);
        await unchanged(root, async () => {
          assert.equal(
            (await service.stat(value.header.id)).formatStatus,
            "current",
          );
          assert.deepEqual((await handle.read()).events, value.events);
        });
      } finally {
        await handle?.close();
      }
    },
  );
  await check(
    "future_generation_refuses_existing_reader",
    "none",
    async (service, root) => {
      await put(service, root, artifact, bytes(artifact, "none"));
      const handle = await service.open(artifact.header.id, "read");
      try {
        assert.deepEqual((await handle.read()).events, artifact.events);
        const header = {
          ...codec.encodeHeader(artifact.header, 0),
          version: 5,
        };
        await put(service, root, artifact, line(header), "session.v5.jsonl");
        await unchanged(root, async () => {
          await assert.rejects(
            handle.read(),
            errors.SessionFormatUnsupportedError,
          );
          await assert.rejects(
            service.open(artifact.header.id, "read"),
            errors.SessionFormatUnsupportedError,
          );
        });
      } finally {
        await handle.close();
      }
    },
  );
  for (const compression of ["none", "zstd"]) {
    await check(
      `${compression}_existing_reader_observes_append`,
      compression,
      async (service, root) => {
        const path = await put(
          service,
          root,
          artifact,
          bytes(artifact, compression),
        );
        const handle = await service.open(artifact.header.id, "read");
        try {
          assert.deepEqual((await handle.read()).events, artifact.events);
          const own = nextTurn("next");
          const batch = Buffer.concat(
            own.map((event) => line(codec.encodeEvent(event))),
          );
          await appendFile(path, compression === "zstd" ? pack(batch) : batch);
          await unchanged(root, async () => {
            assert.deepEqual((await handle.read()).events, [
              ...artifact.events,
              ...own,
            ]);
            assert.deepEqual(
              (await handle.read(artifact.events.length)).events,
              own,
            );
          });
        } finally {
          await handle.close();
        }
      },
    );
  }
  await check(
    "existing_reader_refuses_shrink",
    "none",
    async (service, root) => {
      const path = await put(service, root, artifact, bytes(artifact, "none"));
      const handle = await service.open(artifact.header.id, "read");
      try {
        assert.deepEqual((await handle.read()).events, artifact.events);
        const replacement = `${path}.fixture-stage`;
        await writeFile(
          replacement,
          Buffer.concat(chunks(artifact).slice(0, -1)),
          { flag: "wx" },
        );
        await rename(replacement, path);
        await unchanged(root, async () =>
          assert.rejects(
            handle.read(),
            /shrank below a previously observed prefix/,
          ),
        );
      } finally {
        await handle.close();
      }
    },
  );

  for (const compression of ["none", "zstd"]) {
    await check(
      `${compression}_write_repairs_synthetic_tail`,
      compression,
      async (service, root) => {
        const complete = bytes(artifact, compression);
        const torn =
          compression === "zstd"
            ? complete.subarray(0, -1)
            : Buffer.concat([complete, Buffer.from('{"type":')]);
        const path = await put(service, root, artifact, torn);
        const writer = await service.open(artifact.header.id, "write");
        const own = nextTurn("repaired");
        try {
          assert.deepEqual((await writer.read()).events, artifact.events);
          // open/read 尚不修尾；首次实际 append 才修复并持久化。
          assert.deepEqual(await readFile(path), torn);
          await writer.append(own);
          await writer.flush();
        } finally {
          await writer.close();
        }
        const repaired = await readFile(path);
        assert(!repaired.equals(torn));
        if (compression === "none") {
          assert.deepEqual(
            repaired,
            Buffer.concat([
              complete,
              ...own.map((event) => line(codec.encodeEvent(event))),
            ]),
          );
        } else {
          const retained = Buffer.concat(
            chunks(artifact).slice(0, -1).map(pack),
          );
          assert.deepEqual(repaired.subarray(0, retained.length), retained);
        }
        await unchanged(root, async () =>
          assert.deepEqual(
            (await readAll(service, artifact.header.id)).events,
            [...artifact.events, ...own],
          ),
        );
      },
    );
  }

  console.log(
    JSON.stringify(
      {
        version,
        node_version: process.version,
        scope: "native_backend_with_synthetic_files",
        run_directory: relative(repo, run).replaceAll("\\", "/"),
        checks: counts,
        check_count: Object.keys(counts).length,
        original_logs_read: false,
        real_session_logs_written: false,
        synthetic_logs_written: true,
        host_started: false,
        total_store_written: false,
        backend_read_validated: true,
        synthetic_tail_write_repair_validated: true,
        concurrent_races_validated: false,
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
          : (error?.name ?? "module_or_fixture_error"),
    }),
  );
  process.exitCode = 2;
});

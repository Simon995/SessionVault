#!/usr/bin/env node
/** 实际捕获函数的双进程受控反例；只操作新建的 target 合成目录。 */
import assert from "node:assert/strict";
import childProcess, { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { constants, zstdCompressSync } from "node:zlib";
import { createDshSnapshotReader } from "./dsh-snapshot-reader.mjs";
import { decodeDshV4Jsonl } from "./dsh-native-admission.mjs";
import { loadDshInstallation } from "./dsh-installed.mjs";
import limits from "./dsh-snapshot-limits.json" with { type: "json" };
import {
  bounded,
  fileReadWindow,
  inside,
  ioWindow,
  startPeer,
} from "../tests/fixtures/dsh-v4/io-harness.mjs";
import { seedWithTurn } from "../tests/fixtures/dsh-v4/scenarios.mjs";

const counts = {};
const observations = {};
let stage = "arguments";
let detail;
const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
const line = (value) => Buffer.from(`${JSON.stringify(value)}\n`);
const pack = (bytes) =>
  zstdCompressSync(bytes, { params: { [constants.ZSTD_c_checksumFlag]: 1 } });

async function main() {
  const { values } = parseArgs({
    options: { "dsh-package": { type: "string" } },
  });
  const packagePath = values["dsh-package"];
  assert(packagePath);
  const { load, version } = loadDshInstallation(packagePath);
  const { Context } = await load("@deepseek-ai/cordis");
  const backend = await load("@deepseek-ai/dsh-session-persistence-jsonl");
  const [format, session] = await Promise.all([
    load("@deepseek-ai/dsh-session-format-v3-to-v4"),
    load("@deepseek-ai/dsh-session"),
  ]);
  const codec = format.releasedV4SessionFormatCodec;
  const artifact = JSON.parse(
    await fs.readFile(
      new URL("../tests/fixtures/dsh-v4/complete-turn.json", import.meta.url),
      "utf8",
    ),
  );
  const child = seedWithTurn(artifact, "child");
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
  const suffix = (compression) => (compression === "zstd" ? ".zstd" : "");
  const repo = await fs.realpath(fileURLToPath(new URL("..", import.meta.url)));
  const target = join(repo, "target");
  const info = await fs.lstat(target);
  assert(info.isDirectory() && !info.isSymbolicLink());
  inside(repo, await fs.realpath(target));
  const run = await fs.mkdtemp(join(target, "dsh-capture-"));
  const peer = await startPeer(run, packagePath);
  const mutate = (op, path, bytes) =>
    peer.call(op, { path, bytes: bytes.toString("base64") });
  const verified = (value) => {
    detail = value;
    assert.equal(value.kind, "verified_snapshot");
  };
  const check = async (name, compression, action, initial = artifact) => {
    stage = `${compression}_${name}`;
    detail = undefined;
    const root = join(run, stage);
    await fs.mkdir(root);
    const ctx = new Context();
    try {
      await ctx.plugin(backend, { root, compression }).await();
      const service = ctx.get("sessionPersistence");
      const reader = await createDshSnapshotReader({
        dshPackage: packagePath,
        persistence: service,
        compression,
        sourceInstance: "fixture-source",
      });
      const content = encode(initial, compression);
      const path = service.locate(initial.header).path;
      inside(root, path);
      await fs.mkdir(dirname(path), { recursive: true });
      await fs.writeFile(path, content, { flag: "wx" });
      await peer.call("init", { root, compression });
      await action({ reader, service, path, root, content });
    } finally {
      await ctx.fiber.dispose();
    }
    counts[stage] = "passed";
  };
  try {
    for (const compression of ["none", "zstd"]) {
      await check(
        "stable_and_opaque",
        compression,
        async ({ reader, service }) => {
          const receipt = await reader.capture(artifact.header.id);
          verified(receipt);
          assert.deepEqual(reader.events(receipt), artifact.events);
          assert.deepEqual(Object.keys(receipt), ["kind"]);
          assert.equal(
            reader.describe(receipt).event_count,
            artifact.events.length,
          );
          assert.equal(
            reader.describe(receipt).production_advance_permit,
            false,
          );
          assert.equal(
            (await reader.recheck(receipt)).kind,
            "matches_observation",
          );
          assert.throws(
            () => reader.events({ kind: "verified_snapshot" }),
            TypeError,
          );
          assert.throws(
            () => reader.events(JSON.parse(JSON.stringify(receipt))),
            TypeError,
          );
          const otherReader = await createDshSnapshotReader({
            dshPackage: packagePath,
            persistence: service,
            compression,
            sourceInstance: "fixture-source",
          });
          assert.throws(() => otherReader.describe(receipt), TypeError);
          const exported = reader.events(receipt);
          exported[0].seq = 500;
          assert.deepEqual(reader.events(receipt), artifact.events);
          observations[stage] = reader.describe(receipt);
        },
      );
      await check(
        "readonly_command",
        compression,
        async ({ reader, root, path }) => {
          const before = hash(await fs.readFile(path));
          const output = spawnSync(
            process.execPath,
            [
              fileURLToPath(
                new URL("./read-dsh-snapshot.mjs", import.meta.url),
              ),
              "--dsh-package",
              packagePath,
              "--root",
              root,
              "--compression",
              compression,
              "--id",
              artifact.header.id,
              "--source-instance",
              "fixture-source",
            ],
            {
              encoding: "utf8",
              timeout: limits.audit_timeout_ms,
              maxBuffer: limits.audit_output_bytes,
            },
          );
          assert.ifError(output.error);
          assert.equal(output.status, 0);
          const summary = JSON.parse(output.stdout);
          assert.equal(summary.kind, "verified_snapshot");
          assert.equal(summary.event_count, artifact.events.length);
          assert.equal(summary.snapshot_sha256, before);
          assert.equal(hash(await fs.readFile(path)), before);
          assert.throws(() => reader.describe(summary), TypeError);
        },
      );
      await check(
        "cancel_running_auditor_tree",
        compression,
        async ({ reader }) => {
          const original = childProcess.spawn;
          const controller = new AbortController();
          let hit = false;
          let ready = false;
          let naturalEnd = false;
          childProcess.spawn = (command, args, options) => {
            if (command !== "uv" || !args.includes("--bytes"))
              return original(command, args, options);
            hit = true;
            // 真 uv → Python 链；若只杀 uv，Python 会继续写出 natural_end。
            const child = original(
              command,
              [
                "run",
                "--no-project",
                "python",
                "-I",
                "-c",
                'import sys, time; sys.stdin.buffer.read(); print("ready", flush=True); time.sleep(3); print("natural_end", flush=True)',
              ],
              options,
            );
            let output = "";
            child.stdout.on("data", (chunk) => {
              output += chunk.toString("utf8");
              naturalEnd ||= output.includes("natural_end");
              if (!ready && output.includes("ready")) {
                ready = true;
                controller.abort();
              }
            });
            return child;
          };
          syncBuiltinESMExports();
          try {
            const result = await bounded(
              reader.capture(artifact.header.id, { signal: controller.signal }),
            );
            assert(hit && ready, "the auditor child did not reach its barrier");
            assert.equal(result.kind, "cancelled");
            assert.equal(
              naturalEnd,
              false,
              "the Python child ran to natural exit",
            );
            observations[stage] = {
              auditor_started: ready,
              natural_end_reached: naturalEnd,
            };
          } finally {
            childProcess.spawn = original;
            syncBuiltinESMExports();
          }
          verified(await reader.capture(artifact.header.id));
        },
      );
      await check(
        "native_append_and_old_prefix",
        compression,
        async ({ reader }) => {
          const old = await reader.capture(artifact.header.id);
          verified(old);
          await peer.call("open", { id: artifact.header.id, access: "write" });
          await peer.call("native-append", { events: own });
          await peer.call("close");
          const next = await reader.capture(artifact.header.id);
          verified(next);
          assert.deepEqual(reader.events(next), grown.events);
          assert.equal(reader.compare(old, next), "prefix_preserved");
          assert.equal((await reader.recheck(old)).kind, "changed");
        },
      );
      await check(
        "same_count_rewrite",
        compression,
        async ({ reader, path }) => {
          const old = await reader.capture(artifact.header.id);
          verified(old);
          const changed = structuredClone(artifact);
          changed.events.find(
            (event) => event.type === "user/message",
          ).data.content[0].text = "Read the changed.";
          await mutate("replace", path, encode(changed, compression));
          const next = await reader.capture(artifact.header.id);
          verified(next);
          assert.equal(
            reader.describe(old).event_count,
            reader.describe(next).event_count,
          );
          assert.equal(reader.compare(old, next), "prefix_rewritten");
          assert.notEqual(
            reader.describe(old).snapshot_sha256,
            reader.describe(next).snapshot_sha256,
          );
        },
      );
      await check("lifecycle_change", compression, async ({ reader, path }) => {
        const old = await reader.capture(artifact.header.id);
        verified(old);
        const changed = structuredClone(artifact);
        changed.header.createdAt += 1;
        await mutate("replace", path, encode(changed, compression));
        const next = await reader.capture(artifact.header.id);
        verified(next);
        assert.deepEqual(reader.events(old), reader.events(next));
        assert.equal(reader.compare(old, next), "lifecycle_changed");
      });
      await check(
        "seeded_inherited_cut_lifecycle_change",
        compression,
        async ({ reader, path, content }) => {
          const changed = seedWithTurn(child, "reseeded");
          // 再次继承完整旧前缀，仅保留同一 header；原生 cut 由最后一个 marker 决定。
          changed.header = structuredClone(child.header);
          assert.deepEqual(child.header, changed.header);
          const oldRows = rows(child);
          const nextRows = rows(changed);
          assert.deepEqual(oldRows[0], nextRows[0]);
          const admitted = [oldRows, nextRows].map((encoded) =>
            decodeDshV4Jsonl(Buffer.concat(encoded), {
              codec,
              format,
              session,
            }),
          );
          // 共享 admission 实际运行 strict codec、restore 与 Session.create。
          assert.deepEqual(admitted, [child, changed]);
          assert.notEqual(
            admitted[0].inheritedEventCount,
            admitted[1].inheritedEventCount,
          );
          assert.deepEqual(
            changed.events.slice(0, child.events.length),
            child.events,
          );
          const replacement = encode(changed, compression);
          const physicalHeader =
            compression === "zstd" ? pack(oldRows[0]) : oldRows[0];
          assert.deepEqual(
            content.subarray(0, physicalHeader.length),
            physicalHeader,
          );
          assert.deepEqual(
            replacement.subarray(0, physicalHeader.length),
            physicalHeader,
          );
          const old = await reader.capture(child.header.id);
          verified(old);
          assert.deepEqual(reader.events(old), child.events);
          await mutate("replace", path, replacement);
          const next = await reader.capture(child.header.id);
          verified(next);
          assert.deepEqual(reader.events(next), changed.events);
          const before = reader.describe(old);
          const after = reader.describe(next);
          assert.equal(before.source_binding, after.source_binding);
          assert.equal(
            before.inherited_event_count,
            admitted[0].inheritedEventCount,
          );
          assert.equal(
            after.inherited_event_count,
            admitted[1].inheritedEventCount,
          );
          assert.notEqual(before.lifecycle, after.lifecycle);
          const relation = reader.compare(old, next);
          assert.equal(relation, "lifecycle_changed");
          observations[stage] = {
            logical_header_equal: true,
            encoded_header_equal: true,
            physical_header_equal: true,
            native_admitted: admitted.length,
            inherited_event_counts: [
              before.inherited_event_count,
              after.inherited_event_count,
            ],
            event_counts: [before.event_count, after.event_count],
            old_event_prefix_preserved: true,
            relation,
          };
        },
        child,
      );
      await check(
        "incomplete_holds_old_snapshot",
        compression,
        async ({ reader, path, content }) => {
          const old = await reader.capture(artifact.header.id);
          verified(old);
          await mutate("replace", path, content.subarray(0, -1));
          const next = await reader.capture(artifact.header.id);
          detail = next;
          assert.equal(next.kind, "incomplete");
          assert.deepEqual(reader.events(old), artifact.events);
          assert.throws(() => reader.compare(old, next), TypeError);
        },
      );
      await check(
        "append_during_pinned_read",
        compression,
        async ({ reader, path, content }) => {
          const plain = Buffer.concat(
            own.map((event) => line(codec.encodeEvent(event))),
          );
          const { value, hits } = await fileReadWindow(
            path,
            content.length,
            async (hit) => {
              assert.equal(hit, 1);
              await mutate(
                "append",
                path,
                compression === "zstd" ? pack(plain) : plain,
              );
            },
            () => reader.capture(artifact.header.id),
          );
          detail = value;
          assert.equal(hits, 1);
          assert.equal(value.kind, "changed");
          const next = await reader.capture(artifact.header.id);
          verified(next);
          assert.deepEqual(reader.events(next), grown.events);
        },
      );
      await check(
        "rewrite_during_pinned_read",
        compression,
        async ({ reader, path, content }) => {
          const changed = structuredClone(artifact);
          changed.events.find(
            (event) => event.type === "user/message",
          ).data.content[0].text = "Read the changed.";
          const { value } = await fileReadWindow(
            path,
            content.length,
            async (hit) => {
              assert.equal(hit, 1);
              const replacement = encode(changed, compression);
              assert.equal(replacement.length, content.length);
              await mutate("overwrite", path, replacement);
            },
            () => reader.capture(artifact.header.id),
          );
          detail = value;
          assert.equal(value.kind, "changed");
        },
      );
      await check(
        "aba_file_replacement",
        compression,
        async ({ reader, path, content }) => {
          const oldIdentity = await fs.stat(path, { bigint: true });
          const { value, trace } = await ioWindow(
            "lstat",
            path,
            async (hit) => {
              if (hit === 2) {
                await mutate("replace", path, encode(grown, compression));
                await mutate("replace", path, content);
              }
            },
            () => reader.capture(artifact.header.id),
          );
          assert(trace.length >= 2);
          detail = value;
          assert.equal(value.kind, "changed");
          assert.equal(hash(await fs.readFile(path)), hash(content));
          const after = await fs.stat(path, { bigint: true });
          assert(
            oldIdentity.ino !== after.ino ||
              oldIdentity.ctimeNs !== after.ctimeNs,
          );
          observations[stage] = {
            same_final_bytes: true,
            replacement_detected: true,
          };
        },
      );
      await check(
        "future_generation_during_read",
        compression,
        async ({ reader, path, content }) => {
          const future = line({
            ...codec.encodeHeader(artifact.header, 0),
            version: 5,
          });
          const { value } = await fileReadWindow(
            path,
            content.length,
            async (hit) => {
              assert.equal(hit, 1);
              await mutate(
                "publish",
                join(dirname(path), `session.v5.jsonl${suffix(compression)}`),
                compression === "zstd" ? pack(future) : future,
              );
            },
            () => reader.capture(artifact.header.id),
          );
          detail = value;
          assert.equal(value.kind, "unsupported");
          assert.equal(hash(await fs.readFile(path)), hash(content));
        },
      );
      await check(
        "late_generation_recheck",
        compression,
        async ({ reader, path }) => {
          const old = await reader.capture(artifact.header.id);
          verified(old);
          const future = line({
            ...codec.encodeHeader(artifact.header, 0),
            version: 5,
          });
          await mutate(
            "publish",
            join(dirname(path), `session.v5.jsonl${suffix(compression)}`),
            compression === "zstd" ? pack(future) : future,
          );
          const result = await reader.recheck(old);
          detail = result;
          assert.equal(result.kind, "unsupported");
          assert.deepEqual(reader.events(old), artifact.events);
          assert.equal(
            reader.describe(old).atomic_source_snapshot_proven,
            false,
          );
        },
      );
      await check(
        "cancel_before_and_during_read",
        compression,
        async ({ reader, path, content }) => {
          const before = new AbortController();
          before.abort();
          assert.equal(
            (
              await reader.capture(artifact.header.id, {
                signal: before.signal,
              })
            ).kind,
            "cancelled",
          );
          const during = new AbortController();
          const { value } = await fileReadWindow(
            path,
            content.length,
            async () => {
              during.abort();
            },
            () => reader.capture(artifact.header.id, { signal: during.signal }),
          );
          detail = value;
          assert.equal(value.kind, "cancelled");
        },
      );
      await check(
        "injected_io_failure",
        compression,
        async ({ reader, path }) => {
          const original = fs.open;
          let hits = 0;
          fs.open = async (...args) => {
            if (args[0] === path) {
              hits += 1;
              throw Object.assign(new Error("fixture I/O"), { code: "EIO" });
            }
            return await original(...args);
          };
          syncBuiltinESMExports();
          try {
            const result = await reader.capture(artifact.header.id);
            detail = result;
            assert.equal(result.kind, "unavailable");
            assert(hits > 0);
          } finally {
            fs.open = original;
            syncBuiltinESMExports();
          }
        },
      );
      await check(
        "semantic_rejection",
        compression,
        async ({ reader, path, content }) => {
          const unknown = line({
            type: "fixture/unknown",
            seq: artifact.events.length,
            time: 25,
            data: {},
          });
          await mutate(
            "replace",
            path,
            Buffer.concat([
              content,
              compression === "zstd" ? pack(unknown) : unknown,
            ]),
          );
          const result = await reader.capture(artifact.header.id);
          detail = result;
          assert.equal(result.kind, "rejected");
        },
      );
      await check(
        "native_repair_intermediate_snapshot",
        compression,
        async ({ reader, service, path, content }) => {
          const old = await reader.capture(artifact.header.id);
          verified(old);
          const bad =
            compression === "zstd"
              ? content.subarray(0, -1)
              : Buffer.concat([content, Buffer.from('{"type":')]);
          await mutate("replace", path, bad);
          const writer = await service.open(artifact.header.id, "write");
          let middle;
          try {
            await ioWindow(
              "truncate",
              path,
              async (hit) => {
                assert.equal(hit, 1);
                middle = await reader.capture(artifact.header.id);
              },
              async () => {
                await writer.append(own);
                await writer.flush();
              },
            );
          } finally {
            await writer.close();
          }
          verified(middle);
          const relation = reader.compare(old, middle);
          assert.equal(
            relation,
            compression === "zstd" ? "prefix_shrank" : "prefix_preserved",
          );
          const next = await reader.capture(artifact.header.id);
          verified(next);
          assert.equal(reader.compare(old, next), "prefix_preserved");
          assert.deepEqual(reader.events(next), grown.events);
          observations[stage] = {
            middle_event_count: reader.describe(middle).event_count,
            middle_relation: relation,
            final_event_count: reader.describe(next).event_count,
            advance_permit: reader.describe(middle).production_advance_permit,
          };
        },
      );
      await check(
        "metadata_missing_is_not_empty",
        compression,
        async ({ reader }) => {
          const result = await reader.capture("missing");
          detail = result;
          assert.equal(result.kind, "unavailable");
          assert.throws(() => reader.events(result), TypeError);
        },
      );
    }
    await check("repacked_frame_prefix", "zstd", async ({ reader, path }) => {
      const old = await reader.capture(artifact.header.id);
      verified(old);
      const parts = rows(artifact);
      await mutate(
        "replace",
        path,
        Buffer.concat([pack(parts[0]), pack(Buffer.concat(parts.slice(1)))]),
      );
      const next = await reader.capture(artifact.header.id);
      verified(next);
      assert.equal(reader.compare(old, next), "prefix_preserved");
      assert.notEqual(
        reader.describe(old).snapshot_sha256,
        reader.describe(next).snapshot_sha256,
      );
      assert.equal(
        reader.describe(old).jsonl_sha256,
        reader.describe(next).jsonl_sha256,
      );
    });
    await check("input_budget", "none", async ({ reader, path }) => {
      const content = Buffer.alloc(limits.input_bytes + 1, 32);
      rows(artifact)[0].copy(content);
      await mutate("replace", path, content);
      const result = await reader.capture(artifact.header.id);
      detail = result;
      assert.equal(result.kind, "budget_exceeded");
    });
  } finally {
    await peer.stop();
  }
  console.log(
    JSON.stringify(
      {
        version,
        node: process.version,
        run: relative(repo, run),
        checks: counts,
        observations,
        controlled_capture_windows_validated: true,
        same_buffer_validation_and_decode: true,
        general_stress_validated: false,
        arbitrary_aba_excluded: false,
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
      detail,
    }),
  );
  process.exitCode = 1;
});

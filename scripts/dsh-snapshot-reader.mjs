/** v4 原型 reader：同一缓冲区审计与解码；不注册 provider，不构造生产推进凭据。 */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { isDeepStrictEqual } from "node:util";
import { zstdDecompressSync } from "node:zlib";
import limits from "./dsh-snapshot-limits.json" with { type: "json" };
import { loadDshInstallation } from "./dsh-installed.mjs";

const hash = (value) => createHash("sha256").update(value).digest("hex");
class Blocked extends Error {
  constructor(kind, reason) {
    super(reason);
    this.kind = kind;
    this.reason = reason;
  }
}
const refuse = (kind, reason) => {
  throw new Blocked(kind, reason);
};
const stamp = (value) => {
  const fields = [
    value.dev,
    value.ino,
    value.size,
    value.mtimeNs,
    value.ctimeNs,
  ];
  if (!fields.every((field) => typeof field === "bigint"))
    refuse("unavailable", "identity_fields_unavailable");
  return fields.map(String);
};
const sameStamp = (left, right) => isDeepStrictEqual(stamp(left), stamp(right));

async function auditBytes(bytes, compression, signal) {
  return await new Promise((accept, reject) => {
    signal?.throwIfAborted();
    const child = spawn(
      "uv",
      [
        "run",
        fileURLToPath(new URL("./check-dsh-snapshot.py", import.meta.url)),
        "--bytes",
        "--compression",
        compression,
      ],
      {
        cwd: fileURLToPath(new URL("..", import.meta.url)),
        stdio: ["pipe", "pipe", "pipe"],
        detached: process.platform !== "win32",
        windowsHide: true,
      },
    );
    const chunks = [];
    let size = 0;
    let expired = false;
    let oversized = false;
    let failure;
    let stopping = false;
    let closed = false;
    let cleanupTimer;
    let killer;
    // uv 在 Windows 上等待 Python 子进程；不能先 kill uv 再尝试按 PID 找整棵树。
    const terminate = () => {
      if (stopping || closed) return;
      stopping = true;
      cleanupTimer = setTimeout(() => {
        signal?.removeEventListener("abort", terminate);
        clearTimeout(timer);
        if (killer && killer.exitCode === null) killer.kill();
        child.stdin.destroy();
        child.stdout.destroy();
        child.stderr.destroy();
        child.unref();
        reject(new Blocked("unavailable", "auditor_cleanup_unconfirmed"));
      }, limits.audit_cleanup_timeout_ms);
      if (!child.pid) return;
      if (process.platform === "win32") {
        killer = spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
          stdio: "ignore",
          windowsHide: true,
        });
        killer.on("error", (error) => {
          failure ??= error;
          // 只作 best-effort 收尾；清理未确认时不会构造成功结果。
          child.kill();
        });
        killer.once("close", (code) => {
          if (code !== 0 && !closed) child.kill();
        });
      } else {
        try {
          process.kill(-child.pid, "SIGKILL");
        } catch (error) {
          if (error.code !== "ESRCH") {
            failure ??= error;
            child.kill();
          }
        }
      }
    };
    const timer = setTimeout(() => {
      expired = true;
      terminate();
    }, limits.audit_timeout_ms);
    child.on("error", (error) => {
      failure = error;
    });
    child.stdin.on("error", (error) => {
      failure ??= error;
    });
    child.stderr.resume();
    child.stdout.on("data", (chunk) => {
      size += chunk.length;
      if (size > limits.audit_output_bytes) {
        oversized = true;
        terminate();
      } else chunks.push(chunk);
    });
    child.once("close", (code) => {
      closed = true;
      clearTimeout(timer);
      clearTimeout(cleanupTimer);
      signal?.removeEventListener("abort", terminate);
      if (signal?.aborted) return reject(new Blocked("cancelled", "cancelled"));
      if (expired || oversized || failure || code !== 0)
        return reject(new Blocked("unavailable", "auditor_unavailable"));
      try {
        accept(JSON.parse(Buffer.concat(chunks).toString("utf8")));
      } catch {
        reject(new Blocked("unavailable", "auditor_invalid_result"));
      }
    });
    signal?.addEventListener("abort", terminate, { once: true });
    if (signal?.aborted) terminate();
    else child.stdin.end(bytes);
  });
}

/** persistence 是显式配置的原生 JSONL service；身份仅在该 reader 生命周期内比较。 */
export async function createDshSnapshotReader({
  dshPackage,
  persistence,
  compression,
  sourceInstance,
}) {
  assert(["none", "zstd"].includes(compression));
  assert(typeof sourceInstance === "string" && sourceInstance.length > 0);
  const { load } = loadDshInstallation(dshPackage);
  const format = await load("@deepseek-ai/dsh-session-format-v3-to-v4");
  const filenames = await load("@deepseek-ai/dsh-session-format");
  const session = await load("@deepseek-ai/dsh-session");
  const codec = format.releasedV4SessionFormatCodec;
  const serviceIdentity = persistence.identity;
  const receipts = new WeakMap();

  const select = async (directory, signal) => {
    signal?.throwIfAborted();
    const entries = await fs.readdir(directory, { withFileTypes: true });
    signal?.throwIfAborted();
    const candidates = [];
    for (const entry of entries) {
      const zipped = entry.name.endsWith(".zstd");
      const base = zipped ? entry.name.slice(0, -".zstd".length) : entry.name;
      const generation = filenames.parseSessionFormatLogFilename(base);
      if (generation === undefined) continue;
      if (zipped !== (compression === "zstd"))
        refuse("unsupported", "encoding_conflict");
      if (!entry.isFile() || entry.isSymbolicLink())
        refuse("unsupported", "artifact_kind");
      candidates.push({ name: entry.name, generation });
    }
    candidates.sort((a, b) => b.generation - a.generation);
    const chosen = candidates[0];
    if (!chosen) refuse("changed", "selection_lost");
    if (chosen.generation !== codec.version)
      refuse("unsupported", "physical_generation");
    return { ...chosen, path: join(directory, chosen.name) };
  };
  const metadata = async (id, signal) => {
    signal?.throwIfAborted();
    if (persistence.identity !== serviceIdentity)
      refuse("changed", "service_replaced");
    const value = await persistence.stat(id, { signal });
    signal?.throwIfAborted();
    // undefined 不再推断为空会话；没有 header 就没有可绑定的快照。
    if (!value) refuse("unavailable", "metadata_not_observed");
    if (value.formatStatus !== "current")
      refuse("unsupported", "migration_required");
    if (value.sizeBytes === undefined)
      refuse("unavailable", "not_materialized");
    return value;
  };
  const ensureSameMetadata = (before, after) => {
    if (
      before.revision !== after.revision ||
      before.sizeBytes !== after.sizeBytes ||
      !isDeepStrictEqual(before.header, after.header)
    )
      refuse("changed", "metadata_changed");
  };
  const observeAgain = async (saved, signal) => {
    const chosen = await select(saved.directory, signal);
    if (
      chosen.path !== saved.selected.path ||
      chosen.generation !== saved.selected.generation
    )
      refuse("changed", "selection_changed");
    const actual = await fs.lstat(chosen.path, { bigint: true });
    signal?.throwIfAborted();
    if (!actual.isFile() || actual.isSymbolicLink())
      refuse("changed", "artifact_kind_changed");
    if (!sameStamp(actual, saved.identity)) refuse("changed", "file_changed");
    const after = await metadata(saved.id, signal);
    ensureSameMetadata(saved.metadata, after);
    // stat 内部可能重选；最后再观察本目录，不能把较早的目录清单配给新 metadata。
    const final = await select(saved.directory, signal);
    if (final.path !== chosen.path || final.generation !== chosen.generation)
      refuse("changed", "selection_changed");
  };
  const failure = (error, signal) => {
    if (error instanceof Blocked)
      return Object.freeze({ kind: error.kind, reason: error.reason });
    if (signal?.aborted || error.name === "AbortError")
      return Object.freeze({ kind: "cancelled", reason: "cancelled" });
    if (error.name === "SessionFormatUnsupportedError")
      return Object.freeze({ kind: "unsupported", reason: "native_format" });
    return Object.freeze({
      kind: "unavailable",
      reason: "source_observation_failed",
    });
  };

  const capture = async (id, { signal } = {}) => {
    try {
      const before = await metadata(id, signal);
      const directory = await fs.realpath(
        dirname(persistence.locate(before.header).path),
      );
      const selected = await select(directory, signal);
      const pathIdentity = await fs.lstat(selected.path, { bigint: true });
      if (!pathIdentity.isFile() || pathIdentity.isSymbolicLink())
        refuse("unsupported", "artifact_kind");
      const handle = await fs.open(selected.path, "r");
      let bytes;
      let identity;
      try {
        identity = await handle.stat({ bigint: true });
        if (!sameStamp(identity, pathIdentity))
          refuse("changed", "opened_different_file");
        if (identity.size > BigInt(limits.input_bytes))
          refuse("budget_exceeded", "input_budget");
        const length = Number(identity.size);
        bytes = Buffer.alloc(length);
        let offset = 0;
        while (offset < length) {
          signal?.throwIfAborted();
          const { bytesRead } = await handle.read(
            bytes,
            offset,
            length - offset,
            offset,
          );
          if (bytesRead === 0) refuse("changed", "short_read");
          offset += bytesRead;
        }
        signal?.throwIfAborted();
        if (!sameStamp(identity, await handle.stat({ bigint: true })))
          refuse("changed", "handle_changed");
      } finally {
        await handle.close();
      }
      const saved = { id, directory, selected, identity, metadata: before };
      await observeAgain(saved, signal);
      const audit = await auditBytes(bytes, compression, signal);
      if (audit.status !== "complete") refuse(audit.status, audit.reason);
      if (
        audit.snapshot_sha256 !== hash(bytes) ||
        audit.verified_physical_bytes !== bytes.length
      )
        refuse("unavailable", "audit_binding_mismatch");
      let plain = bytes;
      if (compression === "zstd") {
        let end = 0;
        const chunks = audit.frame_spans.map(([start, stop]) => {
          assert(start === end && stop > start && stop <= bytes.length);
          end = stop;
          return zstdDecompressSync(bytes.subarray(start, stop), {
            maxOutputLength: limits.decoded_bytes,
          });
        });
        assert.equal(end, bytes.length);
        plain = Buffer.concat(chunks);
      }
      if (hash(plain) !== audit.verified_jsonl_sha256)
        refuse("unavailable", "decoded_binding_mismatch");
      let artifact;
      try {
        const rows = new TextDecoder("utf-8", { fatal: true })
          .decode(plain)
          .split("\n");
        assert.equal(rows.pop(), "");
        const physicalHeader = JSON.parse(rows.shift());
        if (physicalHeader.version !== selected.generation)
          refuse("rejected", "header_generation_mismatch");
        const decoder = codec.createDecoder(physicalHeader, "strict");
        const events = [];
        const context = {
          emitEvent: (event) => events.push(event),
          emitRun: () => {
            throw new Error("unexpected packed v4 run");
          },
        };
        for (const row of rows) decoder.decodeRow(JSON.parse(row), context);
        artifact = {
          header: decoder.header,
          events,
          inheritedEventCount: decoder.finish(context),
        };
        format.restoreReleasedV4Artifact(
          artifact,
          session.KNOWN_SESSION_EVENT_TYPES,
        );
        session.Session.create(
          artifact.header.id,
          artifact.events,
          artifact.header,
          artifact.inheritedEventCount,
        );
        if (
          artifact.header.id !== id ||
          !isDeepStrictEqual(artifact.header, before.header)
        )
          refuse("changed", "header_binding_changed");
        if (audit.records !== artifact.events.length + 1)
          refuse("rejected", "record_coverage_mismatch");
      } catch (error) {
        if (error instanceof Blocked) throw error;
        refuse("rejected", "semantic_admission");
      }
      await observeAgain(saved, signal);
      signal?.throwIfAborted();
      const receipt = Object.freeze({ kind: "verified_snapshot" });
      receipts.set(receipt, {
        ...saved,
        artifact,
        bytes,
        audit,
        binding: hash(
          JSON.stringify([
            sourceInstance,
            directory,
            id,
            compression,
            selected.generation,
          ]),
        ),
        lifecycle: hash(
          JSON.stringify(
            codec.encodeHeader(artifact.header, artifact.inheritedEventCount),
          ),
        ),
      });
      return receipt;
    } catch (error) {
      return failure(error, signal);
    }
  };
  const requireReceipt = (receipt) => {
    const saved = receipts.get(receipt);
    if (!saved) throw new TypeError("receipt does not belong to this reader");
    return saved;
  };
  return Object.freeze({
    capture,
    describe(receipt) {
      const saved = requireReceipt(receipt);
      return Object.freeze({
        source_binding: saved.binding,
        lifecycle: saved.lifecycle,
        physical_generation: saved.selected.generation,
        compression,
        event_count: saved.artifact.events.length,
        inherited_event_count: saved.artifact.inheritedEventCount,
        snapshot_sha256: saved.audit.snapshot_sha256,
        jsonl_sha256: saved.audit.verified_jsonl_sha256,
        physical_bytes: saved.bytes.length,
        atomic_source_snapshot_proven: false,
        production_advance_permit: false,
      });
    },
    events(receipt) {
      return structuredClone(requireReceipt(receipt).artifact.events);
    },
    compare(prior, next) {
      const old = requireReceipt(prior);
      const current = requireReceipt(next);
      if (old.binding !== current.binding) return "different_source";
      if (old.lifecycle !== current.lifecycle) return "lifecycle_changed";
      if (current.artifact.events.length < old.artifact.events.length)
        return "prefix_shrank";
      if (
        !isDeepStrictEqual(
          old.artifact.events,
          current.artifact.events.slice(0, old.artifact.events.length),
        )
      )
        return "prefix_rewritten";
      return "prefix_preserved";
    },
    async recheck(receipt, { signal } = {}) {
      const saved = requireReceipt(receipt);
      try {
        await observeAgain(saved, signal);
        return Object.freeze({ kind: "matches_observation" });
      } catch (error) {
        return failure(error, signal);
      }
    },
  });
}

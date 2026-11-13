/** 原生 v4 admission 的共享实现；只解码已取得的 JSONL，不打开来源文件。 */
import nativeAssert from "node:assert/strict";
import { createHash as nativeCreateHash } from "node:crypto";
import { parseArgs as nativeParseArgs } from "node:util";

const nativeHash = (value) =>
  nativeCreateHash("sha256").update(value).digest("hex");

export function decodeDshV4Jsonl(
  plain,
  { codec, format, session },
  expectedGeneration = codec.version,
) {
  const rows = new TextDecoder("utf-8", { fatal: true })
    .decode(plain)
    .split("\n");
  nativeAssert.equal(rows.pop(), "");
  const header = JSON.parse(rows.shift());
  if (
    header.version !== expectedGeneration ||
    header.version !== codec.version
  ) {
    const error = new Error("physical header generation mismatch");
    error.code = "DSH_HEADER_GENERATION";
    throw error;
  }
  const decoder = codec.createDecoder(header, "strict");
  const events = [];
  const context = {
    emitEvent: (event) => events.push(event),
    emitRun: () => {
      throw new Error("unexpected packed v4 run");
    },
  };
  for (const row of rows) decoder.decodeRow(JSON.parse(row), context);
  const artifact = {
    header: decoder.header,
    events,
    inheritedEventCount: decoder.finish(context),
  };
  format.restoreReleasedV4Artifact(artifact, session.KNOWN_SESSION_EVENT_TYPES);
  session.Session.create(
    artifact.header.id,
    events,
    artifact.header,
    artifact.inheritedEventCount,
  );
  nativeAssert.equal(
    events.length,
    rows.length,
    "native events must cover every physical event record",
  );
  return artifact;
}

export function nativeLifecycleHash(artifact, codec) {
  // v4 encodeHeader 只验证 cut，不把它写入物理 header；cut 必须另外绑定。
  return nativeHash(
    JSON.stringify([
      codec.encodeHeader(artifact.header, artifact.inheritedEventCount),
      artifact.inheritedEventCount,
    ]),
  );
}

export function nativeEventHashes(events) {
  return events.map((event) => nativeHash(JSON.stringify(event)));
}

/** Rust 内嵌本模块和安装约束，再显式调用此 stdin/stdout 入口；不依赖运行时脚本路径。 */
export async function runNativeAdmission(loadInstallation, limits, args) {
  const emit = (value, exitCode) => {
    process.stdout.write(`${JSON.stringify(value)}\n`);
    process.exitCode = exitCode;
  };
  let toolkit;
  let installedVersion;
  try {
    const { values } = nativeParseArgs({
      args,
      options: { "dsh-package": { type: "string" } },
    });
    nativeAssert(values["dsh-package"]);
    const installation = loadInstallation(values["dsh-package"]);
    const [format, session] = await Promise.all([
      installation.load("@deepseek-ai/dsh-session-format-v3-to-v4"),
      installation.load("@deepseek-ai/dsh-session"),
    ]);
    toolkit = { format, session, codec: format.releasedV4SessionFormatCodec };
    // 版本已由 loadInstallation 核查；只输出 package 元数据，不输出安装路径。
    installedVersion = installation.version;
  } catch {
    emit({ kind: "unavailable", reason: "native_runtime_unavailable" }, 1);
    return;
  }
  let plain;
  try {
    const chunks = [];
    let size = 0;
    for await (const chunk of process.stdin) {
      size += chunk.length;
      if (size > limits.decoded_bytes) {
        emit({ kind: "budget_exceeded", reason: "native_input_budget" }, 2);
        return;
      }
      chunks.push(chunk);
    }
    plain = Buffer.concat(chunks);
  } catch {
    emit({ kind: "unavailable", reason: "native_input_failed" }, 1);
    return;
  }
  let artifact;
  try {
    artifact = decodeDshV4Jsonl(plain, toolkit);
  } catch {
    emit({ kind: "rejected", reason: "semantic_admission" }, 2);
    return;
  }
  const eventHashes = nativeEventHashes(artifact.events);
  emit(
    {
      kind: "native_admitted",
      session_id: artifact.header.id,
      jsonl_sha256: nativeHash(plain),
      header_sha256: nativeHash(plain.subarray(0, plain.indexOf(10) + 1)),
      native_events: artifact.events.length,
      inherited_event_count: artifact.inheritedEventCount,
      lifecycle_sha256: nativeLifecycleHash(artifact, toolkit.codec),
      native_events_sha256: nativeHash(JSON.stringify(artifact.events)),
      event_sha256s: eventHashes,
      dsh_version: installedVersion,
    },
    0,
  );
}

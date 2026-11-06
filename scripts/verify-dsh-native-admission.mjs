#!/usr/bin/env node
/** 以显式 Rust example 和全新合成 SDK 检查原生接缝的失败与进程收尾。
 * 成功语义由 verify-dsh-rust-snapshot.mjs 对真实安装版验证；这里不替代它。
 * 只写项目 target，不读真实日志、不启动 Host/LLM、不写总库；保留运行物。
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import {
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  writeFile,
} from "node:fs/promises";
import { isAbsolute, join, posix, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { setTimeout as delay } from "node:timers/promises";
import {
  loadDshInstallation,
  SUPPORTED_DSH_VERSION,
} from "./dsh-installed.mjs";
import limits from "./dsh-snapshot-limits.json" with { type: "json" };

const checks = {};
const observations = {};
const TIMEOUT_MS = 3000;
const hash = (value) => createHash("sha256").update(value).digest("hex");
let stage = "arguments";

function inside(root, path) {
  const rel = relative(root, path);
  assert(
    rel && !isAbsolute(rel) && rel !== ".." && !rel.startsWith(`..${sep}`),
  );
}

async function main() {
  const { values } = parseArgs({
    options: {
      "dsh-package": { type: "string" },
      "rust-reader": { type: "string" },
      "wsl-reader": { type: "string" },
      "native-node": { type: "string" },
    },
  });
  assert(values["dsh-package"] && values["native-node"]);
  assert.equal(
    Number(Boolean(values["rust-reader"])) +
      Number(Boolean(values["wsl-reader"])),
    1,
  );
  const inWsl = Boolean(values["wsl-reader"]);
  const reader = inWsl ? values["wsl-reader"] : resolve(values["rust-reader"]);
  assert((inWsl ? posix.isAbsolute : isAbsolute)(reader));
  assert((inWsl ? posix.isAbsolute : isAbsolute)(values["native-node"]));
  const wsl = (args, allowed = [0]) => {
    const child = spawnSync("wsl.exe", ["--exec", ...args], {
      encoding: "utf8",
      timeout: limits.audit_timeout_ms,
      maxBuffer: limits.audit_output_bytes,
    });
    assert.ifError(child.error);
    assert(allowed.includes(child.status));
    return child;
  };
  if (inWsl) {
    assert.equal(process.platform, "win32");
    wsl(["test", "-x", reader]);
    wsl(["test", "-x", values["native-node"]]);
  } else {
    assert((await lstat(reader)).isFile());
  }
  const repo = await realpath(fileURLToPath(new URL("..", import.meta.url)));
  const target = join(repo, "target");
  const info = await lstat(target);
  assert(info.isDirectory() && !info.isSymbolicLink());
  inside(repo, await realpath(target));
  const run = await mkdtemp(join(target, "dsh-native-admission-"));
  inside(target, await realpath(run));
  const wslRun = inWsl ? wsl(["wslpath", "-u", run]).stdout.trim() : undefined;
  if (inWsl) assert(posix.isAbsolute(wslRun));
  const nativePath = (path) => {
    inside(run, path);
    return inWsl
      ? posix.join(wslRun, relative(run, path).split(sep).join("/"))
      : path;
  };
  const { load } = loadDshInstallation(values["dsh-package"]);
  const format = await load("@deepseek-ai/dsh-session-format-v3-to-v4");
  const codec = format.releasedV4SessionFormatCodec;
  const artifact = JSON.parse(
    await readFile(
      new URL("../tests/fixtures/dsh-v4/complete-turn.json", import.meta.url),
      "utf8",
    ),
  );
  const plain = Buffer.from(
    [
      codec.encodeHeader(artifact.header, artifact.inheritedEventCount),
      ...artifact.events.map((event) => codec.encodeEvent(event)),
    ]
      .map((row) => `${JSON.stringify(row)}\n`)
      .join(""),
  );
  const source = join(run, "synthetic.jsonl");
  await writeFile(source, plain, { flag: "wx" });
  const baseline = {
    kind: "native_admitted",
    jsonl_sha256: hash(plain),
    header_sha256: hash(plain.subarray(0, plain.indexOf(10) + 1)),
    native_events: artifact.events.length,
    inherited_event_count: 0,
    lifecycle_sha256: hash("fixture-lifecycle"),
    native_events_sha256: hash("fixture-events"),
    dsh_version: SUPPORTED_DSH_VERSION,
  };
  const hold = "await new Promise(() => setInterval(() => {}, 1000));";
  const drain = "for await (const chunk of process.stdin) { void chunk; }";
  const reply = (value, code = 0) =>
    `${drain}\nprocess.stdout.write(${JSON.stringify(`${JSON.stringify(value)}\n`)});\nprocess.exit(${code});`;
  const makePackage = async (name, body, version = SUPPORTED_DSH_VERSION) => {
    const root = join(run, name);
    await mkdir(root);
    const anchor = join(root, "package.json");
    await writeFile(
      anchor,
      JSON.stringify({ name: "@deepseek-ai/dsh", version }),
      { flag: "wx" },
    );
    for (const [module, text] of [
      ["dsh-session-format-v3-to-v4", body],
      ["dsh-session", "export {};"],
    ]) {
      const dir = join(root, "node_modules", "@deepseek-ai", module);
      await mkdir(dir, { recursive: true });
      await writeFile(
        join(dir, "package.json"),
        JSON.stringify({
          name: `@deepseek-ai/${module}`,
          type: "module",
          exports: "./index.mjs",
        }),
        { flag: "wx" },
      );
      await writeFile(join(dir, "index.mjs"), text, { flag: "wx" });
    }
    return anchor;
  };
  const invoke = (
    anchor,
    { node = values["native-node"], timeout = TIMEOUT_MS } = {},
  ) => {
    const child = spawnSync(
      inWsl ? "wsl.exe" : reader,
      [
        ...(inWsl ? ["--exec", reader] : []),
        "--path",
        nativePath(source),
        "--compression",
        "none",
        "--node",
        node,
        "--dsh-package",
        nativePath(anchor),
        "--native-timeout-ms",
        String(timeout),
      ],
      {
        cwd: repo,
        encoding: "utf8",
        maxBuffer: limits.audit_output_bytes,
        timeout: limits.audit_timeout_ms + limits.audit_cleanup_timeout_ms,
      },
    );
    assert.ifError(child.error);
    assert.equal(child.status, 2, `${stage}: ${child.stdout}`);
    const report = JSON.parse(child.stdout);
    assert.equal(report.record_positions, undefined);
    assert.equal(report.native_admission, undefined);
    assert.equal(report.snapshot_sha256, undefined);
    assert.equal(child.stdout.includes(plain.toString("utf8")), false);
    return report;
  };
  const expect = (report, kind, reason) => {
    assert.deepEqual(report, { kind, reason }, stage);
    checks[stage] = "passed";
  };
  // 通过真实模块加载边界模拟损坏输出；不为生产 API 添加注入参数。
  for (const [name, body, kind, reason] of [
    [
      "invalid_json",
      `${drain}\nprocess.stdout.write("{bad"); process.exit(0);`,
      "unavailable",
      "native_response_invalid",
    ],
    [
      "missing_fields",
      reply({ kind: "native_admitted" }),
      "unavailable",
      "native_response_invalid",
    ],
    [
      "wrong_jsonl",
      reply({ ...baseline, jsonl_sha256: hash("other") }),
      "unverified",
      "native_binding_mismatch",
    ],
    [
      "wrong_header",
      reply({ ...baseline, header_sha256: hash("other") }),
      "unverified",
      "native_binding_mismatch",
    ],
    [
      "wrong_coverage",
      reply({ ...baseline, native_events: artifact.events.length + 1 }),
      "unverified",
      "native_binding_mismatch",
    ],
    [
      "wrong_cut",
      reply({ ...baseline, inherited_event_count: artifact.events.length + 1 }),
      "unverified",
      "native_binding_mismatch",
    ],
    [
      "wrong_exit",
      reply(baseline, 1),
      "unverified",
      "native_exit_response_mismatch",
    ],
    [
      "rejected_with_zero_exit",
      reply({ kind: "rejected", reason: "fixture" }),
      "unverified",
      "native_exit_response_mismatch",
    ],
    [
      "stdout_budget",
      `process.stdout.write(Buffer.alloc(${limits.audit_output_bytes + 1}, 120));\n${hold}`,
      "budget_exceeded",
      "native_output_budget",
    ],
    [
      "stderr_budget",
      `process.stderr.write(Buffer.alloc(${limits.audit_output_bytes + 1}, 120));\n${hold}`,
      "budget_exceeded",
      "native_output_budget",
    ],
    [
      "combined_output_budget",
      `${drain}\nprocess.stdout.write(Buffer.alloc(${Math.floor(limits.audit_output_bytes / 2) + 1}, 120));\nprocess.stderr.write(Buffer.alloc(${Math.floor(limits.audit_output_bytes / 2) + 1}, 120));\nprocess.exitCode = 0;`,
      "budget_exceeded",
      "native_output_budget",
    ],
  ]) {
    stage = name;
    const anchor = await makePackage(name, body);
    expect(invoke(anchor), kind, reason);
  }
  stage = "unsupported_installation";
  expect(
    invoke(await makePackage(stage, "export {};", "0.0.0")),
    "unavailable",
    "native_runtime_unavailable",
  );
  stage = "missing_installation";
  expect(
    invoke(join(run, "missing-package.json")),
    "unavailable",
    "native_runtime_unavailable",
  );
  stage = "missing_node";
  expect(
    invoke(join(run, "unused-package.json"), {
      node: nativePath(join(run, "missing-node")),
    }),
    "unavailable",
    "native_spawn_failed",
  );
  stage = "expired_before_spawn";
  expect(
    invoke(join(run, "unused-package.json"), {
      node: nativePath(join(run, "missing-node")),
      timeout: 0,
    }),
    "unavailable",
    "native_deadline_exhausted",
  );
  // 子进程不继承 stdout/stderr：排空管道不能充当它已退出的证据。
  for (const tree of [false, true]) {
    stage = tree ? "timeout_stops_process_tree" : "timeout_stops_process";
    const marker = join(run, `${stage}.json`);
    const writeMarker = (data) =>
      `import { writeFileSync } from "node:fs";\nwriteFileSync(${JSON.stringify(nativePath(marker))}, JSON.stringify(${data}), { flag: "wx" });\n${hold}`;
    const nonce = randomUUID();
    const ready = `ready: ${JSON.stringify(nonce)}`;
    const body = tree
      ? `import { spawn } from "node:child_process";\nspawn(process.execPath, ["--input-type=module", "--eval", ${JSON.stringify(writeMarker(`{ parent: process.ppid, child: process.pid, ${ready} }`))}], { stdio: "ignore" });\n${hold}`
      : writeMarker(`{ parent: process.pid, ${ready} }`);
    const anchor = await makePackage(stage, body);
    expect(invoke(anchor), "unavailable", "native_timeout");
    const ids = JSON.parse(await readFile(marker, "utf8"));
    assert.equal(ids.ready, nonce);
    const states = {};
    for (const pid of [ids.parent, ...(tree ? [ids.child] : [])]) {
      assert(Number.isSafeInteger(pid) && pid > 1);
      let state;
      for (let attempt = 0; attempt < 20; attempt++) {
        if (inWsl) {
          const found = wsl(["ps", "-o", "stat=", "-p", String(pid)], [0, 1]);
          state = found.status === 1 ? "absent" : found.stdout.trim();
        } else {
          try {
            process.kill(pid, 0);
            state = "running";
          } catch (error) {
            assert.equal(error.code, "ESRCH");
            state = "absent";
          }
        }
        if (state === "absent" || state.startsWith("Z")) break;
        await delay(25);
      }
      assert(
        state === "absent" || state.startsWith("Z"),
        `${stage}: owned pid still running`,
      );
      states[pid === ids.parent ? "parent" : "child"] = state;
    }
    observations[stage] = {
      marker_observed: true,
      process_states_after_timeout: states,
    };
  }
  assert.deepEqual(await readFile(source), plain);
  console.log(
    JSON.stringify(
      {
        version: SUPPORTED_DSH_VERSION,
        node: process.version,
        rust_runtime: inWsl ? "wsl-linux" : process.platform,
        run: relative(repo, run),
        checks,
        observations,
        real_sdk_success_validated_here: false,
        production_advance_gate_integrated: false,
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
      detail: error.code === "ERR_ASSERTION" ? error.message : undefined,
    }),
  );
  process.exitCode = 1;
});

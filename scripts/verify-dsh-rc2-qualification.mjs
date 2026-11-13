#!/usr/bin/env node
/** 隔离候选验收：复用既有生产入口，串行构建；输出与每条命令 rc 永久保留在 target。 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { closeSync, openSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { createProtectedRun, runLoggedCommand } from "../tests/fixtures/dsh-v4/io-harness.mjs";
import { loadDshInstallation } from "./dsh-installed.mjs";

const repo = fileURLToPath(new URL("..", import.meta.url));
const { values } = parseArgs({ options: {
  "dsh-package": { type: "string" },
  "source-repo": { type: "string" },
  "candidate-file": { type: "string" },
  only: { type: "string" },
} });
const run = createProtectedRun(repo, "dsh-rc2-qualification-");
const target = resolve(repo, "target");
const binary = join(target, "debug", "examples", process.platform === "win32" ? "dsh_snapshot.exe" : "dsh_snapshot");
const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
const hashFile = (path) => digest(readFileSync(path));
const git = (...args) => {
  const result = spawnSync("git", args, { cwd: repo, encoding: "utf8" });
  assert.ifError(result.error);
  assert.equal(result.status, 0);
  return result.stdout.trim();
};
const treeDigest = (root) => {
  const entries = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name, "en"))) {
      const path = join(dir, entry.name);
      assert(!entry.isSymbolicLink(), "package digest refuses linked contents");
      if (entry.isDirectory()) walk(path);
      else {
        assert(entry.isFile());
        entries.push([relative(root, path).split("\\").join("/"), hashFile(path)]);
      }
    }
  };
  walk(root);
  return { files: entries.length, sha256: digest(JSON.stringify(entries)) };
};
const summary = {
  status: "running", node: process.version,
  run: relative(repo, run), commands: [],
};
const save = () => writeFileSync(join(run, "status.json"), JSON.stringify(summary, null, 2));
save();
console.log(JSON.stringify({ run: summary.run, status: "started" }));
try {
  assert(values["dsh-package"]);
  const installation = loadDshInstallation(values["dsh-package"]);
  assert.equal(installation.version, "0.2.2-rc.2");
  const moduleNames = ["@deepseek-ai/dsh-session", "@deepseek-ai/dsh-session-format-v3-to-v4", "@deepseek-ai/dsh-session-persistence-jsonl", "@deepseek-ai/dsh-token-meter"];
  const moduleDigests = () => Object.fromEntries(moduleNames.map((name) => [name, hashFile(installation.require.resolve(name))]));
  Object.assign(summary, {
    base: git("rev-parse", "HEAD"), branch: git("branch", "--show-current"),
    dsh_version: installation.version,
    package_tree: treeDigest(dirname(installation.anchor)),
    embedded_sources: Object.fromEntries(["scripts/dsh-installed.mjs", "scripts/dsh-native-admission.mjs", "scripts/dsh-snapshot-limits.json", "src/dsh_snapshot/native.rs"].map((path) => [path, hashFile(join(repo, path))])),
    module_entries: moduleDigests(),
  });
  if (values["source-repo"]) {
    summary.runtime_source_commit = git("-C", resolve(values["source-repo"]), "rev-parse", "candidate/dsh-rc2");
    assert.equal(summary.runtime_source_commit, "ddac96760b6edf8fa0fe17cbdcf0b512f4c12b2c");
  }
  if (values["candidate-file"]) summary.candidate_manifest_sha256 = hashFile(resolve(values["candidate-file"]));
  const packageArgs = ["--dsh-package", installation.anchor];
  const readerArgs = [...packageArgs, "--rust-reader", binary];
  const nativeArgs = [...readerArgs, "--native-node", process.execPath];
  const steps = [
    ["loader-sandbox", process.execPath, ["--test", "scripts/dsh-installed.test.mjs"]],
    ["script-syntax", process.execPath, ["--input-type=module", "--eval", 'import { readdirSync } from "node:fs"; import { spawnSync } from "node:child_process"; for (const name of readdirSync("scripts").filter((name) => name.endsWith(".mjs"))) { const result = spawnSync(process.execPath, ["--check", `scripts/${name}`], { stdio: "inherit" }); if (result.error || result.status !== 0) { process.exitCode = 1; console.error(name, result.status, result.error?.code); } }']],
    ["fmt", "cargo", ["fmt", "--all"]],
    ["build-snapshot", "cargo", ["build", "--example", "dsh_snapshot", "--features", "store,dsh-experimental"]],
    ["v4", process.execPath, ["scripts/verify-dsh-v4.mjs", ...packageArgs]],
    ["native-admission", process.execPath, ["scripts/verify-dsh-native-admission.mjs", ...nativeArgs]],
    ["physical-snapshot", process.execPath, ["scripts/verify-dsh-rust-snapshot.mjs", ...readerArgs]],
    ["native-snapshot", process.execPath, ["scripts/verify-dsh-rust-snapshot.mjs", ...nativeArgs]],
    ["source-capture", process.execPath, ["scripts/verify-dsh-rust-source-capture.mjs", ...nativeArgs]],
    ["normalize-store", process.execPath, ["scripts/verify-dsh-rust-normalize-store.mjs", ...packageArgs, "--binary", binary]],
    ["lib-tests", "cargo", ["test", "--lib", "--features", "store,dsh-experimental"]],
    ["check-bins-default", "cargo", ["check", "--bins"]],
    ["check-bins-store", "cargo", ["check", "--bins", "--features", "store"]],
    ["clippy", "cargo", ["clippy", "--all-targets", "--all-features", "--", "-D", "warnings"]],
    ["public-safe", "uv", ["run", "python", "scripts/check-public-safe.py"]],
    ["agents-budget", "uv", ["run", "python", "scripts/check-agents-budget.py"]],
  ];
  const selected = values.only ? new Set(values.only.split(",")) : new Set(steps.map(([name]) => name));
  for (const name of selected) assert(steps.some(([step]) => name === step), `unknown step ${name}`);
  summary.selected_steps = [...selected];
  summary.full_qualification = selected.size === steps.length;
  const binarySteps = ["native-admission", "physical-snapshot", "native-snapshot", "source-capture", "normalize-store"];
  const describeBinary = (builtByThisRun) => ({
    path: relative(repo, binary), sha256: hashFile(binary), bytes: statSync(binary).size, built_by_this_run: builtByThisRun,
  });
  // 子集可以复用已有 binary，但必须明确记录并绑定实际使用的字节。
  if (!selected.has("build-snapshot") && binarySteps.some((name) => selected.has(name))) {
    summary.binary = describeBinary(false);
  }
  save();
  for (const [name, command, args] of steps) {
    if (!selected.has(name)) continue;
    const stdout = openSync(join(run, `${name}.stdout.log`), "wx");
    let stderr;
    const started = new Date().toISOString();
    let result;
    try {
      stderr = openSync(join(run, `${name}.stderr.log`), "wx");
      result = await runLoggedCommand(command, args, {
        cwd: repo, env: { ...process.env, CARGO_TARGET_DIR: target },
        stdio: ["ignore", stdout, stderr], timeout: 30 * 60_000,
      });
    } finally {
      closeSync(stdout);
      if (stderr !== undefined) closeSync(stderr);
    }
    const record = { name, command, args, started, ended: new Date().toISOString(), rc: result.status, signal: result.signal, error: result.error?.code, cleanup_confirmed: result.cleanup_confirmed };
    summary.commands.push(record);
    save();
    console.log(JSON.stringify({ step: name, rc: record.rc, error: record.error }));
    if (name === "build-snapshot") {
      if (result.error || result.status !== 0) break;
      summary.binary = describeBinary(true);
      save();
    }
    // 收尾未确认时停止，避免后续步骤与遗留进程重叠执行。
    if (result.cleanup_confirmed === false) break;
  }
  summary.binary_binding_matches = summary.binary ? hashFile(binary) === summary.binary.sha256 : null;
  summary.embedded_source_binding_matches = Object.entries(summary.embedded_sources).every(([path, hash]) => hashFile(join(repo, path)) === hash);
  summary.installation_binding_matches = treeDigest(dirname(installation.anchor)).sha256 === summary.package_tree.sha256;
  const finalModules = moduleDigests();
  summary.module_entry_binding_matches = Object.entries(summary.module_entries).every(([name, hash]) => finalModules[name] === hash);
  summary.status = summary.commands.length === selected.size && summary.commands.every((item) => item.rc === 0 && !item.error) && summary.binary_binding_matches !== false && summary.embedded_source_binding_matches && summary.installation_binding_matches && summary.module_entry_binding_matches ? "passed" : "failed";
} catch (error) {
  summary.status = "failed";
  summary.error = { code: error.code, message: error.message };
} finally {
  save();
}
console.log(JSON.stringify({ run: summary.run, status: summary.status }));
process.exitCode = summary.status === "passed" ? 0 : 1;

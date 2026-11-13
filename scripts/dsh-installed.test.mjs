/** 安装约束与隔离目录回归；仅写 target 的独占合成目录，保留夹具。 */
import assert from "node:assert/strict";
import { mkdirSync, readFileSync, realpathSync, symlinkSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import test from "node:test";
import { createProtectedRun, runLoggedCommand } from "../tests/fixtures/dsh-v4/io-harness.mjs";
import { loadDshInstallation, SUPPORTED_DSH_VERSION } from "./dsh-installed.mjs";

const repo = fileURLToPath(new URL("..", import.meta.url));
const run = createProtectedRun(repo, "dsh-loader-test-");
const makePackage = (version, name = "@deepseek-ai/dsh") => {
  const root = join(run, `package-${version}-${name === "@deepseek-ai/dsh" ? "dsh" : "other"}`);
  mkdirSync(root);
  const anchor = join(root, "package.json");
  writeFileSync(anchor, JSON.stringify({ name, version }), { flag: "wx" });
  return anchor;
};

test("旧版接受范围与默认值保留，显式 rc.2 返回真实版本", () => {
  assert.equal(SUPPORTED_DSH_VERSION, "0.2.2-rc.1");
  for (const version of ["0.2.1-rc.5", "0.2.2-rc.1", "0.2.2-rc.2"]) {
    const anchor = makePackage(version);
    const installed = loadDshInstallation(anchor);
    assert.equal(installed.version, version);
    assert.equal(installed.anchor, realpathSync(anchor));
  }
  assert.throws(() => loadDshInstallation(makePackage("0.0.0")), /unsupported/);
  assert.throws(() => loadDshInstallation(makePackage("0.2.2-rc.2", "other")));
});

test("pnpm 形态的目录链接解析到实际安装，不用逻辑路径解析依赖", async () => {
  const root = join(run, "actual-package");
  mkdirSync(root);
  writeFileSync(join(root, "package.json"), JSON.stringify({ name: "@deepseek-ai/dsh", version: "0.2.2-rc.2" }), { flag: "wx" });
  const linked = join(run, "linked-package");
  symlinkSync(root, linked, "junction");
  const makeDependency = (parent, marker) => {
    const dependency = join(parent, "node_modules", "qualification-marker");
    mkdirSync(dependency, { recursive: true });
    writeFileSync(join(dependency, "package.json"), JSON.stringify({ name: "qualification-marker", exports: "./index.mjs" }), { flag: "wx" });
    writeFileSync(join(dependency, "index.mjs"), `export const marker = ${JSON.stringify(marker)};`, { flag: "wx" });
    return join(dependency, "index.mjs");
  };
  const actualEntry = makeDependency(root, "actual");
  makeDependency(run, "logical-fallback");
  const installed = loadDshInstallation(join(linked, "package.json"));
  assert.equal(installed.anchor, realpathSync(join(root, "package.json")));
  assert.equal(installed.version, "0.2.2-rc.2");
  assert.equal(installed.require.resolve("qualification-marker"), realpathSync(actualEntry));
  assert.equal((await installed.load("qualification-marker")).marker, "actual");
});

test("每轮目录独占；拒绝 target 链接和带路径的 prefix", () => {
  assert.notEqual(createProtectedRun(repo, "dsh-loader-test-"), run);
  assert.throws(() => createProtectedRun(repo, "../escape-"));
  const fixtureRepo = join(run, "fixture-repo");
  const linkedTarget = join(run, "linked-target");
  mkdirSync(fixtureRepo);
  mkdirSync(linkedTarget);
  symlinkSync(linkedTarget, join(fixtureRepo, "target"), "junction");
  assert.throws(() => createProtectedRun(fixtureRepo, "refused-"));
});

test("命令保留自身非零 rc 与启动错误", async () => {
  const options = { cwd: repo, stdio: "ignore", timeout: 5_000 };
  const failed = await runLoggedCommand(process.execPath, ["--eval", "process.exit(7)"], options);
  assert.equal(failed.status, 7);
  assert.equal(failed.error, undefined);
  const missing = await runLoggedCommand(join(run, "missing-executable"), [], options);
  assert.equal(missing.error.code, "ENOENT");
});

test("命令超时收尾覆盖直接子进程与后代", async () => {
  const pidFile = join(run, "timeout-grandchild.pid");
  const grandchild = `require("node:fs").writeFileSync(${JSON.stringify(pidFile)}, String(process.pid)); setInterval(() => {}, 1000);`;
  const parent = `require("node:child_process").spawn(process.execPath, ["--eval", ${JSON.stringify(grandchild)}], { stdio: "ignore" }); setInterval(() => {}, 1000);`;
  const result = await runLoggedCommand(process.execPath, ["--eval", parent], {
    cwd: repo, stdio: "ignore", timeout: 2_000,
  });
  assert.equal(result.error.code, "ETIMEDOUT");
  assert.equal(result.cleanup_confirmed, true);
  const pid = Number(readFileSync(pidFile, "utf8"));
  assert(Number.isSafeInteger(pid) && pid > 0);
  assert.throws(() => process.kill(pid, 0), (error) => error.code === "ESRCH");
});

const runQualification = (args, env = process.env) => {
  const result = spawnSync(process.execPath, ["scripts/verify-dsh-rc2-qualification.mjs", ...args], {
    cwd: repo, env, encoding: "utf8", timeout: 30_000,
  });
  assert.ifError(result.error);
  const first = JSON.parse(result.stdout.trim().split(/\r?\n/)[0]);
  return { result, status: JSON.parse(readFileSync(join(repo, first.run, "status.json"), "utf8")) };
};

test("验收预检失败也持久化 failed 终态", () => {
  const { result, status } = runQualification(["--dsh-package", join(run, "absent-package.json")]);
  assert.equal(result.status, 1);
  assert.equal(status.status, "failed");
  assert.equal(status.error.code, "ENOENT");
});

test("验收复核包目录外的依赖入口字节", () => {
  const root = join(run, "binding-fixture");
  const pkg = join(root, "package");
  mkdirSync(pkg, { recursive: true });
  const anchor = join(pkg, "package.json");
  writeFileSync(anchor, JSON.stringify({ name: "@deepseek-ai/dsh", version: "0.2.2-rc.2" }), { flag: "wx" });
  let mutableEntry;
  for (const name of ["dsh-session", "dsh-session-format-v3-to-v4", "dsh-session-persistence-jsonl", "dsh-token-meter"]) {
    const module = join(root, "node_modules", "@deepseek-ai", name);
    mkdirSync(module, { recursive: true });
    writeFileSync(join(module, "package.json"), JSON.stringify({ name: `@deepseek-ai/${name}`, exports: "./index.mjs" }), { flag: "wx" });
    const entry = join(module, "index.mjs");
    writeFileSync(entry, "export const marker = 1;", { flag: "wx" });
    mutableEntry = entry;
  }
  const hook = join(root, "mutate-entry.mjs");
  writeFileSync(hook, `import { writeFileSync } from "node:fs"; if (process.execArgv.includes("--eval")) writeFileSync(${JSON.stringify(mutableEntry)}, "export const marker = 2;");`, { flag: "wx" });
  const args = ["--dsh-package", anchor, "--only", "script-syntax"];
  const changed = runQualification(args, {
    ...process.env, NODE_OPTIONS: `${process.env.NODE_OPTIONS ?? ""} --import=${pathToFileURL(hook).href}`,
  });
  assert.equal(changed.result.status, 1);
  assert.equal(changed.status.status, "failed");
  assert.equal(changed.status.commands[0].rc, 0);
  assert.equal(changed.status.installation_binding_matches, true);
  assert.equal(changed.status.module_entry_binding_matches, false);
  const unchanged = runQualification(args);
  assert.equal(unchanged.result.status, 0);
  assert.equal(unchanged.status.status, "passed");
  assert.equal(unchanged.status.full_qualification, false);
  assert.equal(unchanged.status.module_entry_binding_matches, true);
  // 最终摘要读取失败必须覆盖 running 状态，而非只依赖子命令 rc。
  writeFileSync(hook, `import { unlinkSync } from "node:fs"; if (process.execArgv.includes("--eval")) unlinkSync(${JSON.stringify(mutableEntry)});`);
  const removed = runQualification(args, {
    ...process.env, NODE_OPTIONS: `${process.env.NODE_OPTIONS ?? ""} --import=${pathToFileURL(hook).href}`,
  });
  assert.equal(removed.result.status, 1);
  assert.equal(removed.status.status, "failed");
  assert.equal(removed.status.commands[0].rc, 0);
  assert.equal(removed.status.error.code, "MODULE_NOT_FOUND");
});

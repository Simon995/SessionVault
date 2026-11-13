/** 仅用于合成核验：跨进程回执、公开 I/O 屏障与有界等待。 */
import assert from "node:assert/strict";
import { fork, spawn, spawnSync } from "node:child_process";
import fs from "node:fs/promises";
import { lstatSync, mkdirSync, mkdtempSync, realpathSync } from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { isAbsolute, join, relative, resolve } from "node:path";

const DEADLINE_MS = 15_000;

export function inside(root, path) {
  const rel = relative(root, path);
  assert(
    rel !== "" &&
      !isAbsolute(rel) &&
      rel !== ".." &&
      !rel.startsWith("../") &&
      !rel.startsWith("..\\"),
  );
}

/** 仅复用父 target；每次创建独占新目录，拒绝链接且永不自动清理。 */
export function createProtectedRun(repo, prefix) {
  assert(/^[a-z0-9-]+-$/.test(prefix));
  const root = realpathSync(repo);
  const target = join(root, "target");
  try {
    mkdirSync(target);
  } catch (error) {
    assert.equal(error.code, "EEXIST");
  }
  const info = lstatSync(target);
  assert(info.isDirectory() && !info.isSymbolicLink());
  assert.equal(realpathSync(target), target);
  inside(root, realpathSync(target));
  const run = mkdtempSync(join(target, prefix));
  const created = lstatSync(run);
  assert(created.isDirectory() && !created.isSymbolicLink());
  assert.equal(realpathSync(run), run);
  inside(target, realpathSync(run));
  return run;
}

/** 超时时先终止进程树，再等待直接子进程退出；适用于输出直接写入文件的核验命令。 */
export async function runLoggedCommand(command, args, {
  timeout,
  cleanupTimeout = 10_000,
  ...options
}) {
  const child = spawn(command, args, {
    ...options,
    detached: process.platform !== "win32",
    windowsHide: true,
  });
  let failure;
  child.on("error", (error) => { failure = error; });
  const exited = new Promise((accept) => {
    child.once("close", (status, signal) => accept({ status, signal }));
  });
  const expired = Symbol("expired");
  let timer;
  const wait = (deadline) => new Promise((accept) => {
    timer = setTimeout(() => accept(expired), deadline);
  });
  try {
    const first = await Promise.race([exited, wait(timeout)]);
    clearTimeout(timer);
    if (first !== expired) return { ...first, error: failure };

    let treeStopped = false;
    if (child.pid && process.platform === "win32") {
      // 必须在父进程仍存活时按 PID 找树，不能先 child.kill()。
      const killed = spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
        stdio: "ignore", windowsHide: true, timeout: cleanupTimeout,
      });
      treeStopped = !killed.error && killed.status === 0;
    } else if (child.pid) {
      try {
        process.kill(-child.pid, "SIGKILL");
        treeStopped = true;
      } catch (error) {
        treeStopped = error.code === "ESRCH";
      }
    }
    const last = await Promise.race([exited, wait(cleanupTimeout)]);
    if (last === expired) child.unref();
    return {
      status: last === expired ? null : last.status,
      signal: last === expired ? null : last.signal,
      error: Object.assign(new Error("command timed out"), { code: "ETIMEDOUT" }),
      cleanup_confirmed: treeStopped && last !== expired,
    };
  } finally {
    clearTimeout(timer);
  }
}

export async function bounded(promise) {
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

export async function startPeer(run, packagePath) {
  const child = fork(
    new URL("./persistence-peer.mjs", import.meta.url),
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

/** 在真实 I/O 返回后、调用方获得结果前完成对照动作；finally 恢复方法。 */
export async function ioWindow(operation, path, onHit, action) {
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

/** 包装真正打开的句柄，只在指定大小的读取完成后设置屏障，不注入 reader 状态。 */
export async function fileReadWindow(path, size, onHit, action) {
  const original = fs.open;
  let hits = 0;
  fs.open = async (...args) => {
    const handle = await original(...args);
    if (resolve(args[0]) !== resolve(path)) return handle;
    const read = handle.read.bind(handle);
    handle.read = async (...readArgs) => {
      const value = await read(...readArgs);
      if (readArgs[0]?.length === size && value.bytesRead > 0) {
        hits += 1;
        await onHit(hits);
      }
      return value;
    };
    return handle;
  };
  syncBuiltinESMExports();
  try {
    const value = await bounded(Promise.resolve().then(action));
    assert(hits > 0, "the intended file-handle read was not reached");
    return { value, hits };
  } finally {
    fs.open = original;
    syncBuiltinESMExports();
  }
}

#!/usr/bin/env node
/** 并发核验的独立进程：仅接受本次 target 运行目录内的合成文件与原生句柄操作。 */
import assert from "node:assert/strict";
import {
  appendFile,
  lstat,
  readFile,
  realpath,
  rename,
  writeFile,
} from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { loadDshInstallation } from "../../../scripts/dsh-installed.mjs";

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

async function main() {
  assert(process.send);
  const { values } = parseArgs({
    options: {
      "dsh-package": { type: "string" },
      run: { type: "string" },
    },
  });
  const repo = await realpath(
    fileURLToPath(new URL("../../..", import.meta.url)),
  );
  const target = join(repo, "target");
  const run = resolve(values.run);
  assert(!(await lstat(target)).isSymbolicLink());
  assert(!(await lstat(run)).isSymbolicLink());
  inside(repo, await realpath(target));
  inside(target, await realpath(run));
  const { load } = loadDshInstallation(values["dsh-package"]);
  const { Context } = await load("@deepseek-ai/cordis");
  const backend = await load("@deepseek-ai/dsh-session-persistence-jsonl");
  let ctx;
  let service;
  let handle;
  let root;
  let serial = 0;
  const close = async () => {
    try {
      await handle?.close();
    } finally {
      handle = undefined;
      await ctx?.fiber.dispose();
      ctx = undefined;
      service = undefined;
    }
  };
  const checkedPath = async (path) => {
    assert(root);
    const absolute = resolve(path);
    inside(root, absolute);
    const parent = await realpath(dirname(absolute));
    inside(root, parent);
    try {
      const info = await lstat(absolute);
      assert(info.isFile() && !info.isSymbolicLink());
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    return absolute;
  };
  const perform = async (message) => {
    switch (message.op) {
      case "init": {
        await close();
        root = resolve(message.root);
        inside(run, root);
        assert(!(await lstat(root)).isSymbolicLink());
        inside(run, await realpath(root));
        ctx = new Context();
        await ctx
          .plugin(backend, { root, compression: message.compression })
          .await();
        service = ctx.get("sessionPersistence");
        assert(service);
        return { pid: process.pid };
      }
      case "append": {
        const path = await checkedPath(message.path);
        await appendFile(path, Buffer.from(message.bytes, "base64"));
        return { bytes: (await readFile(path)).length };
      }
      case "replace": {
        const path = await checkedPath(message.path);
        const staged = `${path}.peer-${serial++}`;
        await writeFile(staged, Buffer.from(message.bytes, "base64"), {
          flag: "wx",
        });
        await rename(staged, path);
        return {};
      }
      case "publish": {
        const path = await checkedPath(message.path);
        const staged = `${path}.peer-${serial++}`;
        await writeFile(staged, Buffer.from(message.bytes, "base64"), {
          flag: "wx",
        });
        // 发布使用排他的硬链接，目标若已存在就拒绝；暂存文件保留供核查。
        const { link } = await import("node:fs/promises");
        await link(staged, path);
        return {};
      }
      case "open": {
        assert(service && !handle);
        handle = await service.open(message.id, message.access);
        return {};
      }
      case "read": {
        assert(handle);
        return await handle.read();
      }
      case "native-append": {
        assert(handle);
        await handle.append(message.events);
        await handle.flush();
        return {};
      }
      case "close": {
        await close();
        return {};
      }
      case "stop": {
        await close();
        return {};
      }
      default:
        assert.fail("unknown fixture peer operation");
    }
  };
  let chain = Promise.resolve();
  process.on("message", (message) => {
    chain = chain.then(async () => {
      try {
        const result = await perform(message);
        process.send({ id: message.requestId, ok: true, result });
        if (message.op === "stop") process.disconnect();
      } catch (error) {
        process.send({
          id: message.requestId,
          ok: false,
          error: {
            name: error?.name ?? "peer_error",
            code: error?.code,
            prefix_shrank: /shrank below a previously observed prefix/.test(
              error.message,
            ),
          },
        });
      }
    });
  });
  process.send({ ready: true, pid: process.pid });
}

main().catch((error) => {
  console.error(JSON.stringify({ status: "peer_failed", kind: error?.name }));
  process.exitCode = 2;
});

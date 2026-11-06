#!/usr/bin/env node
/** 显式来源的只读实验入口；只输出摘要，不输出正文，不连接总库。 */
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { isAbsolute } from "node:path";
import { parseArgs } from "node:util";
import { loadDshInstallation } from "./dsh-installed.mjs";
import { createDshSnapshotReader } from "./dsh-snapshot-reader.mjs";

async function main() {
  const { values } = parseArgs({
    options: {
      "dsh-package": { type: "string" },
      root: { type: "string" },
      id: { type: "string" },
      compression: { type: "string" },
      "source-instance": { type: "string" },
    },
  });
  for (const key of [
    "dsh-package",
    "root",
    "id",
    "compression",
    "source-instance",
  ])
    assert(values[key]);
  assert(isAbsolute(values.root));
  const info = await fs.lstat(values.root);
  assert(info.isDirectory() && !info.isSymbolicLink());
  const root = await fs.realpath(values.root);
  const { load } = loadDshInstallation(values["dsh-package"]);
  const { Context } = await load("@deepseek-ai/cordis");
  const backend = await load("@deepseek-ai/dsh-session-persistence-jsonl");
  const ctx = new Context();
  try {
    await ctx
      .plugin(backend, { root, compression: values.compression })
      .await();
    const reader = await createDshSnapshotReader({
      dshPackage: values["dsh-package"],
      persistence: ctx.get("sessionPersistence"),
      compression: values.compression,
      sourceInstance: values["source-instance"],
    });
    const result = await reader.capture(values.id);
    console.log(
      JSON.stringify(
        result.kind === "verified_snapshot"
          ? { ...result, ...reader.describe(result), experimental: true }
          : result,
      ),
    );
    if (result.kind !== "verified_snapshot") process.exitCode = 2;
  } finally {
    await ctx.fiber.dispose();
  }
}
main().catch((error) => {
  console.error(
    JSON.stringify({
      kind: "unavailable",
      reason: "reader_failed",
      error: error.name,
    }),
  );
  process.exitCode = 1;
});

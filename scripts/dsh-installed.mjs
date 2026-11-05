/** 核查工具共用的安装版约束；只解析模块，不初始化 Host 或 profile。 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

export const SUPPORTED_DSH_VERSION = "0.2.1-rc.5";

export function loadDshInstallation(packageFile) {
  const anchor = resolve(packageFile);
  const manifest = JSON.parse(readFileSync(anchor, "utf8"));
  assert.equal(manifest.name, "@deepseek-ai/dsh");
  assert.equal(manifest.version, SUPPORTED_DSH_VERSION);
  const require = createRequire(anchor);
  return {
    anchor,
    require,
    load: (spec) => import(pathToFileURL(require.resolve(spec)).href),
  };
}

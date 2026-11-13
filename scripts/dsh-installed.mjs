/** 核查工具共用的安装版约束；只解析模块，不初始化 Host 或 profile。 */
import assert from "node:assert/strict";
import { readFileSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

export const SUPPORTED_DSH_VERSION = "0.2.2-rc.1";
export const SUPPORTED_DSH_VERSIONS = Object.freeze([
  "0.2.1-rc.5",
  "0.2.2-rc.1",
  "0.2.2-rc.2",
]);

export function loadDshInstallation(packageFile) {
  const anchor = realpathSync(resolve(packageFile));
  const manifest = JSON.parse(readFileSync(anchor, "utf8"));
  assert.equal(manifest.name, "@deepseek-ai/dsh");
  assert(
    SUPPORTED_DSH_VERSIONS.includes(manifest.version),
    `unsupported @deepseek-ai/dsh version: ${manifest.version}`,
  );
  const require = createRequire(anchor);
  return {
    anchor,
    version: manifest.version,
    require,
    load: (spec) => import(pathToFileURL(require.resolve(spec)).href),
  };
}

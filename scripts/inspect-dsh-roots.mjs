#!/usr/bin/env node
/**
 * 只读检查显式 home/profile 的静态日志根，不启动 Host，不执行 !!js。
 * node scripts/inspect-dsh-roots.mjs --dsh-package <package.json> --home <home>
 * 复用安装版的 patch 组合；不调用可能初始化或改写 profile 的加载入口。
 * 输出不含路径、配置正文、profile 名、包名或异常原文。
 */
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync, readdirSync, lstatSync } from "node:fs";
import { isAbsolute, join, resolve } from "node:path";
import { parseArgs } from "node:util";
import { loadDshInstallation } from "./dsh-installed.mjs";
const BACKEND = "@deepseek-ai/dsh-session-persistence-jsonl";
const BACKEND_ID = "session-persistence-jsonl";
const HOME_SESSIONS = /^dshHomePath\(\s*(['"])sessions\1\s*\)$/;

function expression(value) {
  return value !== null && typeof value === "object" && "__jsExpr" in value;
}

function rootShape(row, home, inheritedUnknown) {
  if (row.name !== BACKEND) return { status: "backend_replaced" };
  if (row.disabled === true) return { status: "disabled" };
  if (
    inheritedUnknown ||
    expression(row.disabled) ||
    (row.disabled !== undefined && row.disabled !== false)
  ) {
    return { status: "activation_unknown" };
  }
  const root = row.config?.root;
  if (expression(root)) {
    return {
      status: HOME_SESSIONS.test(root.__jsExpr)
        ? "home_sessions_expression"
        : "root_expression_unknown",
    };
  }
  if (typeof root !== "string" || root.length === 0) {
    return { status: "root_missing_or_invalid" };
  }
  if (!isAbsolute(root)) return { status: "relative_root_unresolved" };
  return {
    status:
      resolve(root) === resolve(home, "sessions")
        ? "literal_home_sessions"
        : "literal_other_root",
  };
}

function inspectRows(rows, home, inheritedUnknown = false) {
  const found = [];
  for (const row of rows) {
    if (row.id === BACKEND_ID || row.name === BACKEND) {
      found.push(rootShape(row, home, inheritedUnknown));
    }
    if (row.group && Array.isArray(row.config) && row.disabled !== true) {
      found.push(
        ...inspectRows(
          row.config,
          home,
          inheritedUnknown ||
            (row.disabled !== undefined && row.disabled !== false),
        ),
      );
    }
  }
  return found;
}

function selfTest(composeEntries) {
  const home = resolve("neutral-home");
  const original = [
    {
      insert: [
        {
          id: BACKEND_ID,
          name: BACKEND,
          config: { root: { __jsExpr: "dshHomePath('sessions')" } },
        },
      ],
    },
  ];
  const baseline = JSON.stringify(original);
  assert.equal(
    inspectRows(composeEntries([original]), home)[0].status,
    "home_sessions_expression",
  );
  const override = [{ id: BACKEND_ID, config: { root: join(home, "other") } }];
  assert.equal(
    inspectRows(composeEntries([original, override]), home)[0].status,
    "literal_other_root",
  );
  assert.equal(JSON.stringify(original), baseline);
  // 上游按整份 config 替换；遗漏 root 不能安静继承成默认值。
  assert.equal(
    inspectRows(
      composeEntries([
        original,
        [{ id: BACKEND_ID, config: { compression: true } }],
      ]),
      home,
    )[0].status,
    "root_missing_or_invalid",
  );
  assert.equal(
    inspectRows(
      composeEntries([original, [{ id: BACKEND_ID, disabled: true }]]),
      home,
    )[0].status,
    "disabled",
  );
  assert.equal(
    inspectRows(
      composeEntries([
        original,
        [
          {
            id: BACKEND_ID,
            config: { root: { __jsExpr: "process.env.OTHER_ROOT" } },
          },
        ],
      ]),
      home,
    )[0].status,
    "root_expression_unknown",
  );
  assert.equal(
    inspectRows(
      [
        {
          group: true,
          disabled: { __jsExpr: "someCondition" },
          config: composeEntries([original]),
        },
      ],
      home,
    )[0].status,
    "activation_unknown",
  );
  assert.deepEqual(
    inspectRows(
      [{ group: true, disabled: true, config: composeEntries([original]) }],
      home,
    ),
    [],
  );
  assert.equal(
    inspectRows([{ id: BACKEND_ID, name: "neutral-replacement" }], home)[0]
      .status,
    "backend_replaced",
  );
}

function plainDirectory(path) {
  const info = lstatSync(path);
  assert(info.isDirectory() && !info.isSymbolicLink());
}

let stage = "arguments";

async function main() {
  const { values } = parseArgs({
    options: {
      "dsh-package": { type: "string" },
      home: { type: "string" },
      profile: { type: "string", multiple: true, default: [] },
      patch: { type: "string", multiple: true, default: [] },
      "self-test": { type: "boolean", default: false },
    },
  });
  assert(values["dsh-package"]);
  const { anchor, load, version } = loadDshInstallation(values["dsh-package"]);
  const api = await load("@deepseek-ai/dsh-app-boot");
  selfTest(api.composeEntries);
  if (values["self-test"]) {
    console.log(JSON.stringify({ version, self_test: "passed" }));
    return;
  }
  assert(values.home);
  const home = resolve(values.home);
  plainDirectory(home);
  const profilesRoot = join(home, "profiles");
  plainDirectory(profilesRoot);
  const profiles = values.profile.length
    ? values.profile
    : readdirSync(profilesRoot, { withFileTypes: true })
        .filter(
          (entry) =>
            entry.isDirectory() &&
            !entry.isSymbolicLink() &&
            entry.name !== "node_modules",
        )
        .map((entry) => entry.name)
        .sort();
  const reads = new Map();
  const snapshot = (file) => {
    try {
      return createHash("sha256").update(readFileSync(file)).digest("hex");
    } catch (error) {
      if (error.code === "ENOENT") return null;
      throw error;
    }
  };
  const observe = (file, action) => {
    const before = snapshot(file);
    assert(!reads.has(file) || reads.get(file) === before);
    reads.set(file, before);
    return action();
  };
  const optional = (file) =>
    observe(file, () => api.loadOptionalPatches("dsh-inspect", file) ?? []);
  const required = (file) =>
    observe(file, () => api.loadOverlayPatches("dsh-inspect", file));
  const results = [];
  for (const name of profiles) {
    stage = "profile_path";
    const dir = api.resolveProfileDir(name, home);
    plainDirectory(dir);
    stage = "profile_manifest";
    const profileManifest = observe(join(dir, "package.json"), () =>
      api.readProfileManifest("dsh-inspect", dir),
    );
    const bundles = profileManifest.dsh?.profile?.bundles;
    assert(
      Array.isArray(bundles) &&
        bundles.every((item) => typeof item === "string"),
    );
    stage = "compatibility";
    const exemptionsFile = join(dir, api.PROFILE_COMPATIBILITY_FILENAME);
    const exemptions = observe(exemptionsFile, () =>
      api.readProfileVersionExemptions(dir),
    );
    const layers = [];
    let skipped = 0;
    for (const bundle of bundles) {
      try {
        const bundleDir = api.resolveBundleDir(
          "dsh-inspect",
          bundle,
          anchor,
          dir,
        );
        const bundleManifest = observe(join(bundleDir, "package.json"), () =>
          api.readProfileManifest("dsh-inspect", bundleDir),
        );
        const issue = api.evaluatePluginCompatibility(
          bundleManifest,
          exemptions,
        );
        if (issue !== undefined && !issue.exempted) {
          skipped++;
          continue;
        }
        assert(bundleManifest.dsh?.bundle);
        layers.push(
          ...api
            .bundlePatchPaths(bundleDir, bundleManifest.dsh.bundle)
            .map(required),
        );
      } catch {
        skipped++;
      }
    }
    stage = "profile_patch";
    layers.push(optional(join(dir, "cordis.patch.yml")));
    stage = "home_patch";
    layers.push(optional(join(home, "cordis.patch.yml")));
    layers.push(...values.patch.map((file) => required(resolve(file))));
    let warnings = 0;
    const rows = api.composeEntries(layers, () => warnings++);
    results.push({
      ordinal: results.length + 1,
      bundles: bundles.length,
      skipped_bundles: skipped,
      patch_warnings: warnings,
      backends: inspectRows(rows, home),
    });
  }
  const unchanged = [...reads].every(
    ([file, digest]) => snapshot(file) === digest,
  );
  const complete =
    unchanged &&
    results.length > 0 &&
    results.every(
      (item) =>
        item.skipped_bundles === 0 &&
        item.patch_warnings === 0 &&
        item.backends.length === 1 &&
        ["home_sessions_expression", "literal_home_sessions"].includes(
          item.backends[0].status,
        ),
    );
  console.log(
    JSON.stringify(
      {
        version,
        scope: "static_profile_composition",
        expression_evaluation: "not_performed",
        live_launch_environment: "not_observed",
        additional_launch_patches: "not_observed",
        explicit_overlays: values.patch.length,
        configuration_unchanged: unchanged,
        observed_files: reads.size,
        complete,
        profiles: results,
      },
      null,
      2,
    ),
  );
  if (!complete) process.exitCode = 2;
}

main().catch((error) => {
  // 配置异常可能夹带凭据或个人路径，不打印原始 message/stack。
  const message = error instanceof Error ? error.message : "";
  const kind =
    error instanceof TypeError
      ? "type_error"
      : error instanceof assert.AssertionError
        ? "assertion"
        : message.includes("failed to parse")
          ? "yaml_parse_error"
          : message.includes("must be a top-level YAML array")
            ? "not_patch_array"
            : message.includes("must be a mapping")
              ? "not_patch_mapping"
              : "read_or_parse_error";
  console.error(
    JSON.stringify({
      status: "inspection_failed",
      stage,
      kind,
      complete: false,
    }),
  );
  process.exitCode = 2;
});

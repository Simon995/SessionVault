#!/usr/bin/env node
/**
 * H.7: isolated cross-version contracts. Never opens the default store or keychain.
 * node scripts/verify-dsh-provider-compat.mjs --binary <new-svault> --old-binary <released-svault>
 *   --dsh-package <package.json> --example-binary <dsh_snapshot>
 * --store-contract-only skips provider wiring checks while the provider is being implemented.
 * Exit 0: supported contracts pass and no legacy loss observed; 1: a contract failed;
 * 2: supported contracts pass, but a legacy reader/rewriter loses unknown evidence fields.
 * Fixtures and logs remain under ${CARGO_TARGET_DIR:-target}/dsh-provider-compat/.
 * On WSL, use a Linux target directory so the fixture key can have mode 0600.
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { constants, zstdCompressSync } from "node:zlib";
import { withRetry } from "../tests/fixtures/dsh-v4/scenarios.mjs";
import { loadDshInstallation } from "./dsh-installed.mjs";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const opts = new Map();
let storeOnly = false;
for (let i = 2; i < process.argv.length; i += 1) {
  const arg = process.argv[i];
  if (arg === "--store-contract-only") storeOnly = true;
  else {
    assert(
      [
        "--binary",
        "--old-binary",
        "--dsh-package",
        "--example-binary",
      ].includes(arg),
      `unknown argument: ${arg}`,
    );
    assert(
      process.argv[i + 1] && !process.argv[i + 1].startsWith("--"),
      `missing value: ${arg}`,
    );
    opts.set(arg, path.resolve(process.argv[++i]));
  }
}
for (const name of [
  "--binary",
  "--old-binary",
  "--dsh-package",
  "--example-binary",
]) {
  assert(opts.has(name), `required: ${name}`);
  assert(fs.statSync(opts.get(name)).isFile(), `not a file: ${name}`);
}
const installation = loadDshInstallation(opts.get("--dsh-package"));
const fixtureCwd =
  process.platform === "win32" ? "C:/fixture/Repo" : "/fixture/repo/";
const fixtureCanonicalRoot =
  process.platform === "win32" ? "C:/fixture/repo" : "/fixture/repo";
const fixtureBase = JSON.parse(
  fs.readFileSync(
    path.join(repo, "tests/fixtures/dsh-v4/complete-turn.json"),
    "utf8",
  ),
);
const parent = path.join(
  path.resolve(repo, process.env.CARGO_TARGET_DIR || "target"),
  "dsh-provider-compat",
);
fs.mkdirSync(parent, { recursive: true });
const root = fs.mkdtempSync(path.join(parent, "run-"));
const binaryStamps = {};
for (const name of ["--binary", "--old-binary", "--example-binary"]) {
  const source = opts.get(name);
  const copy = path.join(root, `${name.slice(2)}${path.extname(source)}`);
  fs.copyFileSync(source, copy);
  binaryStamps[name.slice(2)] = {
    source,
    sha256: createHash("sha256").update(fs.readFileSync(copy)).digest("hex"),
  };
  // Pin this matrix to one build even if another session rebuilds the original path.
  opts.set(name, copy);
}
const dshRoot = path.join(root, "dsh-sessions");
const genericRoot = path.join(root, "generic");
const keyFile = path.join(root, "fixture.key");
const emptyBindingsFile = path.join(root, "empty-bindings.json");
fs.writeFileSync(
  emptyBindingsFile,
  JSON.stringify({ schemaVersion: 1, bindings: [] }),
);
// Matches the existing dsh_snapshot example's explicit synthetic StoreKey.
fs.writeFileSync(
  keyFile,
  Buffer.alloc(32, 0x5a).toString("base64").replace(/=+$/, ""),
  { mode: 0o600 },
);
fs.mkdirSync(dshRoot);
fs.mkdirSync(path.join(genericRoot, "sessions"), { recursive: true });
const env = {
  ...process.env,
  SVAULT_KEY_FILE: keyFile,
  SVAULT_SESSION_BINDINGS: emptyBindingsFile,
  SVAULT_JSONL_DIR: genericRoot,
  SVAULT_DSH_SESSIONS_DIR: dshRoot,
  SVAULT_DSH_HOME: "",
  SVAULT_DSH_NODE: process.execPath,
  SVAULT_DSH_PACKAGE: installation.anchor,
  CLAUDE_CONFIG_DIR: path.join(root, "empty-claude"),
  CODEX_HOME: path.join(root, "empty-codex"),
};
delete env.SVAULT_ACCEPTANCE_KEY;
const checks = [];
const incompatibilities = [];
let commandNumber = 0;
function check(name, condition, detail = {}) {
  checks.push({ name, ok: Boolean(condition), detail });
  assert(condition, `${name}: ${JSON.stringify(detail)}`);
}
function run(binary, args, overrides = {}, allowedStatuses = [0]) {
  const result = spawnSync(binary, args, {
    cwd: repo,
    env: { ...env, ...overrides },
    encoding: "utf8",
    windowsHide: true,
    timeout: 60_000,
    maxBuffer: 8 * 1024 * 1024,
  });
  const id = String(++commandNumber).padStart(3, "0");
  fs.writeFileSync(path.join(root, `${id}.stdout`), result.stdout || "");
  fs.writeFileSync(path.join(root, `${id}.stderr`), result.stderr || "");
  assert.ifError(result.error);
  assert(
    allowedStatuses.includes(result.status),
    `${path.basename(binary)} ${args[0]} exited ${result.status}: ${result.stderr}`,
  );
  const rows = (result.stdout || "")
    .split(/\r?\n/)
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  return { rows, id, status: result.status };
}
function version(binary) {
  const result = spawnSync(binary, ["--version"], {
    cwd: repo,
    env,
    encoding: "utf8",
    windowsHide: true,
    timeout: 10_000,
  });
  assert.ifError(result.error);
  assert.equal(result.status, 0);
  return result.stdout.trim();
}
const newVersion = version(opts.get("--binary"));
const oldVersion = version(opts.get("--old-binary"));
function writeSession(doc, name) {
  const dir = path.join(dshRoot, name);
  fs.mkdirSync(dir);
  fs.writeFileSync(
    path.join(dir, "session.v4.jsonl"),
    [
      JSON.stringify({ type: "session", ...doc.header }),
      ...doc.events.map((event) => JSON.stringify(event)),
    ].join("\n") + "\n",
  );
  return dir;
}
function pull(binary, db, projection = "current") {
  const result = run(binary, [
    "pull",
    "--since",
    "0",
    "--projection",
    projection,
    "--store",
    db,
  ]);
  assert.equal(result.rows.at(-1)?.kind, "pull_summary");
  const events = result.rows
    .filter((row) => row.kind === "pulled")
    .map((row) => row.event);
  const expected = readDb(
    db,
    (conn) =>
      conn
        .prepare(
          projection === "all"
            ? "SELECT COUNT(*) AS n FROM raw_events"
            : "SELECT COUNT(*) AS n FROM raw_events r JOIN current_head h ON h.source_type = r.source_type AND h.source_location = r.source_location AND h.source_path = r.source_path AND h.source_revision = r.source_revision AND h.projection_revision = r.projection_revision",
        )
        .get().n,
  );
  check(
    `pull_${result.id}_returns_every_stored_event`,
    events.length === expected && result.rows.at(-1).events === events.length,
    { returned: events.length, stored: expected },
  );
  return events;
}
function readDb(db, body) {
  const conn = new DatabaseSync(db, { readOnly: true });
  try {
    return body(conn);
  } finally {
    conn.close();
  }
}
function logicalDigest(db, projectionsOnly = false, sourcePath = null) {
  return readDb(db, (conn) => {
    const tables = conn
      .prepare(
        "SELECT name, sql FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
      )
      .all()
      .filter(
        (table) =>
          !projectionsOnly ||
          [
            "raw_events",
            "current_head",
            "projections",
            "projection_log",
            "applied_projections",
            "data_keys",
          ].includes(table.name),
      );
    const contents = tables.map((table) => [
      table,
      conn
        .prepare(`SELECT * FROM "${table.name.replaceAll('"', '""')}"`)
        .all()
        .filter((row) => sourcePath == null || row.source_path === sourcePath)
        .map((row) => {
          // Append of an empty generic batch refreshes this timestamp without moving the head.
          if (projectionsOnly && table.name === "current_head")
            delete row.updated_at;
          return JSON.stringify(row);
        })
        .sort(),
    ]);
    return createHash("sha256").update(JSON.stringify(contents)).digest("hex");
  });
}
function stateSignature(file, sourcePath = null) {
  const state = JSON.parse(fs.readFileSync(file, "utf8"));
  const selected =
    sourcePath == null ? state : state[`jsonl|local|${sourcePath}`];
  if (sourcePath != null)
    assert(selected, "fixture source cursor must exist before testing a hold");
  return JSON.stringify(selected, (_key, value) => {
    if (value && typeof value === "object" && !Array.isArray(value)) {
      return Object.fromEntries(
        Object.entries(value).sort(([a], [b]) => a.localeCompare(b)),
      );
    }
    return value;
  });
}
function sourceRows(db, source) {
  return readDb(db, (conn) =>
    conn
      .prepare("SELECT * FROM raw_events WHERE source_path = ? ORDER BY offset")
      .all(source),
  );
}
function sourceHead(db, source) {
  return readDb(db, (conn) =>
    conn
      .prepare(
        "SELECT source_revision, projection_revision FROM current_head WHERE source_type = 'jsonl' AND source_location = 'local' AND source_path = ?",
      )
      .get(source),
  );
}
function evidence(events) {
  return events
    .filter((event) => event.usage_evidence)
    .map((event) => event.usage_evidence);
}
function scan(binary, db, state, overrides = {}, allowedStatuses = [0]) {
  return run(
    binary,
    [
      "scan-all",
      "--only",
      "jsonl",
      "--write-store",
      "--store",
      db,
      "--state",
      state,
    ],
    overrides,
    allowedStatuses,
  );
}
function appendTurn(doc, dir, turn) {
  const seq = doc.events.length;
  const time = doc.events.at(-1).time + 10;
  const suffix = [
    { type: "turn/start", seq, time, data: { turn } },
    {
      type: "user/message",
      seq: seq + 1,
      time: time + 1,
      surfaceOp: "append",
      data: {
        id: `suffix-u-${turn}`,
        role: "user",
        source: { kind: "user" },
        content: [{ type: "text", text: "neutral guarded suffix" }],
      },
    },
    {
      type: "turn/end",
      seq: seq + 2,
      time: time + 2,
      data: { turn, reason: { kind: "completed" } },
    },
  ];
  fs.appendFileSync(
    path.join(dir, "session.v4.jsonl"),
    suffix.map((event) => JSON.stringify(event)).join("\n") + "\n",
  );
  doc.events.push(...suffix);
}
function binding(sourcePath, sessionId, temporary) {
  return {
    schemaVersion: 1,
    owner: { namespace: "tumechat.user", id: "u" },
    host: { namespace: "tumechat.host", id: "h" },
    session: { namespace: "tumechat.session", id: sessionId },
    nativeSource: {
      backend: "dsh-native",
      sourceLocation: "local",
      sourcePath,
      nativeSessionId: sessionId,
    },
    temporary,
    readMemory: true,
    recordedAt: "2026-11-10T00:00:00Z",
  };
}
function installBindings(db, registry) {
  fs.writeFileSync(
    path.join(path.dirname(db), "session_bindings.json"),
    JSON.stringify(registry),
  );
}

let failed = null;
try {
  const exactDoc = withRetry(fixtureBase);
  exactDoc.header.id = "exact";
  // Two spellings of a neutral root let the real released reattribute command rewrite a generation.
  exactDoc.header.cwd = fixtureCwd;
  const exactDir = writeSession(exactDoc, "exact");
  const partialDoc = structuredClone(fixtureBase);
  partialDoc.header.id = "partial";
  partialDoc.header.cwd = fixtureCwd;
  partialDoc.events.find(
    (event) => event.type === "assistant/message",
  ).data.usage = {
    inputTokens: 10,
    outputTokens: 5,
    totalTokens: 20,
  };
  const partialDir = writeSession(partialDoc, "partial");
  const db = path.join(root, "total_store.db");
  for (const [dir, instance] of [
    [exactDir, "compat-exact"],
    [partialDir, "compat-partial"],
  ]) {
    run(opts.get("--example-binary"), [
      "--session-dir",
      dir,
      "--source-instance",
      instance,
      "--compression",
      "none",
      "--node",
      process.execPath,
      "--dsh-package",
      installation.anchor,
      "--store-db",
      db,
      "--state-file",
      path.join(root, `${instance}.json`),
      "--positions",
    ]);
  }
  const baseline = pull(opts.get("--binary"), db);
  const baselineEvidence = evidence(baseline);
  check("new_evidence_survives_encrypted_store", baselineEvidence.length >= 3, {
    evidence_records: baselineEvidence.length,
  });
  check(
    "exact_usage_and_unknown_cache_are_distinct",
    baseline.some((event) => event.event_type === "usage" && event.usage) &&
      baseline.some(
        (event) =>
          event.event_type !== "usage" &&
          event.usage == null &&
          event.usage_evidence?.availability ===
            "exact_total_with_unknown_cache" &&
          event.usage_evidence.exact_total_tokens === 20 &&
          event.usage_evidence.cache_read_tokens == null &&
          event.usage_evidence.cache_write_tokens == null,
      ),
  );

  const digestBeforeRead = logicalDigest(db);
  const oldRead = pull(opts.get("--old-binary"), db);
  check(
    "released_reader_accepts_jsonl_append_log",
    oldRead.length === baseline.length &&
      oldRead.every(
        (event) =>
          event.source_type === "jsonl" && event.source_mode === "append_log",
      ),
    { events: oldRead.length },
  );
  check(
    "released_reader_never_turns_partial_evidence_into_zero_usage",
    oldRead.some(
      (event) =>
        event.source_session_id === "partial" &&
        event.event_type === "message" &&
        event.actor === "assistant" &&
        event.usage == null,
    ) &&
      !oldRead.some(
        (event) =>
          event.source_session_id === "partial" && event.event_type === "usage",
      ),
  );
  run(opts.get("--old-binary"), [
    "sessions-read",
    "--session",
    `jsonl/local/${partialDir}/partial`,
    "--store",
    db,
  ]);
  run(opts.get("--old-binary"), ["changes", "--since-seq", "0", "--store", db]);
  check(
    "released_reads_do_not_mutate_store_or_projection",
    logicalDigest(db) === digestBeforeRead,
  );
  check("released_wire_drops_new_evidence", evidence(oldRead).length === 0, {
    before: baselineEvidence.length,
    released_wire: evidence(oldRead).length,
  });
  incompatibilities.push({
    kind: "legacy_reader_omits_unknown_fields",
    command: "pull / sessions-read",
    store_unchanged: true,
    evidence_records_omitted: baselineEvidence.length,
  });

  const exactRows = sourceRows(db, exactDir);
  const exactHead = sourceHead(db, exactDir);
  const genericFile = path.join(genericRoot, "sessions", "legacy.jsonl");
  fs.writeFileSync(
    genericFile,
    JSON.stringify({
      role: "user",
      content: "neutral legacy writer fixture",
      session_id: "legacy",
      timestamp: "2026-11-10T00:00:00Z",
    }) + "\n",
  );
  const legacyState = path.join(root, "legacy-cursors.json");
  scan(opts.get("--old-binary"), db, legacyState);
  const afterOldWrite = pull(opts.get("--binary"), db);
  check(
    "released_writer_new_reader",
    afterOldWrite.some((event) => event.source_session_id === "legacy"),
  );
  check(
    "released_normal_append_preserves_new_evidence_and_head",
    JSON.stringify(evidence(afterOldWrite)) ===
      JSON.stringify(baselineEvidence) &&
      JSON.stringify(sourceRows(db, exactDir)) === JSON.stringify(exactRows) &&
      JSON.stringify(sourceHead(db, exactDir)) === JSON.stringify(exactHead),
  );
  scan(opts.get("--old-binary"), db, legacyState);
  check(
    "released_repeat_write_is_idempotent",
    pull(opts.get("--binary"), db).length === afterOldWrite.length,
  );
  const oldOnlyDb = path.join(root, "legacy-created.db");
  scan(
    opts.get("--old-binary"),
    oldOnlyDb,
    path.join(root, "old-only-cursors.json"),
  );
  check(
    "new_reader_accepts_store_created_by_release",
    pull(opts.get("--binary"), oldOnlyDb).length === 1,
  );

  if (!storeOnly) {
    const disabled = run(
      opts.get("--binary"),
      ["scan-all", "--only", "jsonl", "--stateless", "--store", oldOnlyDb],
      {
        SVAULT_DSH_SESSIONS_DIR: "",
        SVAULT_DSH_HOME: "",
        SVAULT_JSONL_DIR: "",
      },
    );
    check(
      "no_explicit_root_adds_no_sources",
      disabled.rows.at(-1)?.sources === 0,
      disabled.rows.at(-1),
    );
    const compressedDoc = structuredClone(fixtureBase);
    compressedDoc.header.id = "compressed";
    const compressedDir = path.join(dshRoot, "compressed");
    fs.mkdirSync(compressedDir);
    const packFrame = (text) =>
      zstdCompressSync(Buffer.from(text), {
        params: { [constants.ZSTD_c_checksumFlag]: 1 },
      });
    const headerLine =
      JSON.stringify({ type: "session", ...compressedDoc.header }) + "\n";
    const bodyLines =
      compressedDoc.events.map((event) => JSON.stringify(event)).join("\n") +
      "\n";
    // Native DSH writes the session header in its own frame.
    fs.writeFileSync(
      path.join(compressedDir, "session.v4.jsonl.zstd"),
      Buffer.concat([packFrame(headerLine), packFrame(bodyLines)]),
    );
    fs.writeFileSync(
      path.join(compressedDir, "session.v3.jsonl.zstd"),
      Buffer.concat([
        packFrame(headerLine.replace('"version":4', '"version":3')),
        packFrame(bodyLines),
      ]),
    );
    const discovered = run(opts.get("--binary"), ["discover"]);
    const sessionDirs = [exactDir, partialDir, compressedDir];
    const dshSources = discovered.rows.filter(
      (row) =>
        row.kind === "source" && sessionDirs.includes(path.resolve(row.path)),
    );
    check(
      "dsh_discovery_exposes_existing_append_log_mode",
      dshSources.length === 3 &&
        dshSources.every(
          (row) =>
            row.source_type === "jsonl" && row.source_mode === "append_log",
        ),
      { sources: dshSources.length },
    );
    check(
      "compressed_v4_and_older_v3_discover_one_logical_source",
      dshSources.filter((row) => path.resolve(row.path) === compressedDir)
        .length === 1,
    );
    const providerDb = path.join(root, "provider.db");
    const providerState = path.join(root, "provider-cursors.json");
    scan(opts.get("--binary"), providerDb, providerState);
    const providerEvents = pull(opts.get("--binary"), providerDb);
    check(
      "formal_provider_persists_full_evidence",
      evidence(providerEvents).length === baselineEvidence.length + 1 &&
        providerEvents.some(
          (event) =>
            event.source_session_id === "partial" &&
            event.usage_evidence?.exact_total_tokens === 20,
        ) &&
        providerEvents.some(
          (event) =>
            event.source_session_id === "compressed" && event.usage_evidence,
        ),
    );
    const digestBeforeRepeat = logicalDigest(providerDb, true);
    scan(opts.get("--binary"), providerDb, providerState);
    check(
      "formal_provider_repeat_does_not_reproject",
      logicalDigest(providerDb, true) === digestBeforeRepeat,
    );
    const exactPlain = path.join(exactDir, "session.v4.jsonl");
    const exactPacked = `${exactPlain}.zstd`;
    const exactBytes = fs.readFileSync(exactPlain);
    const headerEnd = exactBytes.indexOf(10) + 1;
    fs.writeFileSync(
      exactPacked,
      Buffer.concat([
        packFrame(exactBytes.subarray(0, headerEnd)),
        packFrame(exactBytes.subarray(headerEnd)),
      ]),
    );
    fs.unlinkSync(exactPlain);
    const packedScan = scan(opts.get("--binary"), providerDb, providerState);
    check(
      "encoding_switch_to_zstd_preserves_the_logical_source",
      !packedScan.rows.some(
        (row) => row.kind === "store_held" && row.source_path === exactDir,
      ) &&
        JSON.parse(stateSignature(providerState, exactDir)).dsh_state
          .compression === "zstd" &&
        JSON.stringify(pull(opts.get("--binary"), providerDb)) ===
          JSON.stringify(providerEvents),
    );
    fs.writeFileSync(exactPlain, exactBytes);
    fs.unlinkSync(exactPacked);
    const unpackedScan = scan(opts.get("--binary"), providerDb, providerState);
    check(
      "encoding_switch_back_to_plain_preserves_the_logical_source",
      !unpackedScan.rows.some(
        (row) => row.kind === "store_held" && row.source_path === exactDir,
      ) &&
        JSON.parse(stateSignature(providerState, exactDir)).dsh_state
          .compression === "none" &&
        JSON.stringify(pull(opts.get("--binary"), providerDb)) ===
          JSON.stringify(providerEvents),
    );
    scan(
      opts.get("--old-binary"),
      providerDb,
      path.join(root, "provider-legacy-cursors.json"),
    );
    scan(opts.get("--binary"), providerDb, providerState);
    check(
      "new_and_released_writers_coexist_idempotently",
      JSON.stringify(pull(opts.get("--binary"), providerDb)) ===
        JSON.stringify(providerEvents),
    );
    const sharedDb = path.join(root, "shared-state.db");
    const sharedState = path.join(root, "shared-cursors.json");
    scan(opts.get("--binary"), sharedDb, sharedState);
    const sharedInitial = pull(opts.get("--binary"), sharedDb);
    const sharedHeadBeforeOld = sourceHead(sharedDb, exactDir);
    scan(opts.get("--old-binary"), sharedDb, sharedState);
    const sharedOldState = JSON.parse(fs.readFileSync(sharedState, "utf8"));
    check(
      "released_shared_cursor_save_omits_dsh_state",
      !Object.values(sharedOldState).some((cursor) => cursor.dsh_state),
    );
    scan(opts.get("--binary"), sharedDb, sharedState);
    const sharedUnchanged = pull(opts.get("--binary"), sharedDb);
    const sharedHeadAfterUnchanged = sourceHead(sharedDb, exactDir);
    check(
      "shared_cursor_unchanged_source_preserves_current_events",
      JSON.stringify(sharedUnchanged) === JSON.stringify(sharedInitial),
      {
        head_before: sharedHeadBeforeOld,
        head_after: sharedHeadAfterUnchanged,
      },
    );
    const priorSharedEvidence = evidence(
      sharedUnchanged.filter((event) => event.source_path === exactDir),
    );
    scan(opts.get("--old-binary"), sharedDb, sharedState);
    const oldUserText = exactDoc.events.find(
      (event) => event.type === "user/message",
    ).data.content[0].text;
    const rewrittenText = "rewritten neutral source prefix";
    exactDoc.events.find(
      (event) => event.type === "user/message",
    ).data.content[0].text = rewrittenText;
    fs.writeFileSync(
      path.join(exactDir, "session.v4.jsonl"),
      [
        JSON.stringify({ type: "session", ...exactDoc.header }),
        ...exactDoc.events.map((event) => JSON.stringify(event)),
      ].join("\n") + "\n",
    );
    scan(opts.get("--binary"), sharedDb, sharedState);
    const sharedRewritten = pull(opts.get("--binary"), sharedDb);
    const sharedHeadAfterRewrite = sourceHead(sharedDb, exactDir);
    check(
      "shared_cursor_lost_state_does_not_hide_a_rewritten_prefix",
      sharedRewritten.some(
        (event) =>
          event.source_path === exactDir && event.content === rewrittenText,
      ),
      {
        head_before: sharedHeadAfterUnchanged,
        head_after: sharedHeadAfterRewrite,
      },
    );
    const sharedAll = pull(opts.get("--binary"), sharedDb, "all");
    check(
      "shared_cursor_rewrite_opens_a_source_revision_and_retains_prior_bytes",
      sharedHeadAfterRewrite.source_revision >
        sharedHeadAfterUnchanged.source_revision &&
        sharedAll.some(
          (event) =>
            event.source_path === exactDir && event.content === oldUserText,
        ),
      {
        head_before: sharedHeadAfterUnchanged,
        head_after: sharedHeadAfterRewrite,
        current_events: sharedRewritten.length,
        all_generations_events: sharedAll.length,
      },
    );
    check(
      "shared_cursor_rewrite_preserves_usage_evidence",
      JSON.stringify(
        evidence(
          sharedRewritten.filter((event) => event.source_path === exactDir),
        ),
      ) === JSON.stringify(priorSharedEvidence),
    );

    exactDoc.events.find(
      (event) => event.type === "user/message",
    ).data.content[0].text = oldUserText;
    fs.writeFileSync(
      path.join(exactDir, "session.v4.jsonl"),
      [
        JSON.stringify({ type: "session", ...exactDoc.header }),
        ...exactDoc.events.map((event) => JSON.stringify(event)),
      ].join("\n") + "\n",
    );
    scan(opts.get("--binary"), sharedDb, sharedState);
    const returnedToA = pull(opts.get("--binary"), sharedDb);
    const returnedHead = sourceHead(sharedDb, exactDir);
    check(
      "returning_to_a_historical_target_moves_the_current_head",
      returnedHead.source_revision > sharedHeadAfterRewrite.source_revision &&
        returnedToA.some(
          (event) =>
            event.source_path === exactDir && event.content === oldUserText,
        ),
    );
    scan(opts.get("--binary"), sharedDb, sharedState);
    check(
      "historical_target_crash_replay_remains_idempotent",
      JSON.stringify(sourceHead(sharedDb, exactDir)) ===
        JSON.stringify(returnedHead),
    );

    const streamDb = path.join(root, "stream-before-store.db");
    const streamState = path.join(root, "stream-before-store.json");
    scan(opts.get("--binary"), streamDb, streamState);
    appendTurn(exactDoc, exactDir, 2);
    run(opts.get("--binary"), [
      "scan-all",
      "--only",
      "jsonl",
      "--profile",
      "full",
      "--store",
      streamDb,
      "--state",
      streamState,
    ]);
    scan(opts.get("--binary"), streamDb, streamState);
    check(
      "streaming_cursor_does_not_hide_an_unstored_suffix",
      pull(opts.get("--binary"), streamDb).some(
        (event) =>
          event.source_path === exactDir &&
          event.content === "neutral guarded suffix",
      ),
    );
    const streamedRewrite = "neutral prefix read before storage";
    exactDoc.events.find(
      (event) => event.type === "user/message",
    ).data.content[0].text = streamedRewrite;
    fs.writeFileSync(
      path.join(exactDir, "session.v4.jsonl"),
      [
        JSON.stringify({ type: "session", ...exactDoc.header }),
        ...exactDoc.events.map((event) => JSON.stringify(event)),
      ].join("\n") + "\n",
    );
    run(opts.get("--binary"), [
      "scan-all",
      "--only",
      "jsonl",
      "--profile",
      "full",
      "--store",
      streamDb,
      "--state",
      streamState,
    ]);
    scan(opts.get("--binary"), streamDb, streamState);
    check(
      "streaming_cursor_does_not_hide_an_unstored_rewrite",
      pull(opts.get("--binary"), streamDb).some(
        (event) =>
          event.source_path === exactDir && event.content === streamedRewrite,
      ),
    );

    appendTurn(partialDoc, partialDir, 2);
    const beforeMissingRuntime = logicalDigest(providerDb, true);
    const stateBeforeMissingRuntime = stateSignature(providerState);
    for (const missing of ["SVAULT_DSH_NODE", "SVAULT_DSH_PACKAGE"]) {
      const held = scan(
        opts.get("--binary"),
        providerDb,
        providerState,
        { [missing]: "" },
        [0, 1, 2],
      );
      check(
        `${missing}_missing_freezes_store_and_cursor`,
        logicalDigest(providerDb, true) === beforeMissingRuntime &&
          stateSignature(providerState) === stateBeforeMissingRuntime,
      );
      check(
        `${missing}_missing_reports_failure_or_hold`,
        held.status !== 0 ||
          held.rows.some(
            (row) =>
              row.kind === "store_held" ||
              (row.kind === "source_report" && row.status === "error"),
          ),
      );
    }
    scan(opts.get("--binary"), providerDb, providerState);
    check(
      "restored_runtime_consumes_the_previously_held_suffix",
      pull(opts.get("--binary"), providerDb).length > providerEvents.length,
    );
    const beforeFutureGeneration = logicalDigest(
      providerDb,
      true,
      compressedDir,
    );
    const stateBeforeFutureGeneration = stateSignature(
      providerState,
      compressedDir,
    );
    fs.copyFileSync(
      path.join(compressedDir, "session.v4.jsonl.zstd"),
      path.join(compressedDir, "session.v5.jsonl.zstd"),
    );
    const futureDiscovered = run(opts.get("--binary"), ["discover"]);
    check(
      "future_generation_keeps_the_same_logical_source",
      futureDiscovered.rows.filter(
        (row) =>
          row.kind === "source" && path.resolve(row.path) === compressedDir,
      ).length === 1,
    );
    scan(opts.get("--binary"), providerDb, providerState);
    check(
      "future_generation_holds_prior_source_projection_keys_and_cursor",
      logicalDigest(providerDb, true, compressedDir) ===
        beforeFutureGeneration &&
        stateSignature(providerState, compressedDir) ===
          stateBeforeFutureGeneration,
    );

    const guardedDir = path.join(root, "guarded");
    fs.mkdirSync(guardedDir);
    const guardedDb = path.join(guardedDir, "store.db");
    const guardedState = path.join(guardedDir, "cursors.json");
    const temporaryBindings = [binding(exactDir, "exact", true)];
    const guardedEnv = {
      SVAULT_JSONL_DIR: "",
      SVAULT_SESSION_BINDINGS: path.join(guardedDir, "session_bindings.json"),
    };
    installBindings(guardedDb, {
      schemaVersion: 1,
      bindings: temporaryBindings,
      pendingSources: [],
    });
    scan(opts.get("--binary"), guardedDb, guardedState, guardedEnv);
    check(
      "temporary_binding_excludes_events",
      pull(opts.get("--binary"), guardedDb).every(
        (event) => event.source_session_id !== "exact",
      ),
    );
    const beforeHold = logicalDigest(guardedDb, true);
    const cursorBeforeHold = stateSignature(guardedState);
    // Supply a real new suffix: a frozen no-op alone would not establish admission safety.
    appendTurn(partialDoc, partialDir, 3);
    installBindings(guardedDb, {
      schemaVersion: 1,
      bindings: temporaryBindings,
      pendingSources: [
        {
          backend: "dsh-native",
          sourceLocation: "local",
          sourcePath: partialDir,
          reason: "spawning",
        },
      ],
    });
    scan(opts.get("--binary"), guardedDb, guardedState, guardedEnv);
    check(
      "pending_source_freezes_store_and_cursor_despite_new_suffix",
      logicalDigest(guardedDb, true) === beforeHold &&
        stateSignature(guardedState) === cursorBeforeHold,
    );
    // A malformed binding registry is also UnknownHold, rather than an empty registry.
    fs.writeFileSync(
      path.join(guardedDir, "session_bindings.json"),
      "{malformed",
    );
    scan(opts.get("--binary"), guardedDb, guardedState, guardedEnv);
    check(
      "malformed_binding_freezes_store_and_cursor",
      logicalDigest(guardedDb, true) === beforeHold &&
        stateSignature(guardedState) === cursorBeforeHold,
    );
  }

  // Last: destructive legacy behaviour is exercised only on this synthetic database.
  const conn = new DatabaseSync(db);
  conn
    .prepare(
      "INSERT OR REPLACE INTO project_root_registry (root_key, root_path, root_source, first_seen_ms, last_seen_ms) VALUES (?, ?, 'configured', 0, 0)",
    )
    .run(fixtureCanonicalRoot.toLowerCase(), fixtureCanonicalRoot);
  conn.close();
  const beforeRewrite = pull(opts.get("--binary"), db);
  const rewrite = run(opts.get("--old-binary"), [
    "reattribute",
    "--apply",
    "--store",
    db,
  ]);
  const summary = rewrite.rows.find(
    (row) => row.kind === "reattribute_summary",
  );
  check(
    "released_reattribute_really_rewrites_fixture",
    summary?.sources_rewritten === 2,
    summary,
  );
  const afterRewrite = pull(opts.get("--binary"), db);
  check(
    "legacy_rewrite_drops_unknown_fields",
    afterRewrite.length === beforeRewrite.length &&
      evidence(afterRewrite).length === 0,
    {
      events_before: beforeRewrite.length,
      events_after: afterRewrite.length,
      evidence_before: evidence(beforeRewrite).length,
      evidence_after: evidence(afterRewrite).length,
    },
  );
  const allAfterRewrite = pull(opts.get("--binary"), db, "all");
  check(
    "legacy_rewrite_removed_the_evidence_bearing_old_generation",
    evidence(allAfterRewrite).length === 0,
  );
  incompatibilities.push({
    kind: "legacy_rewrite_drops_unknown_fields",
    command: "reattribute --apply",
    old_generation_removed: true,
    evidence_records_lost: evidence(beforeRewrite).length,
    required_action:
      "upgrade content-rewriting writers before running this operation",
  });
} catch (error) {
  failed = String(error.stack || error);
}
const report = {
  kind: "dsh_provider_compatibility",
  status: failed
    ? "contract_failed"
    : incompatibilities.length
      ? "limited_compatibility"
      : "compatible",
  full_compatibility: !failed && incompatibilities.length === 0,
  supported_contracts_passed: !failed,
  provider_checks_run: !storeOnly,
  new_version: newVersion,
  old_version: oldVersion,
  binaries: binaryStamps,
  dsh_version: installation.version,
  fixture_dir: root,
  checks,
  incompatibilities,
  failure: failed,
};
fs.writeFileSync(
  path.join(root, "report.json"),
  JSON.stringify(report, null, 2),
);
console.log(JSON.stringify(report, null, 2));
process.exitCode = failed ? 1 : incompatibilities.length ? 2 : 0;

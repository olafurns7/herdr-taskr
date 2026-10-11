#!/usr/bin/env node
// Dependency-free check of taskr-push.ts with a mocked pi API: node adapters/pi/check.mjs [TASKR]
// TASKR defaults to target/debug/taskr. Node (v22.18+/v24) loads the extension by
// stripping its erasable TypeScript. A real `taskr mcp --channel` child runs on a
// scratch ledger (scratch HOME, TASKR_DB and TASKR_TMP_BASE); a fake child covers
// framing and lifecycle edges. This proves translation and cleanup only: not pi's
// loader, its busy queue or any provider turn.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const taskr = path.resolve(process.argv[2] ?? path.join(here, "../../target/debug/taskr"));
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "taskr-push-check-"));
for (const k of ["TASKR_TASK", "TASKR_LAUNCH", "TASKR_BIN", "TASKR_FROZEN_NOW", "TASKR_FORMAT", "HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH"]) {
  delete process.env[k];
}
Object.assign(process.env, {
  HOME: scratch,
  TASKR_DB: path.join(scratch, "ledger.db"),
  TASKR_TMP_BASE: path.join(scratch, "tmp"),
  TMPDIR: scratch,
});

const GRACE_MS = 5000; // the shim's kill grace
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(what, ok, ms = 20000) {
  const start = Date.now();
  while (!ok()) {
    if (Date.now() - start > ms) throw new Error(`timed out waiting for ${what}`);
    await sleep(25);
  }
}
function cli(env, ...args) {
  const r = spawnSync(taskr, ["--json", ...args], { encoding: "utf8", env: { ...process.env, ...env } });
  assert.equal(r.status, 0, `taskr ${args.join(" ")}: ${r.stdout}${r.stderr}`);
  return JSON.parse(r.stdout);
}
/** Linux: gone or a zombie counts as not running. */
function running(pid) {
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
    return stat.slice(stat.lastIndexOf(")") + 2)[0] !== "Z";
  } catch {
    return false;
  }
}

const { default: extension } = await import(pathToFileURL(path.join(here, "taskr-push.ts")).href);
function load(flags) {
  const handlers = new Map();
  const sent = [];
  const notes = [];
  const registered = {};
  extension({
    on(event, handler) {
      handlers.set(event, handler);
      return () => handlers.delete(event);
    },
    registerFlag(name, options) {
      registered[name] = options;
    },
    getFlag(name) {
      return name in flags ? flags[name] : registered[name]?.default;
    },
    sendUserMessage(content, options) {
      sent.push({ content, options });
    },
  });
  assert.deepEqual(Object.keys(registered).sort(), ["taskr-bin", "taskr-root", "taskr-session"]);
  const ctx = { cwd: scratch, hasUI: true, ui: { notify: (message, level) => notes.push({ message, level }) }, isIdle: () => true };
  return {
    sent,
    notes,
    noted: (text) => notes.some((n) => n.message.includes(text)),
    start: () => handlers.get("session_start")({ type: "session_start", reason: "startup" }, ctx),
    stop: () => handlers.get("session_shutdown")({ type: "session_shutdown", reason: "quit" }, ctx),
  };
}
const checks = [];
const pass = (name) => {
  checks.push(name);
  console.log(`ok - ${name}`);
};

// ---- a real `taskr mcp --channel` child on the scratch ledger ----
cli({}, "new", "top", "--role", "orchestrator");
cli({}, "new", "impl", "--role", "implementer", "--parent", "1", "--cwd", scratch, "--pane", "w9:p1");
const launch = cli({}, "launch", "2", "--provider", "pi", "--model", "test", "--effort", "medium").launch_id;
const worker = { TASKR_TASK: "2", TASKR_LAUNCH: String(launch) };
const ask = cli(worker, "ask", "--", "synthetic question?");
const answer = cli({}, "answer", String(ask.ask_id), "--", "synthetic answer");
const answerEvent = cli(worker, "inbox").events[0].id;
assert.equal(answer.ok, true);

{
  // A worker: identity from the pi process environment.
  Object.assign(process.env, worker);
  const m = load({ "taskr-bin": taskr });
  m.start();
  await until("the worker push", () => m.sent.length === 1);
  const { content, options } = m.sent[0];
  assert.deepEqual(options, { deliverAs: "followUp" });
  assert.match(content, new RegExp(`^taskr event ${answerEvent} \\(kind answer\\) from task 2: delivery attempt \\d+, consumer fence 1\\.\\n`));
  assert.ok(content.includes(`(CLI: taskr ack ${answerEvent} --as 2 --fence 1)`), content);
  const lease = cli(worker, "consumer", "show");
  assert.equal(lease.live, true);
  const pid = Number(lease.holder.replace("mcp-channel-", ""));
  assert.ok(running(pid));
  // A second session_start starts nothing.
  m.start();
  assert.ok(m.noted("already running"));
  await sleep(300);
  assert.equal(cli(worker, "consumer", "show").holder, lease.holder);
  // No receipt is made for the model.
  await until("the submitted stamp", () => cli(worker, "delivery", "ls").attempts.every((a) => a.state === "submitted"));
  assert.equal(cli(worker, "delivery", "ls").attempts.length, 1);
  const started = Date.now();
  await m.stop();
  await m.stop();
  assert.ok(Date.now() - started < GRACE_MS, "EOF released without the kill");
  const after = cli(worker, "consumer", "show");
  assert.deepEqual([after.fence, after.holder, after.live], [1, null, false]);
  assert.ok(!running(pid));
  assert.equal(m.sent.length, 1);
  for (const k of Object.keys(worker)) delete process.env[k];
  pass("worker: one followUp per push, lease released and child reaped on shutdown, repeated start ignored");
}

{
  // A root: typed options, the pinned generation in the envelope.
  const m = load({ "taskr-bin": taskr, "taskr-root": "1", "taskr-session": "0" });
  m.start();
  await until("the root push", () => m.sent.length === 1);
  const { content } = m.sent[0];
  assert.match(content, new RegExp(`^taskr event ${ask.event_id} \\(kind ask\\) from task 2:`));
  assert.ok(content.includes(`(CLI: taskr ack ${ask.event_id} --as 1 --session 0 --fence 1)`), content);
  assert.ok(content.includes("(CLI: taskr inbox --as 1;"), content);
  await m.stop();
  const after = cli({}, "consumer", "show", "--as", "1");
  assert.deepEqual([after.fence, after.holder], [1, null]);
  pass("root: --taskr-root/--taskr-session bind the child; CLI lines carry --session");
}

{
  for (const flags of [{ "taskr-root": "1" }, { "taskr-session": "0" }, { "taskr-root": "1; true", "taskr-session": "0" }, { "taskr-root": "0", "taskr-session": "0" }, { "taskr-root": "1", "taskr-session": "-1" }]) {
    const m = load({ "taskr-bin": taskr, ...flags });
    m.start();
    assert.ok(m.noted("--taskr-root needs"), JSON.stringify(flags));
    await m.stop();
  }
  assert.equal(cli({}, "consumer", "show", "--as", "1").holder, null);
  pass("invalid root options start nothing");
}

{
  const m = load({ "taskr-bin": path.join(scratch, "no-such-taskr") });
  m.start();
  await until("the spawn error", () => m.noted("could not run"));
  await m.stop();
  m.start();
  assert.ok(m.noted("not restarted"));
  assert.equal(m.sent.length, 0);
  pass("a spawn error is logged, never retried");
}

// ---- a fake child for framing and lifecycle edges ----
const fake = path.join(scratch, "fake-taskr.mjs");
fs.writeFileSync(
  fake,
  `#!${process.execPath}
import fs from "node:fs";
import { spawn } from "node:child_process";
const mode = process.env.FAKE_MODE;
fs.appendFileSync(process.env.FAKE_LOG, process.pid + "\\n");
const out = (s) => process.stdout.write(s);
const push = (content) => JSON.stringify({ jsonrpc: "2.0", method: "notifications/claude/channel", params: { content } }) + "\\n";
let buffer = "";
let held = "";
const grandchild = () => {
  const g = spawn("sleep", ["60"], { stdio: "ignore" });
  fs.appendFileSync(process.env.FAKE_LOG, "grandchild " + g.pid + "\\n");
};
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let nl;
  while ((nl = buffer.indexOf("\\n")) >= 0) {
    const message = JSON.parse(buffer.slice(0, nl));
    buffer = buffer.slice(nl + 1);
    if (message.method === "initialize") {
      const result = { protocolVersion: "2025-06-18", capabilities: mode === "nochannel" ? {} : { experimental: { "claude/channel": {} } } };
      const reply = JSON.stringify({ jsonrpc: "2.0", id: message.id, result }) + "\\n";
      if (mode === "leaderfirst") grandchild();
      if (mode === "late") {
        held = reply;
        continue;
      }
      out(reply.slice(0, 20));
      setTimeout(() => out(reply.slice(20)), 30);
    }
    if (message.method === "notifications/initialized") {
      fs.appendFileSync(process.env.FAKE_LOG, "initialized\\n");
      if (mode === "leaderfirst") continue;
      if (mode === "exit") {
        grandchild();
        process.exit(3);
      }
      grandchild();
      out("{not json\\n");
      const big = push("OVERSIZE" + "x".repeat(1 << 20));
      out(big.slice(0, 600000));
      setTimeout(() => {
        out(big.slice(600000));
        out(JSON.stringify({ jsonrpc: "2.0", id: 7, method: "notifications/claude/channel", params: { content: "HAS-ID" } }) + "\\n");
        out(JSON.stringify({ jsonrpc: "2.0", method: "notifications/message", params: { content: "OTHER" } }) + "\\n");
        out(JSON.stringify({ jsonrpc: "2.0", method: "notifications/claude/channel", params: { content: 42 } }) + "\\n");
        out("[1,2]\\n");
        out(push("ONE") + push("TWO"));
        const three = push("THREE");
        out(three.slice(0, 30));
        setTimeout(() => out(three.slice(30)), 30);
      }, 30);
    }
  }
});
process.stdin.on("end", () => {
  // The leader exits at once and leaves its descendant behind.
  if (mode === "leaderfirst") process.exit(0);
  // The initialize reply arrives only after shutdown began, then a push.
  if (mode === "late") {
    out(held + push("PUSH-AFTER-SHUTDOWN"));
    setTimeout(() => process.exit(0), 200);
    return;
  }
  // Lingers after EOF and still writes: the shim must not forward it, and kills it.
  out(push("AFTER-SHUTDOWN"));
  setInterval(() => out(push("STILL-HERE")), 200);
});
`,
);
fs.chmodSync(fake, 0o755);
function fakeRun(mode) {
  const log = path.join(scratch, `fake-${mode}.log`);
  fs.writeFileSync(log, "");
  process.env.FAKE_MODE = mode;
  process.env.FAKE_LOG = log;
  const lines = () => fs.readFileSync(log, "utf8").split("\n").filter(Boolean);
  return { log, initialized: () => lines().includes("initialized"), spawns: () => lines().filter((l) => /^\d+$/.test(l)), grandchildren: () => lines().filter((l) => l.startsWith("grandchild")).map((l) => Number(l.split(" ")[1])) };
}

{
  const f = fakeRun("frames");
  const m = load({ "taskr-bin": fake });
  m.start();
  m.start();
  await until("three forwarded pushes", () => m.sent.length >= 3);
  await sleep(300);
  assert.deepEqual(m.sent.map((s) => s.content), ["ONE", "TWO", "THREE"]);
  assert.ok(m.sent.every((s) => s.options.deliverAs === "followUp"));
  assert.equal(f.spawns().length, 1, "repeated session_start spawned twice");
  const [pid] = f.spawns().map(Number);
  const [grandchild] = f.grandchildren();
  assert.ok(running(pid) && running(grandchild));
  const started = Date.now();
  await Promise.all([m.stop(), m.stop()]);
  const took = Date.now() - started;
  assert.ok(took >= GRACE_MS - 100 && took < GRACE_MS + 3000, `shutdown took ${took} ms`);
  await until("the group kill", () => !running(pid) && !running(grandchild), 3000);
  assert.deepEqual(m.sent.map((s) => s.content), ["ONE", "TWO", "THREE"], "forwarded after shutdown");
  pass("fragmented, coalesced, oversize, malformed and non-channel lines; only channel notifications forwarded; a lingering child and its descendant killed after the grace");
}

{
  const f = fakeRun("exit");
  const m = load({ "taskr-bin": fake });
  m.start();
  await until("the exit log", () => m.noted("not restarted"));
  const [grandchild] = f.grandchildren();
  m.start();
  await sleep(300);
  assert.equal(f.spawns().length, 1, "respawned after an exit");
  await m.stop();
  assert.ok(!running(grandchild), "a descendant outlived an unexpected exit");
  assert.equal(m.sent.length, 0);
  pass("an unexpected child exit is logged, its descendant killed, never respawned");
}

{
  const f = fakeRun("leaderfirst");
  const m = load({ "taskr-bin": fake });
  m.start();
  await until("the handshake", () => f.initialized());
  const [pid] = f.spawns().map(Number);
  const [grandchild] = f.grandchildren();
  assert.ok(running(pid) && running(grandchild));
  const started = Date.now();
  await m.stop();
  assert.ok(Date.now() - started < GRACE_MS, "waited for the grace");
  assert.ok(!running(pid) && !running(grandchild), "shutdown finished with a descendant alive");
  pass("a leader that exits first: shutdown kills its descendant before it completes");
}

{
  const f = fakeRun("late");
  const m = load({ "taskr-bin": fake });
  m.start();
  await sleep(300);
  await m.stop();
  await sleep(300);
  assert.equal(m.sent.length, 0, "forwarded after shutdown began");
  assert.ok(!f.initialized(), "initialized sent after shutdown began");
  pass("an initialize reply after shutdown began enables nothing");
}

{
  const f = fakeRun("nochannel");
  const m = load({ "taskr-bin": fake });
  m.start();
  await until("the refusal", () => m.noted("did not offer claude/channel"));
  await m.stop();
  assert.equal(m.sent.length, 0);
  assert.equal(f.spawns().length, 1);
  pass("a child without the claude/channel capability is stopped");
}

fs.rmSync(scratch, { recursive: true, force: true });
console.log(`PASS ${checks.length} checks`);

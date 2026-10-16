import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { TaskrHook } from "../adapters/opencode/taskr-hook.js";
import piHook from "../adapters/pi/taskr-hook.ts";

const savedEnv = Object.fromEntries(["HOME", "PATH", "TASKR_LAUNCH", "TASKR_RECORD_DIR"].map((key) => [key, process.env[key]]));
const root = await mkdtemp(join(tmpdir(), "taskr-hook-test-"));

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function recordFor(dir, event) {
  const file = join(dir, `${event}.json`);
  for (let attempt = 0; attempt < 100; attempt++) {
    try {
      return JSON.parse(await readFile(file, "utf8"));
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
      await delay(10);
    }
  }
  throw new Error(`taskr did not record ${event}`);
}

try {
  const home = join(root, "home");
  const bin = join(home, ".local/bin");
  const records = join(root, "records");
  await mkdir(bin, { recursive: true });
  await mkdir(records);
  await writeFile(
    join(bin, "taskr"),
    `#!/usr/bin/env node
const fs = require("node:fs");
const { join } = require("node:path");
const args = process.argv.slice(2);
let input = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => { input += chunk; });
process.stdin.on("end", () => {
  fs.writeFileSync(join(process.env.TASKR_RECORD_DIR, args[2] + ".json"), JSON.stringify({ args, input }));
});
`,
    { mode: 0o755 },
  );

  process.env.HOME = home;
  process.env.PATH = `${bin}:${savedEnv.PATH ?? ""}`;
  process.env.TASKR_RECORD_DIR = records;
  delete process.env.TASKR_LAUNCH;
  assert.deepEqual(TaskrHook(), {});
  const handlers = new Map();
  const pi = { on: (event, handler) => handlers.set(event, handler) };
  piHook(pi);
  assert.equal(handlers.size, 0);

  process.env.TASKR_LAUNCH = "test-launch";
  const hooks = TaskrHook();
  const chatInput = { sessionID: "session-1", prompt: "First taskr got 42." };
  const chatOutput = { parts: [{ type: "text", text: "hello" }] };
  assert.equal(hooks["chat.message"](chatInput, chatOutput), undefined);
  const chat = await recordFor(records, "chat.message");
  assert.deepEqual(chat.args, ["hook", "opencode", "chat.message"]);
  assert.equal(chat.input, JSON.stringify({ input: chatInput, output: chatOutput }));

  for (const type of ["session.created", "session.idle", "session.error"]) {
    const event = { type, session: { id: "session-1" }, detail: "fixture" };
    assert.equal(hooks.event({ event }), undefined);
    const record = await recordFor(records, type);
    assert.deepEqual(record.args, ["hook", "opencode", type]);
    assert.equal(record.input, JSON.stringify(event));
  }

  assert.equal(hooks.event({ event: { type: "session.status", session: { id: "session-1" } } }), undefined);
  await delay(50);
  await assert.rejects(readFile(join(records, "session.status.json"), "utf8"), { code: "ENOENT" });

  await rm(join(records, "chat.message.json"));
  await rm(join(records, "session.created.json"));
  const circular = { parts: [] };
  circular.self = circular;
  assert.doesNotThrow(() => hooks["chat.message"](chatInput, circular));
  assert.doesNotThrow(() => hooks.event({ event: { type: "session.created", n: 1n } }));
  await delay(100);
  await assert.rejects(readFile(join(records, "chat.message.json"), "utf8"), { code: "ENOENT" });
  await assert.rejects(readFile(join(records, "session.created.json"), "utf8"), { code: "ENOENT" });

  piHook(pi);
  assert.deepEqual([...handlers.keys()], ["session_start", "input", "agent_settled"]);
  const ctx = {
    sessionManager: {
      getSessionId: () => "pi-session",
      getSessionFile: () => join(root, "session.jsonl"),
      getBranch: () => [
        { type: "message", message: { role: "assistant", stopReason: "stop" } },
        { type: "message", message: { role: "user", content: "ignored" } },
        { type: "message", message: {
          role: "assistant", stopReason: "error", content: "private text omitted",
          diagnostics: [{ type: "error", error: { name: "ApiError", code: "quota", message: "private error omitted" } }],
        } },
      ],
    },
  };
  const session = { sessionId: "pi-session", sessionFile: join(root, "session.jsonl") };
  for (const [event, input, payload] of [
    ["session_start", { reason: "new" }, { type: "session_start", reason: "new", ...session }],
    ["input", { text: "fixture prompt", source: "rpc" }, { type: "input", text: "fixture prompt", source: "rpc", ...session }],
    ["agent_settled", {}, { type: "agent_settled", message: {
      stopReason: "error", diagnostics: [{ type: "error", error: { name: "ApiError", code: "quota" } }],
    }, ...session }],
  ]) {
    assert.equal(handlers.get(event)(input, ctx), undefined);
    const record = await recordFor(records, event);
    assert.deepEqual(record.args, ["hook", "pi", event]);
    assert.deepEqual(JSON.parse(record.input), payload);
  }
  await rm(join(records, "session_start.json"));
  ctx.sessionManager.getSessionFile = () => "relative.jsonl";
  handlers.get("session_start")({}, ctx);
  assert.deepEqual(JSON.parse((await recordFor(records, "session_start")).input), {
    type: "session_start", sessionId: "pi-session",
  });
  const badCtx = { sessionManager: {
    getSessionId: () => { throw new Error("fixture"); },
    getSessionFile: () => { throw new Error("fixture"); },
    getBranch: () => { throw new Error("fixture"); },
  } };
  await rm(join(records, "agent_settled.json"));
  assert.doesNotThrow(() => handlers.get("agent_settled")({}, badCtx));
  assert.deepEqual(JSON.parse((await recordFor(records, "agent_settled")).input), { type: "agent_settled" });

  await writeFile(join(bin, "taskr"), "#!/bin/sh\nexec sleep 2\n", { mode: 0o755 });
  const calls = [
    () => hooks["chat.message"](chatInput, chatOutput),
    ...["session.created", "session.idle", "session.error"].map((type) => () => hooks.event({ event: { type } })),
    ...["session_start", "input", "agent_settled"].map((type) => () => handlers.get(type)({}, ctx)),
  ];
  for (const call of calls) {
    const start = performance.now();
    assert.equal(call(), undefined);
    assert.ok(performance.now() - start < 100, "hook call blocked on taskr");
  }

  process.env.HOME = join(root, "missing-home");
  assert.doesNotThrow(() => hooks.event({ event: { type: "session.idle" } }));
  assert.doesNotThrow(() => handlers.get("input")({}, ctx));
  await delay(50);
} finally {
  for (const [key, value] of Object.entries(savedEnv)) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
  await rm(root, { recursive: true, force: true });
}

console.log("taskr OpenCode and pi hooks passed");

// @ts-nocheck
import { spawn } from "node:child_process";
import { isAbsolute, join } from "node:path";

function session(ctx) {
  const ref = {};
  try {
    const id = ctx?.sessionManager?.getSessionId?.();
    if (typeof id === "string" && id) ref.sessionId = id;
  } catch {}
  try {
    const file = ctx?.sessionManager?.getSessionFile?.();
    if (typeof file === "string" && isAbsolute(file)) ref.sessionFile = file;
  } catch {}
  return ref;
}

// The branch's last assistant message, reduced to the fields taskr reads.
function lastAssistant(ctx) {
  try {
    const branch = ctx?.sessionManager?.getBranch?.() ?? [];
    for (let i = branch.length - 1; i >= 0; i--) {
      const message = branch[i]?.type === "message" ? branch[i].message : undefined;
      if (message?.role !== "assistant") continue;
      const diagnostics = (message.diagnostics ?? []).map((d) => ({
        type: d?.type,
        error: d?.error ? { name: d.error.name, code: d.error.code } : undefined,
      }));
      return { stopReason: message.stopReason, diagnostics };
    }
  } catch {}
  return undefined;
}

function send(event, payload) {
  try {
    if (!process.env.TASKR_LAUNCH) return;
    const body = JSON.stringify(payload);
    const child = spawn(join(process.env.HOME, ".local/bin/taskr"), ["hook", "pi", event], {
      detached: true,
      stdio: ["pipe", "ignore", "ignore"],
    });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.unref();
    child.stdin.end(body);
  } catch {}
}

export default function (pi) {
  try {
    if (!process.env.TASKR_LAUNCH) return;
    pi.on("session_start", (event, ctx) => {
      try {
        send("session_start", { type: "session_start", reason: event?.reason, ...session(ctx) });
      } catch {}
    });
    pi.on("input", (event, ctx) => {
      try {
        send("input", { type: "input", text: event?.text, source: event?.source, ...session(ctx) });
      } catch {}
    });
    pi.on("agent_settled", (_event, ctx) => {
      try {
        send("agent_settled", { type: "agent_settled", message: lastAssistant(ctx), ...session(ctx) });
      } catch {}
    });
  } catch {}
}

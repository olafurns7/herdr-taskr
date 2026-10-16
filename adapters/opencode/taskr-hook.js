import { spawn } from "node:child_process";
import { join } from "node:path";

const EVENTS = new Set(["session.created", "session.idle", "session.error"]);

function send(event, payload) {
  try {
    if (!process.env.TASKR_LAUNCH) return;
    const body = JSON.stringify(payload);
    const child = spawn(join(process.env.HOME, ".local/bin/taskr"), ["hook", "opencode", event], {
      detached: true,
      stdio: ["pipe", "ignore", "ignore"],
    });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.unref();
    child.stdin.end(body);
  } catch {}
}

export const TaskrHook = () => {
  try {
    if (!process.env.TASKR_LAUNCH) return {};
    return {
      "chat.message": (input, output) => {
        try {
          send("chat.message", { input, output });
        } catch {}
      },
      event: (data) => {
        try {
          const event = data?.event;
          if (event && EVENTS.has(event.type)) send(event.type, event);
        } catch {}
      },
    };
  } catch {
    return {};
  }
};

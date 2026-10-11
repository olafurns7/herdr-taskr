/**
 * taskr push for pi: a thin shim over `taskr mcp --channel`.
 *
 * On session_start it starts one `taskr mcp --channel` child (argv only, no shell; the
 * pi process environment, so a worker's TASKR_TASK/TASKR_LAUNCH bind it) and performs
 * the MCP initialize handshake. Each `notifications/claude/channel` becomes
 * `pi.sendUserMessage(params.content, {deliverAs: "followUp"})`; nothing else the child
 * writes reaches the model, and the event itself is never parsed or acted on. The child
 * owns the lease, the attempts and the envelope; receipts and acks are the model's own
 * acts (taskr CLI, or a separately configured plain `taskr mcp`). This shim adds no tool
 * and no socket.
 *
 * Options (pi flags), all optional:
 *   --taskr-bin PATH        the taskr executable (default: `taskr` on PATH)
 *   --taskr-root ID         a root orchestrator's task id, with --taskr-session
 *   --taskr-session GEN     that root's generation: the event id `taskr adopt` printed,
 *                           0 before any adopt
 *
 * On session_shutdown it stops forwarding for good, closes the child's stdin (the child
 * releases its lease) and kills the child's process group after 5 s; whenever the leader
 * exits, what is left of its group is killed and shutdown waits until the group is
 * empty. A child that exits on its own is logged and never respawned, so a crash loop
 * cannot run up spend.
 */
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { ChildProcessWithoutNullStreams } from "node:child_process";
import { spawn } from "node:child_process";

const PROTOCOL = "2025-06-18";
const LINE_MAX = 1 << 20;
const GRACE_MS = 5000;
const TAIL_MAX = 4096;

export default function taskrPush(pi: ExtensionAPI): void {
  pi.registerFlag("taskr-bin", { type: "string", default: "taskr", description: "taskr push: the taskr executable" });
  pi.registerFlag("taskr-root", { type: "string", description: "taskr push: a root's task id (with --taskr-session); a worker uses TASKR_TASK and TASKR_LAUNCH" });
  pi.registerFlag("taskr-session", { type: "string", description: "taskr push: the root's generation, the event id `taskr adopt` printed (0 before any adopt)" });

  /** One child: `closed` is terminal (shutdown began or the child ended); `exited`
   * settles once the leader has exited and its process group is empty. */
  interface Run {
    proc: ChildProcessWithoutNullStreams;
    closed: boolean;
    exited: Promise<void>;
  }
  let run: Run | undefined;
  let failed = false;
  let stopping: Promise<void> | undefined;
  let log = (_message: string, _level?: "info" | "warning" | "error"): void => {};

  /** `mcp --channel` plus a root's typed options; undefined when they are invalid. */
  function argv(): string[] | undefined {
    const flag = (name: string): string => {
      const v = pi.getFlag(name);
      return typeof v === "string" ? v : "";
    };
    const root = flag("taskr-root");
    const session = flag("taskr-session");
    if (!root && !session) return ["mcp", "--channel"];
    if (!/^[1-9][0-9]{0,17}$/.test(root) || !/^(0|[1-9][0-9]{0,17})$/.test(session)) {
      log("--taskr-root needs a positive task id and --taskr-session a generation (0 or an adopt event id); not started", "error");
      return undefined;
    }
    return ["mcp", "--channel", "--as", root, "--session", session];
  }

  /** After the leader exits, kill whatever is left in its group and wait until the
   * group is empty (orphans are reaped by init or a subreaper, not by us). */
  async function sweep(pgid: number | undefined): Promise<void> {
    if (!pgid) return;
    for (let i = 0; ; i++) {
      try {
        process.kill(-pgid, i === 0 ? "SIGKILL" : 0);
      } catch {
        return;
      }
      if (i === 100) return log(`process group ${pgid} still has members 2 s after its leader exited`, "warning");
      await new Promise((r) => setTimeout(r, 20));
    }
  }

  function start(ctx: ExtensionContext): void {
    const args = argv();
    if (!args) return;
    const bin = (pi.getFlag("taskr-bin") as string | undefined) || "taskr";
    const proc = spawn(bin, args, {
      cwd: ctx.cwd,
      env: { ...process.env },
      stdio: ["pipe", "pipe", "pipe"],
      // Its own process group, so cleanup reaches every descendant.
      detached: true,
    });
    let settle!: () => void;
    const self: Run = { proc, closed: false, exited: new Promise<void>((r) => (settle = r)) };
    run = self;
    let tail = "";
    let initialized = false;
    let ended = false;
    const end = (why: string): void => {
      if (ended) return;
      ended = true;
      self.closed = true;
      if (!stopping) {
        failed = true;
        log(`taskr mcp --channel ${why}; not restarted${tail ? `: ${tail.trim().split("\n").pop()}` : ""}`, "warning");
      }
      void sweep(proc.pid).then(settle);
    };
    proc.on("error", (e) => end(`could not run: ${e.message}`));
    proc.on("exit", (code, signal) => end(`exited (${signal ?? code})`));
    proc.stdin.on("error", () => {});
    proc.stderr.setEncoding("utf8");
    proc.stderr.on("data", (chunk: string) => {
      tail = (tail + chunk).slice(-TAIL_MAX);
    });
    const send = (message: unknown): void => {
      if (!proc.stdin.destroyed && !proc.stdin.writableEnded) proc.stdin.write(`${JSON.stringify(message)}\n`);
    };
    const handle = (line: string): void => {
      // Shutdown or exit is final for this child: no late handshake, no forwarding.
      if (self.closed) return;
      let message: any;
      try {
        message = JSON.parse(line);
      } catch {
        return;
      }
      if (message === null || typeof message !== "object" || Array.isArray(message)) return;
      if (!initialized) {
        if (message.id !== 1) return;
        const result = message.result;
        const channel = result?.capabilities?.experimental?.["claude/channel"];
        if (result?.protocolVersion !== PROTOCOL || channel === null || typeof channel !== "object") {
          log("the child did not offer claude/channel on MCP 2025-06-18; stopping it", "error");
          failed = true;
          void shutdown();
          return;
        }
        initialized = true;
        send({ jsonrpc: "2.0", method: "notifications/initialized" });
        return;
      }
      if (message.method !== "notifications/claude/channel" || "id" in message) return;
      const content = message.params?.content;
      if (typeof content !== "string" || content === "") return;
      pi.sendUserMessage(content, { deliverAs: "followUp" });
    };
    // Newline-delimited JSON in arbitrary chunks; a line over LINE_MAX is dropped whole.
    let buffer = "";
    let skipping = false;
    proc.stdout.setEncoding("utf8");
    proc.stdout.on("data", (chunk: string) => {
      buffer += chunk;
      for (;;) {
        const nl = buffer.indexOf("\n");
        if (nl < 0) break;
        const line = buffer.slice(0, nl);
        buffer = buffer.slice(nl + 1);
        if (skipping) skipping = false;
        else if (line.length <= LINE_MAX) handle(line);
      }
      if (buffer.length > LINE_MAX) {
        buffer = "";
        skipping = true;
      }
    });
    send({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: { protocolVersion: PROTOCOL, capabilities: {}, clientInfo: { name: "taskr-push-pi", version: "1" } },
    });
  }

  /** Idempotent: stop forwarding, close stdin, kill the group after the grace, and
   * finish only once the leader has exited and its group is empty. */
  function shutdown(): Promise<void> {
    const self = run;
    if (!self) return Promise.resolve();
    self.closed = true;
    if (stopping) return stopping;
    const { proc } = self;
    const timer = setTimeout(() => {
      try {
        process.kill(-(proc.pid as number), "SIGKILL");
      } catch {
        proc.kill("SIGKILL");
      }
    }, GRACE_MS);
    if (!proc.stdin.destroyed) proc.stdin.end();
    stopping = self.exited.then(() => {
      clearTimeout(timer);
      if (run === self) run = undefined;
      stopping = undefined;
    });
    return stopping;
  }

  pi.on("session_start", (_event, ctx) => {
    log = (message, level = "info") => ctx.ui?.notify?.(`taskr push: ${message}`, level);
    if (failed) return log("the child exited earlier; not restarted", "warning");
    if (run || stopping) return log("already running; not starting another", "warning");
    start(ctx);
  });
  pi.on("session_shutdown", async () => {
    await shutdown();
  });
}

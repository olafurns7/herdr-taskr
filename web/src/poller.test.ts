import { describe, expect, it } from "vitest";
import { poller } from "./poller";

// A fake page: controlled fetches, a timer table, a visibility flag.
function page() {
  type State = { now: number; tag: string };
  const calls: Array<{ resolve: (v: State) => void; reject: (e: unknown) => void }> = [];
  const timers = new Map<number, () => void>();
  let next = 0;
  const shown: string[] = [];
  const failed: unknown[] = [];
  const env = { hidden: false };
  const p = poller<State>({
    fetch: () => new Promise<State>((resolve, reject) => calls.push({ resolve, reject })),
    apply: (v) => shown.push(v.tag),
    fail: (e) => failed.push(e),
    hidden: () => env.hidden,
    setTimeout: (fn, _ms) => (timers.set(++next, fn), next),
    clearTimeout: (id) => void timers.delete(id),
    every: 3000,
    at: (v) => v.now,
  });
  const settle = () => new Promise((r) => setTimeout(r, 0));
  const fire = () => {
    const fns = [...timers.values()];
    timers.clear();
    for (const fn of fns) fn();
  };
  return { p, calls, timers, shown, failed, env, settle, fire };
}

describe("the state poll", () => {
  // The finish review's probe: focus and visibility wakes during the first
  // request used to start two more requests and leave three timers, and the
  // older response overwrote the newer one.
  it("keeps a single poll in flight on focus and visibility wake", async () => {
    const t = page();
    t.p.wake();
    t.p.visibility();
    expect(t.calls.length).toBe(1);
    t.calls[0]!.resolve({ now: 2, tag: "new" });
    await t.settle();
    expect({ fetches: t.calls.length, timers: t.timers.size, shown: t.shown.at(-1) }).toEqual({ fetches: 1, timers: 1, shown: "new" });
  });

  it("polls again on its one timer, and keeps one timer across wakes", async () => {
    const t = page();
    t.calls[0]!.resolve({ now: 1, tag: "a" });
    await t.settle();
    t.p.wake();
    expect(t.timers.size).toBe(0);
    t.calls[1]!.resolve({ now: 2, tag: "b" });
    await t.settle();
    expect(t.timers.size).toBe(1);
    t.fire();
    expect(t.calls.length).toBe(3);
    expect(t.timers.size).toBe(0);
  });

  it("drops a response older than the one shown", async () => {
    const t = page();
    t.calls[0]!.resolve({ now: 5, tag: "newer" });
    await t.settle();
    t.fire();
    t.calls[1]!.resolve({ now: 4, tag: "older" });
    await t.settle();
    expect(t.shown).toEqual(["newer"]);
    expect(t.timers.size).toBe(1);
  });

  it("pauses while hidden and resumes on show", async () => {
    const t = page();
    t.env.hidden = true;
    t.calls[0]!.resolve({ now: 1, tag: "a" });
    await t.settle();
    expect(t.timers.size).toBe(0);
    t.p.wake();
    expect(t.calls.length).toBe(1);
    t.env.hidden = false;
    t.p.visibility();
    expect(t.calls.length).toBe(2);
    t.calls[1]!.reject(new Error("down"));
    await t.settle();
    expect(t.failed.length).toBe(1);
    expect(t.timers.size).toBe(1);
    t.env.hidden = true;
    t.p.visibility();
    expect(t.timers.size).toBe(0);
  });

  it("does nothing after stop", async () => {
    const t = page();
    t.p.stop();
    t.calls[0]!.resolve({ now: 1, tag: "late" });
    await t.settle();
    t.p.wake();
    expect({ shown: t.shown, timers: t.timers.size, fetches: t.calls.length }).toEqual({ shown: [], timers: 0, fetches: 1 });
  });
});

import { expect, it } from "vitest";
import { hashRoute, routePolling } from "./router";
import { poller } from "./poller";

it("selects all views and falls back to the dashboard for unknown or malformed hashes", () => {
  expect(hashRoute("#/campaigns")).toEqual({ view: "campaigns" });
  expect(hashRoute("#/campaign/12")).toEqual({ view: "campaign", id: 12 });
  expect(hashRoute("#/doc/7")).toEqual({ view: "doc", id: 7 });
  for (const hash of ["", "#/", "#main", "#/unknown", "#/campaign/0", "#/doc/no", "#/doc/1/more", "#/doc/999999999999999999999"]) expect(hashRoute(hash)).toEqual({ view: "dashboard" });
});
it("stops real poll timers and in-flight application off dashboard; starts again on return", async () => {
  const pending: ((n: number) => void)[] = []; const shown: number[] = []; const timers = new Map<number, () => void>(); let timer = 0;
  const routes = routePolling(() => poller({
    fetch: () => new Promise<number>(resolve => pending.push(resolve)), apply: n => shown.push(n), fail: () => {}, hidden: () => false,
    setTimeout: fn => { timers.set(++timer, fn); return timer; }, clearTimeout: id => void timers.delete(id), every: 3000, at: n => n,
  }));
  const settle = async () => { await Promise.resolve(); await Promise.resolve(); await Promise.resolve(); };
  routes.select(hashRoute("#/doc/1")); expect(pending).toHaveLength(0);
  routes.select(hashRoute("#/")); routes.select(hashRoute("#/")); expect(pending).toHaveLength(1);
  routes.select(hashRoute("#/campaigns")); pending[0]!(1); await settle();
  expect(shown).toEqual([]); expect(timers.size).toBe(0);
  routes.wake(); routes.visibility(); expect(pending).toHaveLength(1);
  routes.select(hashRoute("#/")); pending[1]!(2); await settle(); expect(shown).toEqual([2]); expect(timers.size).toBe(1);
  routes.select(hashRoute("#/campaign/1")); expect(timers.size).toBe(0);
  routes.select(hashRoute("#/")); expect(pending).toHaveLength(3); routes.stop(); pending[2]!(3); await settle(); expect(shown).toEqual([2]); expect(timers.size).toBe(0);
});

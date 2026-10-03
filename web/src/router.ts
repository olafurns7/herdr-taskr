import type { Poller } from "./poller";

export type Route = { view: "dashboard" } | { view: "campaigns" } | { view: "campaign" | "doc"; id: number };
export function hashRoute(hash: string): Route {
  if (hash === "#/campaigns") return { view: "campaigns" };
  const match = /^#\/(campaign|doc)\/([1-9]\d*)$/.exec(hash);
  if (match && Number.isSafeInteger(Number(match[2]))) return { view: match[1] as "campaign" | "doc", id: Number(match[2]) };
  return { view: "dashboard" };
}

// This owns the same poller used by App: leaving the dashboard stops pending responses and timers.
export function routePolling(start: () => Poller) {
  let current: Poller | null = null;
  return {
    select(route: Route) {
      if (route.view === "dashboard") current ??= start();
      else { current?.stop(); current = null; }
    },
    wake() { current?.wake(); },
    visibility() { current?.visibility(); },
    stop() { current?.stop(); current = null; },
  };
}

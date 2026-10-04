import { useEffect, useRef, useState } from "preact/hooks";
import type { StateView } from "./api";
import { fetchState } from "./api";
import { Dashboard } from "./components/Dashboard";
import { poller } from "./poller";
import { pageTitle, programCounts } from "./model";
import { hashRoute, routePolling } from "./router";
import { CampaignPage } from "./components/CampaignPage";
import { markOwnerNotesRead, readOwnerNotes, OWNER_NOTES_READ_KEY } from "./owner-notes";
import { ownerNoteSummary } from "./work";

const POLL_MS = 3000;

export function App() {
  const [route, setRoute] = useState(() => hashRoute(window.location.hash));
  const [s, setS] = useState<StateView | null>(null);
  const [skew, setSkew] = useState(0); // local clock minus server clock, ms
  const [lost, setLost] = useState<number | null>(null); // local time of the last good poll, while polls fail
  const [readThrough, setReadThrough] = useState(() => readOwnerNotes());
  const lastOK = useRef(0);

  useEffect(() => {
    const p = routePolling(() => poller({
      fetch: fetchState,
      apply: (next) => {
        lastOK.current = Date.now();
        setSkew(Date.now() - Date.parse(next.now));
        setS(next);
        setLost(null);
      },
      fail: () => setLost(lastOK.current),
      hidden: () => document.hidden,
      setTimeout: (fn, ms) => window.setTimeout(fn, ms),
      clearTimeout: (id) => window.clearTimeout(id),
      every: POLL_MS,
      at: (next) => Date.parse(next.now),
    }));
    const select = () => { const next = hashRoute(window.location.hash); setRoute(next); p.select(next); };
    p.select(hashRoute(window.location.hash));
    window.addEventListener("hashchange", select);
    window.addEventListener("focus", p.wake);
    document.addEventListener("visibilitychange", p.visibility);
    return () => {
      p.stop();
      window.removeEventListener("hashchange", select);
      window.removeEventListener("focus", p.wake);
      document.removeEventListener("visibilitychange", p.visibility);
    };
  }, []);

  useEffect(() => {
    const sync = (event: StorageEvent) => {
      if (event.key === OWNER_NOTES_READ_KEY) setReadThrough(readOwnerNotes());
    };
    window.addEventListener("storage", sync);
    return () => window.removeEventListener("storage", sync);
  }, []);

  const now = Date.now() - skew;
  const attention = s?.attention ?? [];
  const counts = programCounts(attention);
  const notes = s ? ownerNoteSummary(s, readThrough) : { newRows: 0, markThrough: readThrough };
  useEffect(() => {
    if (route.view === "dashboard") document.title = pageTitle(counts, notes.newRows);
  });

  if (route.view !== "dashboard") return <CampaignPage key={route.view + ("id" in route ? route.id : "")} route={route} />;
  const markRead = () => setReadThrough(markOwnerNotesRead(notes.markThrough));
  return <Dashboard state={s} now={now} lost={lost} readThrough={readThrough} markRead={markRead} />;
}

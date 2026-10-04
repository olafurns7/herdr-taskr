import { afterEach, expect, it, vi } from "vitest";
import { OWNER_NOTES_READ_KEY } from "./owner-notes";

const hooks = vi.hoisted(() => ({ effect: vi.fn(), setState: vi.fn() }));
vi.mock("preact/hooks", () => ({
  useEffect: hooks.effect,
  useRef: () => ({ current: 0 }),
  useState: (initial: unknown) => [typeof initial === "function" ? initial() : initial, hooks.setState],
}));
import { App } from "./App";

afterEach(() => { vi.unstubAllGlobals(); vi.clearAllMocks(); });

it("re-reads the owner-note marker on another tab's storage event and removes the listener", () => {
  let marker = "20";
  const addEventListener = vi.fn();
  const removeEventListener = vi.fn();
  vi.stubGlobal("window", {
    location: { hash: "#/" },
    localStorage: { getItem: () => marker },
    addEventListener, removeEventListener,
  });
  App();
  const cleanup = hooks.effect.mock.calls[1]![0]() as () => void;
  expect(addEventListener).toHaveBeenCalledWith("storage", expect.any(Function));
  const sync = addEventListener.mock.calls[0]![1] as (event: Pick<StorageEvent, "key">) => void;
  marker = "30";
  sync({ key: "unrelated" });
  expect(hooks.setState).not.toHaveBeenCalled();
  sync({ key: OWNER_NOTES_READ_KEY });
  expect(hooks.setState).toHaveBeenLastCalledWith(30);
  marker = "bad";
  sync({ key: OWNER_NOTES_READ_KEY });
  expect(hooks.setState).toHaveBeenLastCalledWith(0);
  cleanup();
  expect(removeEventListener).toHaveBeenCalledWith("storage", sync);
});

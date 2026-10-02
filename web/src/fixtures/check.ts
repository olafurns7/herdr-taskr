// The Go-written fixture (TestWebStateFixture) must satisfy the /api/state
// types: a renamed or retyped Go field fails `pnpm typecheck`.
import type { StateView } from "../api";
import state from "./state.json";

export const fixture: StateView = state satisfies StateView;

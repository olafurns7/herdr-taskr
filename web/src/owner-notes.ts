export const OWNER_NOTES_READ_KEY = "taskr.ownerNotes.readThrough";
type BrowserStorage = () => Pick<Storage, "getItem" | "setItem">;

export function readOwnerNotes(storage: BrowserStorage = () => window.localStorage): number {
  try {
    const id = Number(storage().getItem(OWNER_NOTES_READ_KEY));
    return Number.isSafeInteger(id) && id >= 0 ? id : 0;
  } catch {
    return 0;
  }
}

export function markOwnerNotesRead(id: number, storage: BrowserStorage = () => window.localStorage): number {
  try {
    storage().setItem(OWNER_NOTES_READ_KEY, String(id));
  } catch {
    // Keep the marker for this session when persistence is unavailable.
  }
  return id;
}

import { useEffect, useState } from "react";

/** A view's stored-settings format version, and how to bring older stored settings up to it — for
 *  when a field's *meaning* changes, not just its default (a brightness whose rendering changed
 *  scale, say). Stored settings written before versions existed are version 0. */
export type ParamsMigration = {
  version: number;
  /** `stored` as written at version `from` (< `version`), returned as the current version would
   *  have written it. Raw: unknown fields are dropped afterwards. */
  migrate: (stored: Record<string, unknown>, from: number) => Record<string, unknown>;
};

/** The field stamping a stored blob's version — not a param, so it's dropped on read. */
const VERSION_KEY = "__v";

function writeStored<T extends object>(key: string, params: T, migration?: ParamsMigration) {
  localStorage.setItem(key, JSON.stringify(migration ? { ...params, [VERSION_KEY]: migration.version } : params));
}

function readStored<T extends object>(key: string, defaults: T, migration?: ParamsMigration): T | null {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return null;
    let stored = JSON.parse(raw) as Record<string, unknown>;
    const from = typeof stored[VERSION_KEY] === "number" ? (stored[VERSION_KEY] as number) : 0;
    const migrating = migration !== undefined && from < migration.version;
    if (migrating) stored = migration.migrate(stored, from);
    // Merged over `defaults` rather than used as-is, so a stored blob from an older version
    // missing a since-added field still gets that field's current default instead of `undefined`
    // leaking into the live params — and keeping only the fields `defaults` has, so one since
    // removed doesn't linger (it would also make "is this the saved default" compare unequal).
    const params = { ...defaults, ...Object.fromEntries(Object.entries(stored).filter(([k]) => k in defaults)) };
    // Written back at once, stamped, so a slot is migrated exactly once — the saved default above
    // all, which is otherwise only ever written by an explicit "Save as default".
    if (migrating) writeStored(key, params, migration);
    return params;
  } catch {
    return null; // corrupt/foreign JSON in that key — fall back rather than crash the view
  }
}

/**
 * Backs a scope/analyzer view's on-screen tuning panel (glow, trail, tail, beam, …) with two
 * independent localStorage slots, same lightweight-preference mechanism `cageq-theme` already uses
 * in App.tsx (not the app's real document state, which lives on the backend):
 *
 *  - **Last used** (`${storageKey}-last`), auto-saved on every change, restores exactly where you
 *    left off on the next launch. This is *not* the same thing as the saved default below — that
 *    would clash whenever the two diverge (you tweak something after loading your saved default,
 *    close the app, and now which one should the next launch show?) — so this gets its own slot
 *    and is what actually seeds `params` on mount, falling back to the saved default (not straight
 *    to `defaults`) when it doesn't exist yet — e.g. right after this two-slot split first ships,
 *    so a saved default from before it existed keeps loading exactly as it always did, until
 *    something actually gets saved to (or diverges into) the new slot.
 *  - **Saved default** (`${storageKey}`), written only by an explicit `saveAsDefault()` call —
 *    a deliberate "this is my preset" snapshot, exposed as `savedDefault` so a caller can offer a
 *    "load it back" control (e.g. a third preset button) without needing to reload the view.
 *
 * `resetToFactory` clears the *last-used* slot and returns `params` to the hard-coded `defaults` —
 * it does NOT touch the saved default, which is now a persistent preset in its own right, same as
 * any other curated preset, not something Reset should discard. `clearSavedDefault` is the one way
 * to drop it; the views offer it on their "Save as default" button while `atSavedDefault` (saving
 * again would change nothing), so it needs no control of its own.
 *
 * `migration` (optional) versions both slots — see `ParamsMigration`.
 */
export function useTunableParams<T extends object>(storageKey: string, defaults: T, migration?: ParamsMigration) {
  const lastKey = `${storageKey}-last`;
  const [savedDefault, setSavedDefault] = useState<T | null>(() => readStored(storageKey, defaults, migration));
  const [params, setParams] = useState<T>(() => readStored(lastKey, defaults, migration) ?? savedDefault ?? defaults);

  useEffect(() => {
    try {
      writeStored(lastKey, params, migration);
    } catch {
      // Cosmetic preference, not worth surfacing an error for (private-mode/quota localStorage
      // failures are rare and inconsequential here — the view just keeps its in-memory params).
    }
    // `migration` is a module-level constant in every caller; not a dependency.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [params, lastKey]);

  const saveAsDefault = () => {
    try {
      writeStored(storageKey, params, migration);
      setSavedDefault(params);
    } catch {
      // Same rationale as the effect above.
    }
  };

  const resetToFactory = () => {
    localStorage.removeItem(lastKey);
    setParams(defaults);
  };

  const clearSavedDefault = () => {
    try {
      localStorage.removeItem(storageKey);
    } catch {
      // Same rationale as the effect above.
    }
    setSavedDefault(null);
  };

  /** The current params are the saved default (its preset button is the highlighted one). */
  const atSavedDefault = savedDefault !== null && JSON.stringify(params) === JSON.stringify(savedDefault);

  return { params, setParams, saveAsDefault, resetToFactory, savedDefault, clearSavedDefault, atSavedDefault };
}

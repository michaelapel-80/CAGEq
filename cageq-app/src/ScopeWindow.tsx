import { TimeScope } from "./TimeScope";
import { Vectorscope } from "./Vectorscope";

/**
 * A detached scope window (loaded via `index.html#scope` for the vectorscope, `#timescope` for the
 * time scope, see main.tsx). It renders nothing but that one instrument, full-window, in fill mode.
 * The loopback monitor runs in the main window; its `scope` stream reaches every window, so this
 * one just listens — it starts no capture of its own and carries none of the app's state.
 */
export function ScopeWindow({ instrument }: { instrument: "vector" | "time" }) {
  return <div className="scope-window">{instrument === "time" ? <TimeScope fill /> : <Vectorscope fill />}</div>;
}

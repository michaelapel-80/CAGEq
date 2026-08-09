import { Vectorscope } from "./Vectorscope";

/**
 * The detached vectorscope window (loaded via `index.html#scope`, see main.tsx). It renders nothing
 * but a full-window, fill-mode scope. The loopback monitor runs in the main window; its `scope`
 * events are emitted app-globally, so this window just listens (via Vectorscope) — it starts no
 * capture of its own and carries none of the app's state.
 */
export function ScopeWindow() {
  return (
    <div className="scope-window">
      <Vectorscope fill />
    </div>
  );
}

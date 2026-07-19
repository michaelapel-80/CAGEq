import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

type Headphone = { source: string; form_factor: string; name: string; path: string };
type Target = { name: string; path: string };
type ApplyResult = {
  hash: string;
  device: string;
  cageq_path: string;
  cageq_text: string;
  preamp_db: number;
  clipping_warning: boolean;
};
type Status = {
  startup: string;
  health: string;
  recoveries: number;
  config_dir: string;
  config_source: string;
  sidecar: string;
};

const display = (h: Headphone) => `${h.name} · ${h.source} · ${h.form_factor}`;

function App() {
  const [status, setStatus] = useState<Status | null>(null);
  const [headphones, setHeadphones] = useState<Headphone[]>([]);
  const [targets, setTargets] = useState<Target[]>([]);
  const [query, setQuery] = useState("");
  const [targetPath, setTargetPath] = useState("");
  const [result, setResult] = useState<ApplyResult | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [applying, setApplying] = useState(false);

  useEffect(() => {
    (async () => {
      try {
        setStatus(await invoke<Status>("status"));
        const [hp, tg] = await Promise.all([
          invoke<{ headphones: Headphone[] }>("list_headphones"),
          invoke<{ targets: Target[] }>("list_targets"),
        ]);
        setHeadphones(hp.headphones);
        setTargets(tg.targets);
        const harman = tg.targets.find((t) => /harman over-ear 2018$/i.test(t.name));
        setTargetPath(harman?.path ?? tg.targets[0]?.path ?? "");
      } catch (e) {
        setError(String(e));
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  // Filter client-side and cap the datalist so 6800+ entries stay responsive.
  const matches = useMemo(() => {
    if (query.length < 2) return [];
    const q = query.toLowerCase();
    return headphones.filter((h) => display(h).toLowerCase().includes(q)).slice(0, 200);
  }, [query, headphones]);

  async function apply() {
    const hp = headphones.find((h) => display(h) === query);
    if (!hp) {
      setError("Pick a headphone from the list first.");
      return;
    }
    try {
      setError("");
      setApplying(true);
      setResult(
        await invoke<ApplyResult>("apply", {
          device: hp.name,
          headphone: hp.path,
          target: targetPath || null,
        })
      );
      setStatus(await invoke<Status>("status"));
    } catch (e) {
      setError(String(e));
      setResult(null);
    } finally {
      setApplying(false);
    }
  }

  return (
    <main className="container">
      <h1>CAGEq</h1>
      <p>Caged Auto-Gain EQ — AutoEq database</p>

      {status && (
        <p style={{ fontSize: "0.8em", opacity: 0.75 }}>
          sidecar: {status.sidecar} · health: {status.health}
          <br />
          config: {status.config_source}
          <br />
          <span style={{ opacity: 0.7 }}>writes to: {status.config_dir}</span>
        </p>
      )}

      {loading ? (
        <p>Loading AutoEq catalogue…</p>
      ) : (
        <form
          className="row"
          onSubmit={(e) => {
            e.preventDefault();
            apply();
          }}
        >
          <input
            list="hp-list"
            value={query}
            onChange={(e) => setQuery(e.currentTarget.value)}
            placeholder={`Search ${headphones.length} headphones…`}
            style={{ minWidth: "22em" }}
          />
          <datalist id="hp-list">
            {matches.map((h) => (
              <option key={h.path} value={display(h)} />
            ))}
          </datalist>
          <select value={targetPath} onChange={(e) => setTargetPath(e.currentTarget.value)}>
            {targets.map((t) => (
              <option key={t.path} value={t.path}>
                {t.name}
              </option>
            ))}
          </select>
          <button type="submit" disabled={applying}>
            {applying ? "Fitting…" : "Apply"}
          </button>
        </form>
      )}

      {error && <p style={{ color: "crimson" }}>{error}</p>}

      {result && (
        <>
          <p>
            Preamp <code>{result.preamp_db.toFixed(1)} dB</code> (Auto-LUFS loudness match) · hash{" "}
            <code>{result.hash}</code> → {result.cageq_path}
          </p>
          {result.clipping_warning && (
            <p style={{ color: "#b8860b" }}>
              ⚠ Emergency clipping protection active instead of the loudness match — this curve has an
              extreme peak.
            </p>
          )}
          <pre
            style={{
              textAlign: "left",
              background: "#0002",
              padding: "0.75em",
              overflowX: "auto",
            }}
          >
            {result.cageq_text}
          </pre>
        </>
      )}
    </main>
  );
}

export default App;

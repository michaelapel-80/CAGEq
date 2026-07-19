import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

type ApplyResult = {
  hash: string;
  device: string;
  cageq_path: string;
  cageq_text: string;
};
type Status = {
  startup: string;
  health: string;
  recoveries: number;
  config_dir: string;
};

function App() {
  const [device, setDevice] = useState("USB DAC");
  const [result, setResult] = useState<ApplyResult | null>(null);
  const [status, setStatus] = useState<Status | null>(null);
  const [error, setError] = useState("");

  async function refreshStatus() {
    try {
      setStatus(await invoke<Status>("status"));
    } catch (e) {
      setError(String(e));
    }
  }

  useEffect(() => {
    refreshStatus();
  }, []);

  async function apply() {
    try {
      setError("");
      setResult(await invoke<ApplyResult>("apply", { device }));
      await refreshStatus();
    } catch (e) {
      setError(String(e));
      setResult(null);
    }
  }

  return (
    <main className="container">
      <h1>CAGEq</h1>
      <p>Caged Auto-Gain EQ — live backend (stub DSP)</p>

      {status && (
        <p style={{ fontSize: "0.85em", opacity: 0.8 }}>
          startup: {status.startup} · health: {status.health} · recoveries:{" "}
          {status.recoveries}
          <br />
          config dir: {status.config_dir}
        </p>
      )}

      <form
        className="row"
        onSubmit={(e) => {
          e.preventDefault();
          apply();
        }}
      >
        <input
          value={device}
          onChange={(e) => setDevice(e.currentTarget.value)}
          placeholder="Device name"
        />
        <button type="submit">Apply</button>
      </form>

      {error && <p style={{ color: "crimson" }}>{error}</p>}

      {result && (
        <>
          <p>
            Wrote hash <code>{result.hash}</code> → {result.cageq_path}
          </p>
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

import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

// Mirrors cageq-core's DeviceConfig (re-exported from cageq-config-writer). Tauri
// serialises the Rust return value via serde, so these fields line up 1:1.
type Filter = { kind: string; freq_hz: number; gain_db: number; q: number };
type DeviceConfig = { device: string; preamp_db: number; filters: Filter[] };

function App() {
  const [device, setDevice] = useState("USB DAC");
  const [config, setConfig] = useState<DeviceConfig | null>(null);
  const [error, setError] = useState("");

  async function compute() {
    try {
      setError("");
      setConfig(await invoke<DeviceConfig>("sample_config", { device }));
    } catch (e) {
      setError(String(e));
    }
  }

  return (
    <main className="container">
      <h1>CAGEq</h1>
      <p>Caged Auto-Gain EQ — backend wiring smoke test</p>

      <form
        className="row"
        onSubmit={(e) => {
          e.preventDefault();
          compute();
        }}
      >
        <input
          value={device}
          onChange={(e) => setDevice(e.currentTarget.value)}
          placeholder="Device name"
        />
        <button type="submit">Compute filters</button>
      </form>

      {error && <p style={{ color: "crimson" }}>{error}</p>}

      {config && (
        <>
          <table>
            <thead>
              <tr>
                <th>#</th>
                <th>Type</th>
                <th>Fc (Hz)</th>
                <th>Gain (dB)</th>
                <th>Q</th>
              </tr>
            </thead>
            <tbody>
              {config.filters.map((f, i) => (
                <tr key={i}>
                  <td>{i + 1}</td>
                  <td>{f.kind}</td>
                  <td>{f.freq_hz}</td>
                  <td>{f.gain_db}</td>
                  <td>{f.q}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p>
            Preamp: {config.preamp_db} dB · Device: {config.device}
          </p>
        </>
      )}
    </main>
  );
}

export default App;

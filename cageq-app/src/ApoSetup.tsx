import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";

/**
 * Setup panel for CAGEq's own APO (filter.md §5.3c).
 *
 * The shape of this panel follows one rule from the backend: **there is exactly one next
 * step at a time, and its order matters.** Attaching a device while Windows is still
 * refusing unsigned effects looks like it succeeded and silently does nothing — so the panel
 * never offers a list of buttons to pick from. It asks the backend what comes next and shows
 * that, which is why `next_step` is computed there rather than here.
 *
 * Reading the state needs no elevation, so this refreshes freely; only pressing the button
 * raises a UAC prompt, and only ever one.
 */
export type ApoSetupStatus = {
  registered_dll: string | null;
  dll_present: boolean;
  gate_open: boolean;
  machine_ready: boolean;
  attached: string[];
  effects_disabled: string[];
  active_backend_is_apo: boolean;
  /** Command string for the next step, e.g. `"open-gate"` — echoed back to the backend. */
  next_step: string | null;
  next_step_description: string | null;
  helper_available: boolean;
};

type Props = {
  /** The endpoint the user currently has selected, or null if none. */
  endpointId: string | null;
  endpointName: string | null;
};

export default function ApoSetup({ endpointId, endpointName }: Props) {
  const { t } = useTranslation();
  const [status, setStatus] = useState<ApoSetupStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setStatus(await invoke<ApoSetupStatus>("apo_setup_status", { endpoint: endpointId }));
  }, [endpointId]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  if (!status) return null;

  const attached = endpointId !== null && status.attached.includes(endpointId);
  const inert = endpointId !== null && status.effects_disabled.includes(endpointId);
  // Setup is complete but the app is still running the other backend: selection happens once
  // at startup, so nothing changes until CAGEq restarts. Saying so explicitly is the
  // difference between "finished" and a user wondering why it made no difference.
  const needsRestart = attached && !inert && !status.active_backend_is_apo;

  // The whole panel is uninteresting once everything is done and running.
  if (attached && !inert && status.active_backend_is_apo) {
    return (
      <div className="panel">
        <h2>{t("apoSetup.title")}</h2>
        <p className="ok">{t("apoSetup.active", { device: endpointName ?? "" })}</p>
        <button type="button" disabled={busy} onClick={() => void run(`detach ${endpointId}`)}>
          {t("apoSetup.detach")}
        </button>
        {feedback()}
      </div>
    );
  }

  async function run(action: string) {
    setBusy(true);
    setError(null);
    setMessage(null);
    try {
      // One UAC prompt happens inside this call. Cancelling it comes back as an empty
      // string rather than an error, because declining is a choice, not a failure.
      const text = await invoke<string>("apo_setup_run", { action });
      setMessage(text.trim() === "" ? t("apoSetup.cancelled") : text.trim());
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  function feedback() {
    return (
      <>
        {error !== null && <p className="warn">{error}</p>}
        {message !== null && <pre className="setup-output">{message}</pre>}
      </>
    );
  }

  return (
    <div className="panel">
      <h2>{t("apoSetup.title")}</h2>
      <p>{t("apoSetup.intro")}</p>

      <ul className="setup-steps">
        <Step done={status.registered_dll !== null && status.dll_present}
              label={t("apoSetup.stepRegister")} />
        <Step done={status.gate_open} label={t("apoSetup.stepGate")} />
        <Step done={attached && !inert}
              label={t("apoSetup.stepAttach", { device: endpointName ?? "" })} />
      </ul>

      {/* The gate reduces a machine-wide security mitigation, so it gets its own explanation
          rather than being folded into a general "set up" button. The Equalizer APO context
          matters: it makes this a known trade rather than something CAGEq invented. */}
      {status.next_step === "open-gate" && (
        <p className="warn">{t("apoSetup.gateWarning")}</p>
      )}

      {inert && <p className="warn">{t("apoSetup.effectsDisabled")}</p>}

      {!status.helper_available && <p className="warn">{t("apoSetup.helperMissing")}</p>}

      {endpointId === null ? (
        <p>{t("apoSetup.pickDevice")}</p>
      ) : needsRestart ? (
        <p className="ok">{t("apoSetup.restartNeeded")}</p>
      ) : status.next_step !== null ? (
        <>
          <p>{status.next_step_description}</p>
          <button
            type="button"
            disabled={busy || !status.helper_available}
            onClick={() => void run(status.next_step!)}
          >
            {busy ? t("apoSetup.working") : t("apoSetup.doStep")}
          </button>
          <p className="hint">{t("apoSetup.uacHint")}</p>
        </>
      ) : null}

      {feedback()}
    </div>
  );
}

function Step({ done, label }: { done: boolean; label: string }) {
  return (
    <li className={done ? "step done" : "step"}>
      <span aria-hidden="true">{done ? "✓" : "•"}</span> {label}
    </li>
  );
}

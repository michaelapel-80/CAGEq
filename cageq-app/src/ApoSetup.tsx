import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";

/**
 * Setup for CAGEq's own APO (filter.md §5.3c) — a small header trigger plus a pop-up dialog,
 * not an inline panel.
 *
 * It used to sit inline next to the device picker, permanently, even once everything was set
 * up and running — a whole bordered panel spent on a steady state nobody needed to look at
 * twice. Equalizer APO's own setup lives in a separate Configurator app, reachable but out of
 * the way; this follows the same shape, reachable from one small button instead of occupying
 * space in the main flow.
 *
 * The dialog's own rule from the backend is unchanged: **there is exactly one next step at a
 * time, and its order matters.** Attaching a device while Windows still refuses unsigned
 * effects looks like it succeeded and silently does nothing — so it never offers a list of
 * buttons to pick from, it asks the backend what comes next (`next_step`) and shows that.
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
  /** Live playback sample rate for the selected endpoint, if known. */
  sampleRate: number | null;
  /** Whether Equalizer APO itself is enabled for this device (its own DeviceSelector setting,
      independent of anything CAGEq tracks) — needed to tell "EqAPO is handling it" apart from
      "nothing is attached at all, this EQ has zero effect right now". */
  eqapoEnabled: boolean;
  /** Opens the endpoint's page in Windows Sound settings. Lives here rather than in the header
      because the resampler turned out clean and most Bluetooth devices don't offer a rate
      choice anyway — not worth its own permanent icon, just a link inside this dialog. */
  onOpenOutputSettings: () => void;
};

// Windows playback rate, shown compactly (48 kHz, 44.1 kHz, 96 kHz…).
const fmtRate = (hz: number) => `${+(hz / 1000).toFixed(1)} kHz`;

export default function ApoSetup({ endpointId, endpointName, sampleRate, eqapoEnabled, onOpenOutputSettings }: Props) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [status, setStatus] = useState<ApoSetupStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    if (endpointId === null) return;
    setStatus(await invoke<ApoSetupStatus>("apo_setup_status", { endpoint: endpointId }));
  }, [endpointId]);

  // Reading is free (no elevation): fetch once up front so the trigger's color reflects
  // reality before it's ever clicked, then again on every open to catch changes made
  // elsewhere (the elevated helper, or another CAGEq window) while the dialog was closed.
  useEffect(() => {
    void refresh();
  }, [refresh]);
  useEffect(() => {
    if (open) void refresh();
  }, [open, refresh]);

  // Attaching is inherently per-device, so there is nothing to configure without one selected
  // — same condition under which the OS-settings link inside this dialog is also hidden.
  if (endpointId === null) return null;

  const attached = status !== null && status.attached.includes(endpointId);
  const inert = status !== null && status.effects_disabled.includes(endpointId);
  // Setup is complete but the app is still running the other backend: selection happens once
  // at startup, so nothing changes until CAGEq restarts. Saying so explicitly is the
  // difference between "finished" and a user wondering why it made no difference.
  const needsRestart = status !== null && attached && !inert && !status.active_backend_is_apo;
  const settled = status !== null && attached && !inert && status.active_backend_is_apo;
  // Why the trigger looks the way it does — which backend is actually processing audio right
  // now, or why neither is: attached-but-inert, attached while the unsigned-effects gate is
  // still closed (the exact "Windows silently skips the APO" failure mode found during
  // setup-tool testing), the setup helper being missing, or neither CAGEq nor Equalizer APO
  // attached to this device at all — which means every filter in this app currently has zero
  // effect on what's actually being heard, the one case worth a distinct message for.
  const engineReason: "apo" | "eqapo" | "helperMissing" | "broken" | "unattached" =
    status === null
      ? "eqapo"
      : !status.helper_available
        ? "helperMissing"
        : attached && (inert || !status.gate_open)
          ? "broken"
          : settled
            ? "apo"
            : eqapoEnabled
              ? "eqapo"
              : "unattached";
  const engineState: "apo" | "eqapo" | "attention" = engineReason === "apo" || engineReason === "eqapo" ? engineReason : "attention";

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

  const triggerTitle = t(
    {
      apo: "apoSetup.triggerTitleApo",
      eqapo: "apoSetup.triggerTitleEqapo",
      unattached: "apoSetup.triggerTitleUnattached",
      helperMissing: "apoSetup.triggerTitleAttention",
      broken: "apoSetup.triggerTitleAttention",
    }[engineReason],
  );

  return (
    <>
      <button
        type="button"
        className="dev-settings engine-trigger"
        data-state={engineState}
        onClick={() => setOpen(true)}
        title={triggerTitle}
        aria-label={triggerTitle}
      >
        <span className="apo-gear" aria-hidden>
          ⚙
        </span>{" "}
        {t("apoSetup.triggerLabel")}
      </button>

      {open && (
        <div
          onClick={() => setOpen(false)}
          style={{
            position: "fixed",
            inset: 0,
            background: "#0006",
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            zIndex: 10,
          }}
        >
          <div className="modal-card" onClick={(e) => e.stopPropagation()} style={{ maxWidth: "34em" }}>
            <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
              <h2 style={{ marginTop: 0 }}>{t("apoSetup.title")}</h2>
              <button type="button" onClick={() => setOpen(false)} aria-label={t("dialog.cancel")}>
                ×
              </button>
            </div>

            <button
              type="button"
              className="dev-settings"
              onClick={onOpenOutputSettings}
              title={t("header.soundSettings")}
            >
              <span className="dev-gear" aria-hidden>
                ⚙
              </span>{" "}
              {sampleRate != null ? fmtRate(sampleRate) : t("header.soundSettings")}
            </button>

            {status === null ? (
              <p>{t("apoSetup.loading")}</p>
            ) : settled ? (
              <>
                <p className="ok">{t("apoSetup.active", { device: endpointName ?? "" })}</p>
                <button type="button" disabled={busy} onClick={() => void run(`detach ${endpointId}`)}>
                  {t("apoSetup.detach")}
                </button>
              </>
            ) : (
              <>
                <p>{t("apoSetup.intro")}</p>

                {/* Equalizer APO not being attached either means no filter from either engine
                    is currently applied — worth calling out here since the checklist below
                    only tracks CAGEq's own steps and wouldn't otherwise surface that. */}
                {engineReason === "unattached" && <p className="warn">{t("apoSetup.neitherAttached")}</p>}

                <ul className="setup-steps">
                  <Step done={status.registered_dll !== null && status.dll_present} label={t("apoSetup.stepRegister")} />
                  <Step done={status.gate_open} label={t("apoSetup.stepGate")} />
                  <Step done={attached && !inert} label={t("apoSetup.stepAttach", { device: endpointName ?? "" })} />
                </ul>

                {/* The gate reduces a machine-wide security mitigation, so it gets its own
                    explanation rather than being folded into a general "set up" button. The
                    Equalizer APO context makes this a known trade, not something CAGEq invented. */}
                {status.next_step === "open-gate" && <p className="warn">{t("apoSetup.gateWarning")}</p>}

                {inert && <p className="warn">{t("apoSetup.effectsDisabled")}</p>}

                {!status.helper_available && <p className="warn">{t("apoSetup.helperMissing")}</p>}

                {needsRestart ? (
                  <p className="ok">{t("apoSetup.restartNeeded")}</p>
                ) : status.next_step !== null ? (
                  <>
                    <p>{status.next_step_description}</p>
                    <button type="button" disabled={busy || !status.helper_available} onClick={() => void run(status.next_step!)}>
                      {busy ? t("apoSetup.working") : t("apoSetup.doStep")}
                    </button>
                    <p className="hint">{t("apoSetup.uacHint")}</p>
                  </>
                ) : null}
              </>
            )}

            {feedback()}
          </div>
        </div>
      )}
    </>
  );
}

function Step({ done, label }: { done: boolean; label: string }) {
  return (
    <li className={done ? "step done" : "step"}>
      <span aria-hidden="true">{done ? "✓" : "•"}</span> {label}
    </li>
  );
}

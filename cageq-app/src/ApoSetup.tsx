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
 * buttons to pick from, it asks the backend what comes next (`next_step`) and shows that one
 * action in a fixed spot at the bottom of the card, so clicking through several steps in a
 * row doesn't mean chasing a button around the screen as the card reflows.
 */
export type ApoSetupStatus = {
  registered_dll: string | null;
  dll_present: boolean;
  /** Whether the registered DLL is byte-for-byte the one shipped with this build — `false`
   *  means an app update shipped a newer `CAGEqApo.dll` that hasn't been installed yet, so the
   *  OLD one is what's actually running. `register` (the same step first-time setup uses)
   *  fixes it: it always re-copies the shipped DLL, no separate "update" action exists. */
  dll_current: boolean;
  gate_open: boolean;
  machine_ready: boolean;
  attached: string[];
  effects_disabled: string[];
  active_backend_is_apo: boolean;
  /** Command string for the next step, e.g. `"open-gate"` — echoed back to the backend. */
  next_step: string | null;
  next_step_description: string | null;
  helper_available: boolean;
  /** What the *live* control channel looks like right now — `null` with no endpoint selected,
   *  or whenever CAGEq's own engine isn't the active backend at all. Everything else in this
   *  type comes from the registry alone, which is exactly the blind spot this closes: a
   *  registration and an attachment can both read as entirely correct there while the DLL
   *  silently fails to load (see the Rust side's `LiveChannelStatus` for the real incident that
   *  motivated this) — `"no_channel"` also just means nothing is playing right now, which the
   *  UI cannot tell apart from that failure on its own. */
  live_channel: "no_channel" | "stalled" | "processing" | null;
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

export type EngineReason = "apo" | "eqapo" | "helperMissing" | "broken" | "unattached";

/**
 * Which backend is actually processing audio on `endpointId` right now, or why neither is —
 * pure and exported so the one-time "try CAGEq's own engine" nudge (App.tsx) can ask the exact
 * same question this component's trigger colour answers, from its own independent status
 * fetch, without the two ever silently drifting into disagreeing about what "eqapo" means.
 *
 * attached-but-inert, attached while the unsigned-effects gate is still closed (the exact
 * "Windows silently skips the APO" failure mode found during setup-tool testing), the setup
 * helper being missing, or neither CAGEq nor Equalizer APO attached to this device at all —
 * which means every filter in this app currently has zero effect on what's actually being
 * heard, the one case worth a distinct message for.
 */
export function computeEngineReason(status: ApoSetupStatus | null, eqapoEnabled: boolean, endpointId: string): EngineReason {
  const attached = status !== null && status.attached.includes(endpointId);
  const inert = status !== null && status.effects_disabled.includes(endpointId);
  // `active_backend_is_apo` is reconciled against reality on every status read (the backend
  // can now swap live — see `reconcile_backend` on the Rust side), so this flips true right
  // after attaching, on the very next poll. No "restart to take effect" state exists anymore.
  const activeIsApo = status !== null && attached && !inert && status.active_backend_is_apo;
  if (status === null) return "eqapo";
  if (!status.helper_available) return "helperMissing";
  if (attached && (inert || !status.gate_open)) return "broken";
  if (activeIsApo) return "apo";
  return eqapoEnabled ? "eqapo" : "unattached";
}

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
  // While open, keep polling rather than fetching once — `live_channel` is the reason: it's a
  // live heartbeat comparison (see the Rust side's `LiveChannelStatus`), so a one-shot fetch on
  // open would freeze whatever it happened to catch instead of letting someone watch it flip to
  // "processing" the moment they start playback, which is exactly the confirmation this field
  // exists to give. Stops the moment the dialog closes — nobody is looking, and every tick costs
  // a live heartbeat sample.
  useEffect(() => {
    if (!open) return;
    void refresh();
    const id = window.setInterval(() => void refresh(), 1500);
    return () => window.clearInterval(id);
  }, [open, refresh]);

  // Attaching is inherently per-device, so there is nothing to configure without one selected
  // — same condition under which the OS-settings link inside this dialog is also hidden.
  if (endpointId === null) return null;

  const attached = status !== null && status.attached.includes(endpointId);
  const inert = status !== null && status.effects_disabled.includes(endpointId);
  const activeIsApo = status !== null && attached && !inert && status.active_backend_is_apo;
  // An app update shipped a newer CAGEqApo.dll that was never installed — see
  // `ApoSetupStatus.dll_current`'s own doc. Independent of `activeIsApo`: it's readable even
  // before anything is attached, so it also fires for "registered, present, but stale" on its
  // own, not just once running.
  const stale = status !== null && status.dll_present && !status.dll_current;
  // Fully done, nothing to show or do — also requires the DLL being current, so a stale-but-
  // active install doesn't read as finished: it re-enters the checklist below instead, with
  // its own explanation, rather than silently keeping the old build running indefinitely.
  const settled = activeIsApo && !stale;
  const engineReason = computeEngineReason(status, eqapoEnabled, endpointId);
  const engineState: "apo" | "eqapo" | "attention" = engineReason === "apo" || engineReason === "eqapo" ? engineReason : "attention";

  async function run(action: string) {
    setBusy(true);
    setError(null);
    setMessage(null);
    try {
      // One UAC prompt happens inside this call. Cancelling it comes back as an empty
      // string rather than an error, because declining is a choice, not a failure.
      const text = await invoke<string>("apo_setup_run", { action });
      // The checklist below already shows success (a step ticks off) — the helper's own raw
      // output is for troubleshooting, not something a first-time user needs thrown at them,
      // so it's tucked behind a closed <details> rather than always on screen.
      setMessage(text.trim() === "" ? null : text.trim());
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  const triggerTitle = t(
    engineReason === "apo" && stale
      ? "apoSetup.triggerTitleApoStale"
      : {
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
          <div className="modal-card apo-modal" onClick={(e) => e.stopPropagation()}>
            <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
              <h2 style={{ marginTop: 0 }}>{t("apoSetup.title")}</h2>
              <button type="button" onClick={() => setOpen(false)} aria-label={t("dialog.cancel")}>
                ×
              </button>
            </div>

            {/* Everything that describes *why* things are the way they are. Never a button
                here — see the fixed action slot below, always in the same place. */}
            <div className="apo-modal-body">
              {status === null ? (
                <p>{t("apoSetup.loading")}</p>
              ) : settled ? (
                <>
                  <p className="ok">{t("apoSetup.active", { device: endpointName ?? "" })}</p>
                  {/* The registry-only checks above can all read "done" while the DLL silently
                      failed to load — see `ApoSetupStatus.live_channel`'s own doc. This is the
                      one thing on this whole card that reflects the *live* channel rather than
                      HKLM, so it's shown even in the otherwise-quiet settled state. */}
                  {status.live_channel === "processing" && <p className="ok">{t("apoSetup.liveProcessing")}</p>}
                  {status.live_channel === "stalled" && <p className="warn">{t("apoSetup.liveStalled")}</p>}
                  {status.live_channel === "no_channel" && <p>{t("apoSetup.liveNoChannel")}</p>}
                </>
              ) : (
                <>
                  <p>{t("apoSetup.intro")}</p>

                  {/* Equalizer APO not being attached either means no filter from either engine
                      is currently applied — worth calling out here since the checklist below
                      only tracks CAGEq's own steps and wouldn't otherwise surface that. */}
                  {engineReason === "unattached" && <p className="warn">{t("apoSetup.neitherAttached")}</p>}

                  {/* Distinct from "never set up" — an already-attached, working install that
                      just hasn't picked up an app update yet. Shown before the checklist so it
                      reads as the reason Register reappeared, not as a surprise regression. */}
                  {stale && <p className="warn">{t("apoSetup.updateAvailable")}</p>}

                  <ul className="setup-steps">
                    <Step done={status.registered_dll !== null && status.dll_present && status.dll_current} label={t("apoSetup.stepRegister")} />
                    <Step done={status.gate_open} label={t("apoSetup.stepGate")} />
                    <Step done={attached && !inert} label={t("apoSetup.stepAttach", { device: endpointName ?? "" })} />
                  </ul>

                  {/* The gate reduces a machine-wide security mitigation, so it gets its own
                      explanation rather than being folded into a general "set up" button. The
                      Equalizer APO context makes this a known trade, not something CAGEq invented. */}
                  {status.next_step === "open-gate" && <p className="warn">{t("apoSetup.gateWarning")}</p>}

                  {inert && <p className="warn">{t("apoSetup.effectsDisabled")}</p>}

                  {!status.helper_available && <p className="warn">{t("apoSetup.helperMissing")}</p>}

                  {status.next_step !== null && <p className="hint">{status.next_step_description}</p>}
                </>
              )}

              {error !== null && <p className="warn">{error}</p>}
              {message !== null && (
                <details className="setup-details">
                  <summary>{t("apoSetup.detailsSummary")}</summary>
                  <pre className="setup-output">{message}</pre>
                </details>
              )}
            </div>

            {/* The one thing to click, always in the same place regardless of which step this
                is — the whole point of pinning it here rather than wherever the text above
                happens to end. */}
            <div className="row apo-modal-actions" style={{ justifyContent: "flex-end", gap: "0.5em" }}>
              {/* Every registry-level check can read "done" while the live channel never came
                  up (see `live_channel`'s own doc) — `settled` alone offers no way back into the
                  checklist above, so this is the direct fix rather than making someone detach
                  and reattach to get there. `register` is always safe to re-run: it unconditionally
                  re-copies the DLL and re-registers, whatever state it finds. */}
              {status !== null && settled && status.live_channel !== null && status.live_channel !== "processing" && (
                <button type="button" disabled={busy || !status.helper_available} onClick={() => void run("register")}>
                  {busy ? t("apoSetup.working") : t("apoSetup.retryRegister")}
                </button>
              )}
              {status !== null && settled && (
                <button type="button" disabled={busy} onClick={() => void run(`detach ${endpointId}`)}>
                  {t("apoSetup.detach")}
                </button>
              )}
              {status !== null && !settled && status.next_step !== null && (
                <button type="button" disabled={busy || !status.helper_available} onClick={() => void run(status.next_step!)}>
                  {busy ? t("apoSetup.working") : t("apoSetup.doStep")}
                </button>
              )}
            </div>
            {status !== null && !settled && status.next_step !== null && <p className="hint apo-modal-uachint">{t("apoSetup.uacHint")}</p>}

            {/* A shortcut, not part of the setup flow — kept visually secondary so it never
                competes with the action above for attention. */}
            <button type="button" className="apo-modal-footer-link" onClick={onOpenOutputSettings} title={t("header.soundSettings")}>
              ⚙ {sampleRate != null ? fmtRate(sampleRate) : t("header.soundSettings")}
            </button>
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

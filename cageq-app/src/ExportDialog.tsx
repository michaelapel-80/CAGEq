import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { Band, composedCurveDb, logGrid, type ResponseModel } from "./biquad";
import { EqChart, RefCurve, Series } from "./EqChart";
import { parametricEqText } from "./exportFormats";

type ExportFormat = "parametric" | "graphic";
type FixedBandPreset = "10" | "31";

const BAND_COUNT_MIN = 3;
const BAND_COUNT_MAX = 20;
const BAND_COUNT_DEFAULT = 8;
// Re-fit debounce: dragging the band-count slider (or switching the 10-/31-band preset)
// shouldn't fire a ~1-2s SciPy optimization per tick — same reasoning as every other
// live-tunable control that gates an expensive backend call behind a settle delay.
const DEBOUNCE_MS = 300;

type ExportFit = { filters: Band[]; preamp_db: number };

/** §8 mobile export dialog: fits the active slot's full cascade to a phone-friendly band
 *  count — either a free band count (Parametric tab, `fit_export_eq`) or AutoEq's own standard
 *  10-/31-band graphic EQ (Graphic EQ tab, `fit_fixed_band_eq`, fixed ISO-standard Fc/Q, only
 *  gain optimized). Both resolve to the exact same `{filters, preamp_db}` shape and are shown
 *  the same way — a preview chart, an Fc/Gain/Q table (the "manual entry" path), and the
 *  parametric-syntax text AutoEq's own site writes for all three of these presets (see
 *  exportFormats.ts's own doc for why there's no separate dense-curve GraphicEQ format here).
 *  `filters` is the slot's full composed cascade — `result.filters` in App.tsx, the same
 *  `Band[]` the §5.2 chart already draws for the active slot. Reuses the existing `.modal-card`
 *  overlay convention (see the `presetSave` dialog in App.tsx) rather than inventing new modal
 *  chrome. */
/** `model`: how the slot's own `filters` are realised on the desktop (the effective model) — the
 *  curve the export approximates, i.e. what is heard. The *exported* bands are designed (and
 *  previewed) for the receiving app's filter design instead — its own toggle, RBJ by default,
 *  since nearly every EQ app uses RBJ, independent of CAGEq's playback model. */
export function ExportDialog({ filters, model, sampleRate, onClose }: { filters: Band[]; model: ResponseModel; sampleRate?: number; onClose: () => void }) {
  const { t } = useTranslation();
  const [format, setFormat] = useState<ExportFormat>("parametric");
  const [bandCount, setBandCount] = useState(BAND_COUNT_DEFAULT);
  const [fixedBandPreset, setFixedBandPreset] = useState<FixedBandPreset>("31");
  // The receiving app's filter design. RBJ unless the user says their app is warping-corrected.
  const [bandModel, setBandModel] = useState<ResponseModel>("Rbj");
  const [fit, setFit] = useState<ExportFit | null>(null);
  const [fitting, setFitting] = useState(false);
  const [copied, setCopied] = useState(false);
  // Own legend host (App.tsx's `chart-legend-host` convention) — without one, EqChart's inline
  // legend renders as normal-flow content below the chart's own fixed-height box and overflows
  // into whatever follows it in the DOM (reported live: the legend labels overlapping the band
  // table's header row). Giving it reserved space here is the same fix App.tsx's main chart
  // already applies for the identical reason.
  const [legendHost, setLegendHost] = useState<HTMLDivElement | null>(null);

  useEffect(() => {
    if (filters.length === 0) return;
    setFitting(true);
    const method = format === "parametric" ? "export_eq_fit" : "fixed_band_eq_fit";
    const params = format === "parametric" ? { filters, bandCount, bandModel } : { filters, preset: fixedBandPreset, bandModel };
    // Set by this effect's cleanup once a newer fit (or unmount) supersedes this one. The backend
    // call itself can't be cancelled, so an older, slower solve can still resolve after a newer one
    // started — without this it would flip `fitting` off (hiding the spinner) while the newer solve
    // is still running, and briefly show its stale result in place of the one being waited for.
    let superseded = false;
    const h = setTimeout(() => {
      invoke<ExportFit>(method, params)
        // Sorted ascending by Fc — AutoEq's optimizer returns bands in fit order (shelves
        // first, peaking bands not otherwise ordered — the fixed-band presets happen to come
        // back roughly low-to-high already, but not guaranteed to), which reads poorly both
        // in the table and as "Filter 1/2/3..." in the exported text. Sorted once here so
        // every consumer (the table, parametricEqText, the preview curve — order-independent
        // for that one) sees the same canonical order.
        .then((r) => {
          if (!superseded) setFit({ ...r, filters: [...r.filters].sort((a, b) => a.freq_hz - b.freq_hz) });
        })
        .catch(() => {
          if (!superseded) setFit(null);
        })
        .finally(() => {
          if (!superseded) setFitting(false);
        });
    }, DEBOUNCE_MS);
    return () => {
      superseded = true;
      clearTimeout(h);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [format, filters, bandCount, fixedBandPreset, bandModel]);

  // Q omitted for the fixed-band presets — it's constant per preset, so AutoEq's own site
  // doesn't state it either (see parametricEqText's own doc); the free-band-count fit still
  // needs it, since there Q genuinely varies band to band.
  const text = useMemo(() => (fit ? parametricEqText(fit.filters, fit.preamp_db, format === "parametric") : ""), [fit, format]);

  // Preview curves: the fit's own response (solid) against the full cascade it's approximating
  // (dotted reference) — the same fit-vs-ideal visual language EqChart already uses elsewhere
  // (the AutoEq fit vs. its own reference_curve). Both computed client-side via composedCurveDb
  // — the exact function both `fit_export_eq`/`fit_fixed_band_eq` fit against, so there's no
  // second curve implementation to keep in sync (see exportFormats.ts's own doc).
  const previewFreqs = useMemo(() => logGrid(480, 20, 20000), []);
  const series: Series[] = useMemo(
    () => (fit ? [{ id: "export-fit", bands: fit.filters, color: "var(--accent)", label: t("export.fitCurve") }] : []),
    [fit, t],
  );
  const refs: RefCurve[] = useMemo(() => {
    if (!filters.length) return [];
    const curve = composedCurveDb(filters, previewFreqs, model, sampleRate);
    return [
      {
        id: "export-full",
        points: Array.from(previewFreqs, (f, i) => ({ f, db: curve[i] })),
        color: "var(--fg)",
        label: t("export.fullCurve"),
      },
    ];
  }, [filters, previewFreqs, sampleRate, model, t]);

  // How well the exported (reduced-band) fit actually matches the full cascade it's approximating
  // — the same two curves the chart above already draws (`series`/`refs`), just reduced to two
  // numbers instead of a shape someone has to eyeball. RMS is the fit's overall closeness; Max is
  // its worst single point, since that's what the 31-band ripple (or an aggressive band-count cut)
  // can hide inside an otherwise-good RMS. Both computed client-side via the same `composedCurveDb`
  // the backend fit itself targets (see `fit_export_eq`/`fit_fixed_band_eq`'s own doc for why they
  // don't hand back a curve at all) — no second error computation to keep in sync with the backend.
  const fitError = useMemo(() => {
    if (!fit || !filters.length) return null;
    const achieved = composedCurveDb(fit.filters, previewFreqs, bandModel, sampleRate); // exported bands, as the receiving app will run them
    const reference = composedCurveDb(filters, previewFreqs, model, sampleRate);
    let sumSq = 0;
    let max = 0;
    for (let i = 0; i < previewFreqs.length; i++) {
      const err = Math.abs(achieved[i] - reference[i]);
      sumSq += err * err;
      max = Math.max(max, err);
    }
    return { rms: Math.sqrt(sumSq / previewFreqs.length), max };
  }, [fit, filters, previewFreqs, sampleRate, model, bandModel]);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard permission denied/unavailable (e.g. no OS clipboard access) — the text is
      // still right there in the read-only textarea to select and copy by hand.
    }
  };

  return (
    <div
      onClick={onClose}
      style={{ position: "fixed", inset: 0, background: "#0006", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 10 }}
    >
      <div
        className="modal-card"
        onClick={(e) => e.stopPropagation()}
        // maxHeight + its own scroll: a backstop in case the sum of everything below still runs
        // taller than the viewport (nothing to scroll it back into view since the fixed overlay
        // above has no scroll path of its own) — the card itself scrolls instead of silently
        // overflowing past the screen edge. The table/textarea below use a *fixed* height each
        // (not a shrink-then-cap one) so the dialog's own size stays constant regardless of
        // which tab or preset is active — this is just the safety net, not the primary fix.
        //
        // `overflowX: "hidden"` is required, not cosmetic, once `overflowY` is set to anything
        // but `visible`: per the CSS overflow spec, an unset `overflow-x` on an element whose
        // `overflow-y` is non-visible is computed to `auto` too, not left `visible` — so without
        // this, EqChart's hover-cursor readout (`.ss-cursor`, centered on the mouse X position
        // via `transform: translateX(-50%)`) bleeding past the chart's own right edge near the
        // top of its frequency range was enough to pop a real horizontal scrollbar on this card
        // (reported live: triggered by moving the mouse off the chart to the right). Hidden, not
        // auto: there's nothing here that's ever supposed to need horizontal scrolling.
        style={{ maxWidth: "34em", width: "92vw", maxHeight: "85vh", overflowY: "auto", overflowX: "hidden" }}
      >
        <p style={{ marginTop: 0, fontWeight: 600 }}>{t("export.title")}</p>
        <p style={{ fontSize: "0.85em", opacity: 0.75 }}>{t("export.hint")}</p>

        {/* The app-design checkbox shares the format row rather than taking a line of its own —
            this card has a fixed height budget (see its own doc), and a separate row pushed it past
            it again (reported live). Short label; the explanation is in the tooltip. */}
        <div style={{ display: "flex", alignItems: "center", gap: "0.8em" }}>
          <div className="pl-toggle" style={{ flex: 1 }}>
            <button type="button" className={format === "parametric" ? "on" : ""} onClick={() => setFormat("parametric")}>
              {t("export.parametric")}
            </button>
            <button type="button" className={format === "graphic" ? "on" : ""} onClick={() => setFormat("graphic")}>
              {t("export.graphic")}
            </button>
          </div>
          <label
            // lineHeight 1: the root's fixed `line-height: 24px` otherwise gives this small label a full
            // 24px line box — the same height as the switch buttons, so at some display scalings it
            // rounded up past them and grew the row (and the card) by a pixel or two.
            style={{ fontSize: "0.75em", lineHeight: 1, opacity: 0.8, display: "flex", alignItems: "center", gap: "0.35em", flex: "none", whiteSpace: "nowrap" }}
            title={t("export.bandModelHint")}
          >
            <input
              name="export-band-model"
              type="checkbox"
              checked={bandModel === "AnalogMatched"}
              onChange={(e) => setBandModel(e.currentTarget.checked ? "AnalogMatched" : "Rbj")}
              style={{ flex: "none", margin: 0 }}
            />
            {t("export.bandModel")}
          </label>
        </div>

        {format === "parametric" ? (
          <label className="vs-tune-row" style={{ marginTop: "0.6em" }}>
            <span className="vs-tune-label">{t("export.bandCount")}</span>
            <input
              name="band-count"
              type="range"
              min={BAND_COUNT_MIN}
              max={BAND_COUNT_MAX}
              step={1}
              value={bandCount}
              onChange={(e) => setBandCount(Number(e.currentTarget.value))}
            />
            <b>{bandCount}</b>
          </label>
        ) : (
          // Hint lives in `title` (hover), not a permanent paragraph — a visible line here
          // pushed the dialog's total content past its own height budget again (reported live
          // as the scrollbar coming back), the same growth this dialog's other fixed-height
          // choices were already built to avoid.
          <div className="pl-toggle" style={{ marginTop: "0.6em" }} title={t("export.fixedBandHint")}>
            <button type="button" className={fixedBandPreset === "10" ? "on" : ""} onClick={() => setFixedBandPreset("10")}>
              {t("export.band10")}
            </button>
            <button type="button" className={fixedBandPreset === "31" ? "on" : ""} onClick={() => setFixedBandPreset("31")}>
              {t("export.band31")}
            </button>
          </div>
        )}

        <div style={{ height: 160, position: "relative", margin: "0.6em 0" }}>
          {/* The only band series here is the exported fit, drawn in the receiving app's design; the
              slot's own curve arrives as `refs`, already computed in its effective model. */}
          <EqChart model={bandModel} series={series} refs={refs} height={160} screen legendHost={legendHost} />
          {/* Absolutely positioned inside the chart's own fixed-height box, not a line of its
              own — same "nothing here may grow the dialog" discipline as everything else in it
              (see the modal-card's own doc). `pointerEvents: none` so it never steals the
              chart's hover-cursor tracking underneath it. */}
          {fitError && (
            <div style={{ position: "absolute", top: 6, left: 8, fontSize: "0.68em", opacity: 0.75, pointerEvents: "none" }}>
              {t("export.fitError", { rms: fitError.rms.toFixed(2), max: fitError.max.toFixed(2) })}
            </div>
          )}
          {/* Solver-running indicator, in the same reserved corner-overlay style as the fit-error
              label (top-right, so it never collides with it) — a high band count or the 31-band
              preset takes long enough that, with nothing here, the previous result just sat there
              looking current until the new one popped in. `fitting` covers the debounce wait too,
              so it starts the instant the control moves. */}
          {fitting && (
            <div
              role="status"
              style={{ position: "absolute", top: 6, right: 8, display: "flex", alignItems: "center", gap: "0.4em", fontSize: "0.68em", opacity: 0.85, pointerEvents: "none" }}
            >
              <span className="spinner" aria-hidden="true" />
              {t("export.fitting")}
            </div>
          )}
        </div>
        <div ref={setLegendHost} className="chart-legend-host" />

        {fit && (
          // Fixed `height`, not `maxHeight`: a shrink-then-cap box still grows with every extra
          // row up to the cap (reported live as the dialog visibly expanding before the internal
          // scrollbar ever kicks in) — a fixed height scrolls internally from the very first row
          // past it, so the dialog's total size stops depending on the band count/preset at all.
          <div style={{ height: "9.5em", overflowY: "auto", opacity: fitting ? 0.45 : 1, transition: "opacity 0.15s" }}>
            <table className="export-table">
              <thead>
                <tr>
                  <th>{t("export.tableType")}</th>
                  <th>{t("export.tableFc")}</th>
                  <th>{t("export.tableGain")}</th>
                  <th>{t("export.tableQ")}</th>
                </tr>
              </thead>
              <tbody>
                {fit.filters.map((b, i) => (
                  <tr key={i}>
                    <td>{b.kind}</td>
                    <td>{Math.round(b.freq_hz)} Hz</td>
                    <td>
                      {b.gain_db > 0 ? "+" : ""}
                      {b.gain_db.toFixed(1)} dB
                    </td>
                    <td>{b.q.toFixed(2)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

        {/* Always the same fixed row count, regardless of band count/preset — same "no growth
            at all, not just a cap" reasoning as the table's own fixed height just above. The
            textarea's native scrollbar shows the rest. */}
        <textarea
          name="export-text"
          readOnly
          rows={6}
          value={fitting && !fit ? t("export.fitting") : text}
          style={{ width: "100%", fontFamily: "monospace", fontSize: "0.8em", marginTop: "0.6em", opacity: fitting ? 0.45 : 1, transition: "opacity 0.15s" }}
        />
        <button type="button" disabled={!fit || fitting} onClick={copy}>
          {copied ? t("export.copied") : t("export.copy")}
        </button>

        {/* lineHeight 1.35 + a small bottom margin: this 0.75em note otherwise inherits the root's fixed
            24px line-height (3–4 lines of 12px text taking 24px each) — the space the app-design
            checkbox's row needed back to keep the card inside its height budget. */}
        <p style={{ fontSize: "0.75em", lineHeight: 1.35, opacity: 0.7, marginTop: "0.8em", marginBottom: "0.3em" }}>{t("export.impedanceCaveat")}</p>

        <button type="button" onClick={onClose} style={{ marginTop: "0.5em" }}>
          {t("dialog.cancel")}
        </button>
      </div>
    </div>
  );
}

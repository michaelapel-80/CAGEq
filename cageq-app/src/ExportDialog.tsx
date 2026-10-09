import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { Band, composedCurveDb, logGrid, type ResponseModel } from "./biquad";
import { EqChart, RefCurve, Series } from "./EqChart";
import { flatBandCurve, graphicEqCurve, graphicEqText, interpolatePoints, parametricEqText, sliderValues, type SliderPreset } from "./exportFormats";

type ExportFormat = "parametric" | "graphic";
/** Graphic EQ tab: the dense curve, or slider values for a 10-/31-band graphic EQ. */
type GraphicKind = "curve" | SliderPreset;

const BAND_COUNT_MIN = 3;
const BAND_COUNT_MAX = 20;
const BAND_COUNT_DEFAULT = 8;
// Re-fit debounce: dragging the band-count slider shouldn't fire a ~1-2 s optimization per tick
// — same reasoning as every other live-tunable control that gates an expensive backend call
// behind a settle delay.
const DEBOUNCE_MS = 300;

type ExportFit = { filters: Band[]; preamp_db: number };
/** One row of the export table — a parametric band, or a graphic-EQ point (no kind/Q to set). */
type Row = { kind?: string; freq_hz: number; gain_db: number; q?: number };

/** §8 mobile export dialog. Parametric tab: the active slot's full cascade fitted to a free band
 *  count (`fit_export_eq`, a backend solve). Graphic EQ tab: the slot's curve sampled, no solver —
 *  the dense 127-point `GraphicEQ` curve, or slider values for a 10-/31-band graphic EQ (see
 *  exportFormats.ts's module doc for why sampling, not fitting). Every mode is shown the same way:
 *  a preview chart, a table for typing values in by hand, and the text to paste.
 *  `filters` is the slot's full composed cascade — `result.filters` in App.tsx, the same
 *  `Band[]` the §5.2 chart already draws for the active slot. Reuses the existing `.modal-card`
 *  overlay convention (see the `presetSave` dialog in App.tsx) rather than inventing new modal
 *  chrome. */
/** `model`: how the slot's own `filters` are realised on the desktop (the effective model) — the
 *  curve the export approximates, i.e. what is heard. The *exported* parametric bands are designed
 *  (and previewed) for the receiving app's filter design instead — its own toggle, RBJ by default,
 *  since nearly every EQ app uses RBJ, independent of CAGEq's playback model. */
export function ExportDialog({ filters, model, sampleRate, onClose }: { filters: Band[]; model: ResponseModel; sampleRate?: number; onClose: () => void }) {
  const { t } = useTranslation();
  const [format, setFormat] = useState<ExportFormat>("parametric");
  const [bandCount, setBandCount] = useState(BAND_COUNT_DEFAULT);
  const [graphicKind, setGraphicKind] = useState<GraphicKind>("curve");
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
    if (filters.length === 0 || format !== "parametric") return;
    setFitting(true);
    // Set by this effect's cleanup once a newer fit (or unmount) supersedes this one. The backend
    // call itself can't be cancelled, so an older, slower solve can still resolve after a newer one
    // started — without this it would flip `fitting` off (hiding the spinner) while the newer solve
    // is still running, and briefly show its stale result in place of the one being waited for.
    let superseded = false;
    const h = setTimeout(() => {
      invoke<ExportFit>("export_eq_fit", { filters, bandCount, bandModel })
        // Sorted ascending by Fc — AutoEq's optimizer returns bands in fit order (shelves
        // first, peaking bands not otherwise ordered), which reads poorly both in the table and
        // as "Filter 1/2/3..." in the exported text. Sorted once here so every consumer (the
        // table, parametricEqText, the preview curve — order-independent for that one) sees the
        // same canonical order.
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
      // Switching to the graphic tab mid-fit must not leave the spinner up: nothing would clear it.
      setFitting(false);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [format, filters, bandCount, bandModel]);

  const previewFreqs = useMemo(() => logGrid(480, 20, 20000), []);
  // The slot's own curve as heard — what every mode approximates.
  const reference = useMemo(() => (filters.length ? composedCurveDb(filters, previewFreqs, model, sampleRate) : null), [filters, previewFreqs, model, sampleRate]);

  // What the active mode exports, in one shape: the table rows, the text to paste, and the curve the
  // receiving app ends up with (on the preview grid, without any preamp so it lines up with the
  // slot's curve). Parametric comes from the backend fit; the graphic modes are computed right here.
  const output = useMemo((): { rows: Row[]; text: string; achieved: Float64Array; graphic: boolean } | null => {
    if (!filters.length) return null;
    if (format === "parametric") {
      if (!fit) return null;
      return {
        rows: fit.filters,
        text: parametricEqText(fit.filters, fit.preamp_db),
        achieved: composedCurveDb(fit.filters, previewFreqs, bandModel, sampleRate), // as the receiving app will run them
        graphic: false,
      };
    }
    if (graphicKind === "curve") {
      const points = graphicEqCurve(filters, model, sampleRate);
      // The points carry the folded-in preamp; add it back so the preview compares shapes.
      const curveAtPoints = composedCurveDb(filters, Float64Array.from(points, (p) => p.freq_hz), model, sampleRate);
      const shift = curveAtPoints[0] - points[0].gain_db;
      return { rows: points, text: graphicEqText(points), achieved: interpolatePoints(points, previewFreqs).map((v) => v + shift), graphic: true };
    }
    const { bands, preampDb } = sliderValues(filters, graphicKind, model, sampleRate);
    // Q omitted from the text: each app's sliders have their own band shape (see parametricEqText).
    return { rows: bands, text: parametricEqText(bands, preampDb, false), achieved: flatBandCurve(bands, previewFreqs), graphic: true };
  }, [format, fit, graphicKind, filters, model, sampleRate, bandModel, previewFreqs]);
  const text = output?.text ?? "";

  // Preview curves: the export (solid) against the full cascade it's approximating (dotted
  // reference) — the same fit-vs-ideal visual language EqChart already uses elsewhere (the AutoEq
  // fit vs. its own reference_curve). A parametric fit is drawn from its bands in the receiving
  // app's design; the graphic modes have points, not bands, so they arrive as a solid curve instead
  // (the dense curve interpolated, or the sliders as flat bands, as Wavelet would run them).
  const series: Series[] = useMemo(
    () => (format === "parametric" && fit ? [{ id: "export-fit", bands: fit.filters, color: "var(--accent)", label: t("export.fitCurve") }] : []),
    [format, fit, t],
  );
  const refs: RefCurve[] = useMemo(() => {
    if (!reference) return [];
    const out: RefCurve[] = [];
    if (output?.graphic) {
      const a = output.achieved;
      out.push({
        id: "export-graphic",
        points: Array.from(previewFreqs, (f, i) => ({ f, db: a[i] })),
        color: "var(--accent)",
        label: t(graphicKind === "curve" ? "export.curvePreview" : "export.sliderPreview"),
        solid: true,
      });
    }
    out.push({ id: "export-full", points: Array.from(previewFreqs, (f, i) => ({ f, db: reference[i] })), color: "var(--fg)", label: t("export.fullCurve") });
    return out;
  }, [reference, output, previewFreqs, graphicKind, t]);

  // How well the export matches the full cascade it's approximating — the same two curves the chart
  // above draws, reduced to two numbers instead of a shape someone has to eyeball. RMS is the overall
  // closeness; Max the worst single point, which an aggressive band-count cut (or a slider band too
  // wide for the curve's detail) can hide inside an otherwise-good RMS.
  const fitError = useMemo(() => {
    if (!output || !reference) return null;
    let sumSq = 0;
    let max = 0;
    for (let i = 0; i < previewFreqs.length; i++) {
      const err = Math.abs(output.achieved[i] - reference[i]);
      sumSq += err * err;
      max = Math.max(max, err);
    }
    return { rms: Math.sqrt(sumSq / previewFreqs.length), max };
  }, [output, reference, previewFreqs]);

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
            it again (reported live). Short label; the explanation is in the tooltip. Disabled on the
            graphic tab: sampled curve values involve no filter design of the receiving app's. */}
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
            style={{ fontSize: "0.75em", lineHeight: 1, opacity: format === "parametric" ? 0.8 : 0.4, display: "flex", alignItems: "center", gap: "0.35em", flex: "none", whiteSpace: "nowrap" }}
            title={t(format === "parametric" ? "export.bandModelHint" : "export.bandModelGraphicHint")}
          >
            <input
              name="export-band-model"
              type="checkbox"
              disabled={format !== "parametric"}
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
          // Hints live in `title` (hover), not a permanent paragraph — a visible line here
          // pushed the dialog's total content past its own height budget again (reported live
          // as the scrollbar coming back), the same growth this dialog's other fixed-height
          // choices were already built to avoid.
          <div className="pl-toggle" style={{ marginTop: "0.6em" }}>
            <button type="button" className={graphicKind === "curve" ? "on" : ""} title={t("export.curveHint")} onClick={() => setGraphicKind("curve")}>
              {t("export.curve")}
            </button>
            <button type="button" className={graphicKind === "10" ? "on" : ""} title={t("export.sliderHint")} onClick={() => setGraphicKind("10")}>
              {t("export.band10")}
            </button>
            <button type="button" className={graphicKind === "31" ? "on" : ""} title={t("export.sliderHint")} onClick={() => setGraphicKind("31")}>
              {t("export.band31")}
            </button>
          </div>
        )}

        <div style={{ height: 160, position: "relative", margin: "0.6em 0" }}>
          {/* The only band series here is the parametric fit, drawn in the receiving app's design;
              the slot's own curve (and a graphic export's points) arrive as `refs`. */}
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
              label (top-right, so it never collides with it) — a high band count takes long enough
              that, with nothing here, the previous result just sat there looking current until the
              new one popped in. `fitting` covers the debounce wait too, so it starts the instant
              the control moves. */}
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

        {output && (
          // Fixed `height`, not `maxHeight`: a shrink-then-cap box still grows with every extra
          // row up to the cap (reported live as the dialog visibly expanding before the internal
          // scrollbar ever kicks in) — a fixed height scrolls internally from the very first row
          // past it, so the dialog's total size stops depending on the band count/preset at all.
          // Graphic modes show frequency and gain only: there's no filter type or Q to set.
          <div style={{ height: "9.5em", overflowY: "auto", opacity: fitting ? 0.45 : 1, transition: "opacity 0.15s" }}>
            <table className="export-table">
              <thead>
                <tr>
                  {!output.graphic && <th>{t("export.tableType")}</th>}
                  <th>{t("export.tableFc")}</th>
                  <th>{t("export.tableGain")}</th>
                  {!output.graphic && <th>{t("export.tableQ")}</th>}
                </tr>
              </thead>
              <tbody>
                {output.rows.map((b, i) => (
                  <tr key={i}>
                    {!output.graphic && <td>{b.kind}</td>}
                    <td>{Math.round(b.freq_hz)} Hz</td>
                    <td>{b.kind === "HighPass" ? "—" : `${b.gain_db > 0 ? "+" : ""}${b.gain_db.toFixed(1)} dB`}</td>
                    {!output.graphic && <td>{b.q?.toFixed(2)}</td>}
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
          value={fitting && !output ? t("export.fitting") : text}
          style={{ width: "100%", fontFamily: "monospace", fontSize: "0.8em", marginTop: "0.6em", opacity: fitting ? 0.45 : 1, transition: "opacity 0.15s" }}
        />
        <button type="button" disabled={!output || fitting} onClick={copy}>
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

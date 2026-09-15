import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { Band, composedCurveDb, logGrid } from "./biquad";
import { EqChart, RefCurve, Series } from "./EqChart";
import { graphicEqText, parametricEqText } from "./exportFormats";

type ExportFormat = "parametric" | "graphic";

const BAND_COUNT_MIN = 3;
const BAND_COUNT_MAX = 20;
const BAND_COUNT_DEFAULT = 8;
// Re-fit debounce: dragging the band-count slider shouldn't fire a ~1-2s SciPy optimization
// (fit_export_eq) per tick — same reasoning as every other live-tunable slider that gates an
// expensive backend call behind a settle delay rather than the raw onChange stream.
const DEBOUNCE_MS = 300;

type ExportFit = { filters: Band[]; preamp_db: number };

/** §8 mobile export dialog: re-fits the active slot's full cascade to a low band count for a
 *  mobile parametric EQ (Parametric tab), or resamples it exactly with no band-count tradeoff at
 *  all (Graphic tab, see `exportFormats.ts`'s own doc for why that one needs no backend call).
 *  `filters` is the slot's full composed cascade — `result.filters` in App.tsx, the same `Band[]`
 *  the §5.2 chart already draws for the active slot. Reuses the existing `.modal-card` overlay
 *  convention (see the `presetSave` dialog in App.tsx) rather than inventing new modal chrome. */
export function ExportDialog({ filters, sampleRate, onClose }: { filters: Band[]; sampleRate?: number; onClose: () => void }) {
  const { t } = useTranslation();
  const [format, setFormat] = useState<ExportFormat>("parametric");
  const [bandCount, setBandCount] = useState(BAND_COUNT_DEFAULT);
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
    if (format !== "parametric" || filters.length === 0) return;
    setFitting(true);
    const h = setTimeout(() => {
      invoke<ExportFit>("export_eq_fit", { filters, bandCount })
        .then((r) => setFit(r))
        .catch(() => setFit(null))
        .finally(() => setFitting(false));
    }, DEBOUNCE_MS);
    return () => clearTimeout(h);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [format, filters, bandCount]);

  const graphicText = useMemo(() => (filters.length ? graphicEqText(filters, sampleRate) : ""), [filters, sampleRate]);
  const parametricText = useMemo(() => (fit ? parametricEqText(fit.filters, fit.preamp_db) : ""), [fit]);

  // Preview curves: the low-band fit's own response (solid) against the full cascade it's
  // approximating (dotted reference) — the same fit-vs-ideal visual language EqChart already
  // uses elsewhere (the AutoEq fit vs. its own reference_curve). Both computed client-side via
  // composedCurveDb — the exact function `fit_export_eq` fit against, so there's no second curve
  // implementation to keep in sync (see exportFormats.ts's own doc).
  const previewFreqs = useMemo(() => logGrid(480, 20, 20000), []);
  const series: Series[] = useMemo(
    () => (fit ? [{ id: "export-fit", bands: fit.filters, color: "var(--accent)", label: t("export.fitCurve") }] : []),
    [fit, t],
  );
  const refs: RefCurve[] = useMemo(() => {
    if (!filters.length) return [];
    const curve = composedCurveDb(filters, previewFreqs, sampleRate);
    return [
      {
        id: "export-full",
        points: Array.from(previewFreqs, (f, i) => ({ f, db: curve[i] })),
        color: "var(--fg)",
        label: t("export.fullCurve"),
      },
    ];
  }, [filters, previewFreqs, sampleRate, t]);

  const copy = async (text: string) => {
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
        // maxHeight + its own scroll: at a high band count the table + textarea can run taller
        // than the viewport (reported live as the dialog getting cropped, nothing to scroll it
        // back into view since the fixed overlay above has no scroll path of its own) — the card
        // itself scrolls instead of silently overflowing past the screen edge.
        style={{ maxWidth: "34em", width: "92vw", maxHeight: "85vh", overflowY: "auto" }}
      >
        <p style={{ marginTop: 0, fontWeight: 600 }}>{t("export.title")}</p>
        <p style={{ fontSize: "0.85em", opacity: 0.75 }}>{t("export.hint")}</p>

        <div className="pl-toggle">
          <button type="button" className={format === "parametric" ? "on" : ""} onClick={() => setFormat("parametric")}>
            {t("export.parametric")}
          </button>
          <button type="button" className={format === "graphic" ? "on" : ""} onClick={() => setFormat("graphic")}>
            {t("export.graphic")}
          </button>
        </div>

        {format === "parametric" && (
          <>
            <label className="vs-tune-row" style={{ marginTop: "0.6em" }}>
              <span className="vs-tune-label">{t("export.bandCount")}</span>
              <input
                type="range"
                min={BAND_COUNT_MIN}
                max={BAND_COUNT_MAX}
                step={1}
                value={bandCount}
                onChange={(e) => setBandCount(Number(e.currentTarget.value))}
              />
              <b>{bandCount}</b>
            </label>

            <div style={{ height: 160, position: "relative", margin: "0.6em 0" }}>
              <EqChart series={series} refs={refs} height={160} screen legendHost={legendHost} />
            </div>
            <div ref={setLegendHost} className="chart-legend-host" />

            {fit && (
              // Fixed `height`, not `maxHeight`: a shrink-then-cap box still grows with every
              // extra band up to the cap (reported live as the dialog visibly expanding band by
              // band before the internal scrollbar ever kicks in) — a fixed height scrolls
              // internally from the very first row past it, so the dialog's total size stops
              // depending on the band-count slider at all, not just above some threshold.
              <div style={{ height: "9.5em", overflowY: "auto" }}>
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

            {/* Always the same fixed row count, never `fit.filters.length`-dependent — the same
                "no growth at all, not just a cap" reasoning as the table's own fixed height just
                above. The textarea's native scrollbar shows the rest. */}
            <textarea
              readOnly
              rows={6}
              value={fitting && !fit ? t("export.fitting") : parametricText}
              style={{ width: "100%", fontFamily: "monospace", fontSize: "0.8em", marginTop: "0.6em" }}
            />
            <button type="button" disabled={!fit} onClick={() => copy(parametricText)}>
              {copied ? t("export.copied") : t("export.copy")}
            </button>
          </>
        )}

        {format === "graphic" && (
          <>
            <textarea readOnly rows={4} value={graphicText} style={{ width: "100%", fontFamily: "monospace", fontSize: "0.8em", marginTop: "0.6em" }} />
            <button type="button" onClick={() => copy(graphicText)}>
              {copied ? t("export.copied") : t("export.copy")}
            </button>
          </>
        )}

        <p style={{ fontSize: "0.75em", opacity: 0.7, marginTop: "0.8em" }}>{t("export.impedanceCaveat")}</p>

        <button type="button" onClick={onClose} style={{ marginTop: "0.5em" }}>
          {t("dialog.cancel")}
        </button>
      </div>
    </div>
  );
}

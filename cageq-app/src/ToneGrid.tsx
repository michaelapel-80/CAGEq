import { useEffect, useRef, type CSSProperties } from "react";
import { useTranslation } from "react-i18next";
import { Band, FilterKind } from "./biquad";
import { fcHue, logNorm } from "./fcColor";
import { ScrubNumber } from "./ScrubNumber";

/**
 * §5.2 tone-band editor — the keyboard-first, graphic-EQ-style grid (stage 3).
 *
 * Each band is a **vertical column** (kind icon · gain fader · Fc · Q · remove), several
 * side by side like a graphic EQ, sorted left→right by centre frequency. Replaces the
 * v1 placeholder of plain horizontal number fields (custom_filter_editor_ux memory):
 *
 *   • **gain** → a vertical fader (the console feel), with a scrub/typeable readout;
 *   • **Fc / Q** → scrubbable values ({@link ScrubNumber}: drag / type / ↑↓), not knobs;
 *   • fully keyboard-navigable: every cell is a real focusable control, Tab flows column
 *     by column.
 *
 * Columns are sorted for *display* only via an index permutation; callbacks always carry
 * the band's **storage** index, and React keys are the storage index — so a band keeps
 * its DOM identity (and thus keyboard focus) even when an Fc edit slides it past a
 * neighbour. That structurally avoids the index-vs-reorder bug class the HTML mockup had
 * to hand-guard against, so no neighbour-clamping is needed here.
 */

/** A tone band, plus two frontend-only flags:
 *  - `fixed`: the always-present Bass/Treble macro bands (non-removable, type locked);
 *  - `enabled`: `false` bypasses the band (kept in the grid, excluded from what's applied).
 *  Both default off/true when absent. */
export type ToneBand = Band & { fixed?: boolean; enabled?: boolean; macro?: string };

export type ToneGridProps = {
  filters: ToneBand[];
  disabled?: boolean;
  /** Presented, not editable (the AutoEq fit stage): every control is inert, the edit
   *  affordances (kind cycle, bypass, remove, add) drop out, but the strips stay full-contrast. */
  readOnly?: boolean;
  /** The active stage's colour — lights the faders / LEDs / lit columns in the stage's hue,
   *  so the grid matches its stage chip (Fit cyan / Content pink / Tone green). */
  accent?: string;
  /** Live, throttled — every scrub frame / fader move / keystroke. */
  onInput: (index: number, patch: Partial<ToneBand>) => void;
  /** Final, un-throttled — drag release / blur / Enter / enable toggle. */
  onCommit: (index: number, patch: Partial<ToneBand>) => void;
  onAdd: () => void;
  /** Disables just the Add button (existing bands stay fully editable) — e.g. at MAX_BANDS,
   *  where adding one more would exceed what CAGEq's own APO can run. Separate from the
   *  blanket `disabled` above on purpose: that one also freezes editing/removing. */
  addDisabled?: boolean;
  /** Overrides the Add button's tooltip while `addDisabled` — explains *why*, not just that it
   *  won't respond. Falls back to the ordinary "Add band" title when absent. */
  addDisabledTitle?: string;
  onRemove: (index: number) => void;
  /** Storage index of a just-added band to reveal + focus (its Fc), or null. */
  focusIndex?: number | null;
  /** Bumped per add so the focus fires again even when the index repeats. */
  focusNonce?: number;
  /** Storage index of the band to cross-highlight (the pointer is over its chart node). */
  hoverIndex?: number | null;
  /** Reports the band the pointer is over so the chart can echo the highlight; null on leave. */
  onHover?: (index: number | null) => void;
  /** §5.2 solo: storage index of the soloed band (only it is heard within the stage), or null. */
  soloIndex?: number | null;
  /** Toggle solo for a band (its storage index). Absent → no solo affordance. */
  onSolo?: (index: number) => void;
  /** §5.2 isolate: storage index of the band whose region is bandpassed (peaking only), or null. */
  isolateIndex?: number | null;
  /** Toggle isolate for a band (its storage index). Shown only for peaking bands. */
  onIsolate?: (index: number) => void;
};

const KINDS: FilterKind[] = ["Peaking", "LowShelf", "HighShelf"];
const GAIN_MIN = -20;
const GAIN_MAX = 20;
// Gain's tint ramps logarithmically from this floor (dB) up to full at ±GAIN_MAX — the same
// log response Fc/Q already use, so a small boost/cut still registers (no linear dead zone).
const GAIN_TINT_MIN_DB = 1;

// A fixed macro band's Fc is constrained to a sensible window around its corner so its
// label stays meaningful (a "Bass" shelf dragged to 8 kHz is no longer bass). Free bands
// span the whole audible range.
const fcBounds = (f: ToneBand): [number, number] => {
  if (!f.fixed) return [20, 20000];
  switch (f.macro) {
    case "Bass":
      return [40, 250];
    case "Treble":
      return [1500, 8000];
    case "Air":
      return [8000, 16000];
    default:
      return [20, 20000];
  }
};

const fmtHz = (v: number) => (v >= 1000 ? `${+(v / 1000).toFixed(2)}k` : `${Math.round(v)}`);
// Spoken/label form with the unit attached (kHz / Hz) — the compact `fmtHz` "1.2k" would read
// as "1.2k Hz" once a bare " Hz" is appended, so aria strings carry the full unit here instead.
const fmtHzUnit = (v: number) => (v >= 1000 ? `${+(v / 1000).toFixed(2)} kHz` : `${Math.round(v)} Hz`);
const fmtGain = (v: number) => `${v > 0 ? "+" : ""}${v.toFixed(1)}`;

// --- value-reactive tints -------------------------------------------------------------
// A hair of colour so a strip's *setting* reads at a glance, without leaving the instrument
// look: gain warms on a boost / cools on a cut; Fc runs spectral (warm low → cool high, the
// bass→treble metaphor); Q departs either way from a neutral 1.0 — violet as it narrows,
// teal as it widens. Each ramps logarithmically to full tint at the extreme — Fc through its
// hue position, gain/Q through the mix amount — so small settings still register. Applied
// inline as a color-mix into the theme text colour, so it stays legible in either theme.
const WARM = "#e8a020"; // gain boost
const COOL = "#3f9bef"; // gain cut
const Q_NARROW = "#b95cf0"; // high Q — surgical / focused (violet)
const Q_WIDE = "#14b8a6"; // low Q — broad / gentle (teal)
const clamp01 = (v: number) => Math.max(0, Math.min(1, v));
/** A readout tint: `amt`% of the target hue mixed into the theme text colour, set inline so it
 *  beats the base `input` rule's `color` (which outranks a plain `.tg-num` class). */
const tint = (c: string, amt: number): CSSProperties => ({ color: `color-mix(in srgb, var(--fg), ${c} ${Math.round(amt)}%)` });

/** A tiny pictogram of each filter's shape, so the kind reads at a glance (memory: curve
 *  icons, not "PK/LS/HS" text). Click cycles Peaking → LowShelf → HighShelf. */
function KindGlyph({ kind }: { kind: FilterKind }) {
  const d =
    kind === "Peaking"
      ? "M2,11 L7,11 L10,3 L13,11 L18,11"
      : kind === "LowShelf"
        ? "M2,4 L8,4 L11,10 L18,10"
        : "M2,10 L8,10 L11,4 L18,4";
  return (
    <svg viewBox="0 0 20 14" width="20" height="14" aria-hidden="true">
      <path d={d} fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinejoin="round" strokeLinecap="round" />
    </svg>
  );
}

export function ToneGrid({
  filters,
  disabled,
  readOnly,
  accent,
  focusIndex,
  focusNonce,
  hoverIndex,
  onHover,
  soloIndex,
  onSolo,
  isolateIndex,
  onIsolate,
  onInput,
  onCommit,
  onAdd,
  addDisabled,
  addDisabledTitle,
  onRemove,
}: ToneGridProps) {
  const { t } = useTranslation();
  const inert = disabled || readOnly; // no interaction while read-only, even without `disabled`
  // Display order: sort indices by Fc; storage order (and thus the indices we pass back)
  // never changes here, so focus survives a visual reorder.
  const order = filters.map((_, i) => i).sort((a, b) => filters[a].freq_hz - filters[b].freq_hz);

  // A newly-added band (keyboard/Add path) is "born selected": scroll its column into view and
  // flash it. Its Fc field additionally enters edit mode via `beginEditSignal` below, so a
  // frequency can be typed at once (a plain focus wouldn't — the field is readOnly until then).
  // Keyed on the nonce so it re-fires per add; the storage index (= data-idx) survives the sort.
  const gridRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (focusIndex == null) return;
    const col = gridRef.current?.querySelector<HTMLElement>(`[data-idx="${focusIndex}"]`);
    if (!col) return;
    col.scrollIntoView({ block: "nearest", inline: "nearest", behavior: "smooth" });
    col.classList.add("tg-col-flash");
    const t = window.setTimeout(() => col.classList.remove("tg-col-flash"), 900);
    return () => window.clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusNonce]);

  const cycleKind = (i: number, kind: FilterKind) => {
    const next = KINDS[(KINDS.indexOf(kind) + 1) % KINDS.length];
    onCommit(i, { kind: next });
  };

  return (
    <div className={`tg-grid${readOnly ? " tg-readonly" : ""}`} ref={gridRef} role="group" aria-label={t("bands.title")} style={accent ? ({ "--tg": accent } as CSSProperties) : undefined}>
      {order.map((i) => {
        const f = filters[i];
        const macroId = f.fixed ? (f.macro ?? (f.kind === "LowShelf" ? "Bass" : "Treble")) : null;
        const macroLabel = macroId ? t(`macros.${macroId}`) : null;
        const on = f.enabled !== false;
        const name = macroLabel ?? t("bands.bandAt", { hz: fmtHzUnit(f.freq_hz) });
        // Value-reactive tints (see the WARM/COOL block above). The gain readout warms/cools
        // with the setting (log ramp); the fader just shows a static gradient fill up to the thumb.
        const gainTint = f.gain_db >= 0 ? WARM : COOL;
        const gainAmt = logNorm(Math.abs(f.gain_db), GAIN_TINT_MIN_DB, GAIN_MAX) * 100;
        const fillPct = `${((f.gain_db - GAIN_MIN) / (GAIN_MAX - GAIN_MIN)) * 100}%`;
        // Q tints either way from a neutral 1.0 — violet as it narrows, teal as it widens; the
        // amount is the log-distance from 1, normalised to its end (0.1 or 20) so both reach full.
        const qTint = f.q >= 1 ? Q_NARROW : Q_WIDE;
        const qAmt = clamp01(Math.abs(Math.log(f.q)) / (f.q >= 1 ? Math.log(20) : -Math.log(0.1))) * 100;
        return (
          <div
            className={`tg-col${f.fixed ? " tg-col-fixed" : ""}${on ? "" : " tg-col-off"}${hoverIndex === i ? " tg-col-hover" : ""}${
              soloIndex === i || isolateIndex === i
                ? " tg-col-solo"
                : soloIndex != null || isolateIndex != null
                  ? " tg-col-solo-off"
                  : ""
            }`}
            key={i}
            data-idx={i}
            onPointerEnter={() => onHover?.(i)}
            onPointerLeave={() => onHover?.(null)}
          >
            {!readOnly && onSolo && (
              <button
                type="button"
                className={`tg-aud tg-aud-l${soloIndex === i ? " on" : ""}`}
                disabled={disabled}
                aria-pressed={soloIndex === i}
                title={soloIndex === i ? t("bands.unsolo") : t("bands.solo", { name })}
                aria-label={soloIndex === i ? t("bands.unsolo") : t("bands.solo", { name })}
                onClick={() => onSolo(i)}
              >
                S
              </button>
            )}
            {!readOnly && onIsolate && f.kind === "Peaking" && !f.fixed && (
              <button
                type="button"
                className={`tg-aud tg-aud-r${isolateIndex === i ? " on" : ""}`}
                disabled={disabled}
                aria-pressed={isolateIndex === i}
                title={isolateIndex === i ? t("bands.unisolate") : t("bands.isolate", { name })}
                aria-label={isolateIndex === i ? t("bands.unisolate") : t("bands.isolate", { name })}
                onClick={() => onIsolate(i)}
              >
                I
              </button>
            )}
            {readOnly ? (
              <span className={`tg-enable${on ? " on" : ""}`} aria-hidden="true">
                <span className="tg-led" />
              </span>
            ) : (
              <button
                type="button"
                className={`tg-enable${on ? " on" : ""}`}
                disabled={disabled}
                role="switch"
                aria-checked={on}
                title={on ? t("bands.bypass", { name }) : t("bands.enable", { name })}
                aria-label={on ? t("bands.bypass", { name }) : t("bands.enable", { name })}
                onClick={() => onCommit(i, { enabled: !on })}
              >
                <span className="tg-led" aria-hidden="true" />
              </button>
            )}

            {readOnly ? (
              <span className="tg-kind" title={f.kind} aria-label={t("bands.kindFixedAria", { macro: name, kind: f.kind })}>
                <KindGlyph kind={f.kind} />
              </span>
            ) : (
              <button
                type="button"
                className="tg-kind"
                disabled={disabled || f.fixed}
                title={f.fixed ? t("bands.kindFixed", { macro: macroLabel, kind: f.kind }) : t("bands.kindCycle", { kind: f.kind })}
                aria-label={f.fixed ? t("bands.kindFixedAria", { macro: macroLabel, kind: f.kind }) : t("bands.kindCycleAria", { kind: f.kind })}
                onClick={() => cycleKind(i, f.kind)}
              >
                <KindGlyph kind={f.kind} />
              </button>
            )}

            <ScrubNumber
              className="tg-num tg-gain"
              value={f.gain_db}
              min={GAIN_MIN}
              max={GAIN_MAX}
              mode="add"
              arrowStep={0.1}
              decimals={1}
              format={fmtGain}
              disabled={inert}
              style={tint(gainTint, gainAmt)}
              ariaLabel={t("bands.gainAria", { name, unit: "dB" })}
              onInput={(v) => onInput(i, { gain_db: v })}
              onCommit={(v) => onCommit(i, { gain_db: v })}
            />

            <div className="tg-fader-wrap">
              <input
                type="range"
                className="tg-fader"
                min={GAIN_MIN}
                max={GAIN_MAX}
                step={0.1}
                value={f.gain_db}
                disabled={inert}
                style={{ "--fill": fillPct } as CSSProperties}
                aria-label={t("bands.gainFaderAria", { name })}
                title={t("bands.faderTitle")}
                onChange={(e) => onInput(i, { gain_db: Number(e.currentTarget.value) })}
                onPointerUp={() => onCommit(i, { gain_db: f.gain_db })}
                onDoubleClick={() => !inert && onCommit(i, { gain_db: 0 })}
              />
            </div>

            <label className="tg-cell">
              <span className="tg-lbl">{t("bands.fc")}</span>
              <ScrubNumber
                className="tg-num"
                value={f.freq_hz}
                min={fcBounds(f)[0]}
                max={fcBounds(f)[1]}
                mode="mult"
                arrowStep={1.02}
                decimals={0}
                format={fmtHz}
                disabled={inert}
                style={tint(fcHue(f.freq_hz), 100)}
                ariaLabel={t("bands.fcAria", { kind: f.kind, unit: "Hz" })}
                beginEditSignal={focusIndex === i ? focusNonce : undefined}
                onInput={(v) => onInput(i, { freq_hz: v })}
                onCommit={(v) => onCommit(i, { freq_hz: v })}
              />
            </label>

            <label className="tg-cell">
              <span className="tg-lbl">{t("bands.q")}</span>
              <ScrubNumber
                className="tg-num"
                value={f.q}
                min={0.1}
                max={20}
                mode="mult"
                arrowStep={1.05}
                decimals={2}
                disabled={inert}
                style={tint(qTint, qAmt)}
                ariaLabel={t("bands.qAria", { kind: f.kind, hz: fmtHzUnit(f.freq_hz) })}
                onInput={(v) => onInput(i, { q: v })}
                onCommit={(v) => onCommit(i, { q: v })}
              />
            </label>

            {readOnly ? (
              <span className="tg-fixed">{f.kind === "Peaking" ? "PK" : f.kind === "LowShelf" ? "LS" : "HS"}</span>
            ) : f.fixed ? (
              <span className="tg-fixed" title={t("bands.fixedTitle", { macro: macroLabel })}>
                {macroLabel}
              </span>
            ) : (
              <button
                type="button"
                className="tg-remove"
                disabled={disabled}
                title={t("bands.removeTitle")}
                aria-label={t("bands.removeAria", { name })}
                onClick={() => onRemove(i)}
              >
                ✕
              </button>
            )}
          </div>
        );
      })}

      {!readOnly && (
        <button
          type="button"
          className="tg-add"
          disabled={disabled || addDisabled}
          onClick={onAdd}
          title={addDisabled && addDisabledTitle ? addDisabledTitle : t("bands.addTitle")}
        >
          <span aria-hidden="true">＋</span>
          <span className="tg-add-lbl">{t("bands.add")}</span>
        </button>
      )}
    </div>
  );
}

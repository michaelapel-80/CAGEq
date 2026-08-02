import { useEffect, useRef, type CSSProperties } from "react";
import { Band, FilterKind } from "./biquad";
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
  /** The active stage's colour — lights the faders / LEDs / lit columns in the stage's hue,
   *  so the grid matches its stage chip (Fit cyan / Content pink / Tone green). */
  accent?: string;
  /** Live, throttled — every scrub frame / fader move / keystroke. */
  onInput: (index: number, patch: Partial<ToneBand>) => void;
  /** Final, un-throttled — drag release / blur / Enter / enable toggle. */
  onCommit: (index: number, patch: Partial<ToneBand>) => void;
  onAdd: () => void;
  onRemove: (index: number) => void;
  /** Storage index of a just-added band to reveal + focus (its Fc), or null. */
  focusIndex?: number | null;
  /** Bumped per add so the focus fires again even when the index repeats. */
  focusNonce?: number;
  /** Storage index of the band to cross-highlight (the pointer is over its chart node). */
  hoverIndex?: number | null;
  /** Reports the band the pointer is over so the chart can echo the highlight; null on leave. */
  onHover?: (index: number | null) => void;
};

const KINDS: FilterKind[] = ["Peaking", "LowShelf", "HighShelf"];
const GAIN_MIN = -20;
const GAIN_MAX = 20;

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
const fmtGain = (v: number) => `${v > 0 ? "+" : ""}${v.toFixed(1)}`;

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

export function ToneGrid({ filters, disabled, accent, focusIndex, focusNonce, hoverIndex, onHover, onInput, onCommit, onAdd, onRemove }: ToneGridProps) {
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
    <div className="tg-grid" ref={gridRef} role="group" aria-label="Tone filter bands" style={accent ? ({ "--tg": accent } as CSSProperties) : undefined}>
      {order.map((i) => {
        const f = filters[i];
        const macroLabel = f.fixed ? (f.macro ?? (f.kind === "LowShelf" ? "Bass" : "Treble")) : null;
        const on = f.enabled !== false;
        const name = macroLabel ?? `band at ${fmtHz(f.freq_hz)} hertz`;
        return (
          <div
            className={`tg-col${f.fixed ? " tg-col-fixed" : ""}${on ? "" : " tg-col-off"}${hoverIndex === i ? " tg-col-hover" : ""}`}
            key={i}
            data-idx={i}
            onPointerEnter={() => onHover?.(i)}
            onPointerLeave={() => onHover?.(null)}
          >
            <button
              type="button"
              className={`tg-enable${on ? " on" : ""}`}
              disabled={disabled}
              role="switch"
              aria-checked={on}
              title={on ? `Bypass ${name}` : `Enable ${name}`}
              aria-label={`${on ? "Bypass" : "Enable"} ${name}`}
              onClick={() => onCommit(i, { enabled: !on })}
            >
              <span className="tg-led" aria-hidden="true" />
            </button>

            <button
              type="button"
              className="tg-kind"
              disabled={disabled || f.fixed}
              title={f.fixed ? `${macroLabel} macro (${f.kind}) — type is fixed` : `${f.kind} — click to change type`}
              aria-label={f.fixed ? `${macroLabel} macro band (${f.kind})` : `Filter type: ${f.kind}. Activate to cycle.`}
              onClick={() => cycleKind(i, f.kind)}
            >
              <KindGlyph kind={f.kind} />
            </button>

            <ScrubNumber
              className="tg-num tg-gain"
              value={f.gain_db}
              min={GAIN_MIN}
              max={GAIN_MAX}
              mode="add"
              arrowStep={0.1}
              decimals={1}
              format={fmtGain}
              disabled={disabled}
              ariaLabel={`Gain, band at ${fmtHz(f.freq_hz)} hertz`}
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
                disabled={disabled}
                aria-label={`Gain fader, band at ${fmtHz(f.freq_hz)} hertz`}
                onChange={(e) => onInput(i, { gain_db: Number(e.currentTarget.value) })}
                onPointerUp={() => onCommit(i, { gain_db: f.gain_db })}
              />
            </div>

            <label className="tg-cell">
              <span className="tg-lbl">Fc</span>
              <ScrubNumber
                className="tg-num"
                value={f.freq_hz}
                min={fcBounds(f)[0]}
                max={fcBounds(f)[1]}
                mode="mult"
                arrowStep={1.02}
                decimals={0}
                format={fmtHz}
                disabled={disabled}
                ariaLabel={`Centre frequency, ${f.kind} band`}
                beginEditSignal={focusIndex === i ? focusNonce : undefined}
                onInput={(v) => onInput(i, { freq_hz: v })}
                onCommit={(v) => onCommit(i, { freq_hz: v })}
              />
            </label>

            <label className="tg-cell">
              <span className="tg-lbl">Q</span>
              <ScrubNumber
                className="tg-num"
                value={f.q}
                min={0.1}
                max={20}
                mode="mult"
                arrowStep={1.05}
                decimals={2}
                disabled={disabled}
                ariaLabel={`Q, ${f.kind} band at ${fmtHz(f.freq_hz)} hertz`}
                onInput={(v) => onInput(i, { q: v })}
                onCommit={(v) => onCommit(i, { q: v })}
              />
            </label>

            {f.fixed ? (
              <span className="tg-fixed" title={`${macroLabel} macro — always available, can't be removed`}>
                {macroLabel}
              </span>
            ) : (
              <button
                type="button"
                className="tg-remove"
                disabled={disabled}
                title="Remove this band"
                aria-label={`Remove band at ${fmtHz(f.freq_hz)} hertz`}
                onClick={() => onRemove(i)}
              >
                ✕
              </button>
            )}
          </div>
        );
      })}

      <button type="button" className="tg-add" disabled={disabled} onClick={onAdd} title="Add a band in the widest gap (or double-click the chart to place one)">
        <span aria-hidden="true">＋</span>
        <span className="tg-add-lbl">Add</span>
      </button>
    </div>
  );
}

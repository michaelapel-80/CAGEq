import { useEffect, useRef, useState, type CSSProperties } from "react";

/**
 * A numeric value that is **scrubbable, typeable, and arrow-steppable** — the control
 * the §5.2 grid uses for Fc and Q (custom_filter_editor_ux memory: drag-the-value, not a
 * knob; fully keyboard-usable):
 *
 *   • **drag** left/right on the value to scrub it (multiplicative for Fc/Q, additive for
 *     gain) — the EQ-console feel without the imprecision of hitting a pixel on a log axis;
 *   • **click** (no drag) focuses it for direct type-in-place; a drag started from that
 *     edit state still scrubs (it drops out of edit mode on the first move);
 *   • **↑/↓** step it (multiplicatively where `mode==="mult"`, so a fixed % feels equal at
 *     every frequency — the §5.2 log-step rule), so it's fully usable from the keyboard.
 *
 * Emits `onInput` live (every scrub frame / keystroke, so the chart and audio track the
 * edit) and `onCommit` once at the end (blur / Enter / drag release) for a final,
 * un-throttled write. Typed input is **validated, not clamped**: a value outside
 * [min, max] (or garbage) is rejected and the last good value is kept.
 */
export type ScrubNumberProps = {
  value: number;
  onInput: (v: number) => void;
  onCommit: (v: number) => void;
  min: number;
  max: number;
  /** `mult`: drag/arrows scale the value (Fc, Q). `add`: they offset it (gain). */
  mode: "mult" | "add";
  /** Arrow-key step: a factor (mult, e.g. 1.02 = +2%) or an absolute delta (add). */
  arrowStep: number;
  decimals: number;
  format?: (v: number) => string;
  suffix?: string;
  disabled?: boolean;
  ariaLabel: string;
  className?: string;
  /** Extra inline style merged onto the input (after the internal cursor/touch style) — the
   *  grid uses it to pass the value-reactive tint vars (--tint / --tint-amt). */
  style?: CSSProperties;
  /** Bump to programmatically enter type-in-place mode (value selected, ready to type) —
   *  e.g. a freshly-added band focusing its Fc. `readOnly` otherwise swallows keystrokes. */
  beginEditSignal?: number;
};

const DRAG_THRESHOLD_PX = 3;
/** Scrub sensitivity: exp rate per px (mult) — ~200 px spans ≈ 2.2× — and dB per px (add). */
const MULT_PER_PX = 0.004;
const ADD_PER_PX = 0.05;

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

/** Parse typed text, accepting a trailing `k`/`K` as ×1000 (so "4k" reads back as 4000 —
 *  the same shorthand the field shows). Returns null on anything non-numeric. */
function parseTyped(s: string): number | null {
  const m = /^\s*(-?\d*\.?\d+)\s*([kK])?\s*$/.exec(s.replace(",", "."));
  if (!m) return null;
  const n = parseFloat(m[1]);
  if (!Number.isFinite(n)) return null;
  return m[2] ? n * 1000 : n;
}

export function ScrubNumber({
  value,
  onInput,
  onCommit,
  min,
  max,
  mode,
  arrowStep,
  decimals,
  format,
  suffix,
  disabled,
  ariaLabel,
  className,
  style,
  beginEditSignal,
}: ScrubNumberProps) {
  const inputRef = useRef<HTMLInputElement>(null);
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState("");
  const [invalid, setInvalid] = useState(false);
  const invalidTimer = useRef<number | null>(null);
  // Drag bookkeeping for the active pointer, if any. `last` is the most recent scrubbed
  // value, so the final commit doesn't depend on the `value` prop having re-rendered
  // between the last move and the release. `fromEditing` remembers a drag begun while the
  // field was in type-in-place mode (bug: a focused value couldn't be dragged).
  const drag = useRef<{ startX: number; startVal: number; scrubbed: boolean; last: number; fromEditing: boolean } | null>(null);

  const roundClamp = (v: number) => {
    const f = 10 ** decimals;
    return clamp(Math.round(v * f) / f, min, max);
  };
  const shown = format ? format(value) : value.toFixed(decimals);
  // The editable text for a value — always the plain number, never the "4k" shorthand
  // (which can't be re-typed cleanly), so type-in-place round-trips.
  const editText = (v: number) => (decimals === 0 ? String(Math.round(v)) : v.toFixed(decimals));

  const flashInvalid = () => {
    setInvalid(true);
    if (invalidTimer.current != null) window.clearTimeout(invalidTimer.current);
    invalidTimer.current = window.setTimeout(() => setInvalid(false), 700);
  };

  const step = (dir: 1 | -1) => {
    const next = mode === "mult" ? value * arrowStep ** dir : value + arrowStep * dir;
    const r = roundClamp(next);
    onInput(r);
    if (editing) setText(editText(r));
  };

  const beginEdit = () => {
    setText(editText(value));
    setEditing(true);
    requestAnimationFrame(() => inputRef.current?.select());
  };

  // Enter edit mode when the caller bumps the signal (a just-added band made keyboard-ready).
  useEffect(() => {
    if (beginEditSignal == null || disabled) return;
    beginEdit();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [beginEditSignal]);

  const endDrag = (e?: React.PointerEvent<HTMLInputElement>) => {
    const d = drag.current;
    drag.current = null;
    if (!d) return;
    if (e) {
      try {
        (e.target as Element).releasePointerCapture(e.pointerId);
      } catch {
        /* capture may already be lost — nothing to release */
      }
    }
    if (d.scrubbed) onCommit(d.last);
    else if (!d.fromEditing) beginEdit(); // a plain click (no drag) → type-in-place
    // fromEditing && !scrubbed: a click inside the edit field — keep the caret, do nothing.
  };

  const onPointerDown = (e: React.PointerEvent<HTMLInputElement>) => {
    if (disabled) return;
    drag.current = { startX: e.clientX, startVal: value, scrubbed: false, last: value, fromEditing: editing };
    if (!editing) {
      // Block the native focus/selection on press so a horizontal drag scrubs instead of
      // selecting text; a click that never crosses the threshold re-focuses on release.
      e.preventDefault();
      try {
        (e.target as Element).setPointerCapture(e.pointerId);
      } catch {
        /* no capture — move/up still work while over the element, plus the buttons guard */
      }
    }
    // If editing, let the browser place the caret; we capture on the first scrub move.
  };
  const onPointerMove = (e: React.PointerEvent<HTMLInputElement>) => {
    const d = drag.current;
    if (!d) return;
    // A missed pointerup (lost capture) would otherwise leave the drag "stuck", scrubbing
    // on plain hover with no button down. If no button is held, treat it as released.
    if (e.buttons === 0) {
      endDrag(e);
      return;
    }
    const dx = e.clientX - d.startX;
    if (!d.scrubbed && Math.abs(dx) < DRAG_THRESHOLD_PX) return;
    if (!d.scrubbed) {
      d.scrubbed = true;
      if (d.fromEditing) {
        // Transition out of type-in-place into scrubbing: drop edit mode, clear any text
        // selection the press started, and grab the pointer so it tracks outside the field.
        setEditing(false);
        window.getSelection()?.removeAllRanges();
        try {
          (e.target as Element).setPointerCapture(e.pointerId);
        } catch {
          /* best-effort */
        }
      }
    }
    const next = roundClamp(mode === "mult" ? d.startVal * Math.exp(dx * MULT_PER_PX) : d.startVal + dx * ADD_PER_PX);
    d.last = next;
    onInput(next);
  };
  const onPointerUp = (e: React.PointerEvent<HTMLInputElement>) => endDrag(e);
  const onPointerCancel = (e: React.PointerEvent<HTMLInputElement>) => endDrag(e);

  const commitText = () => {
    const parsed = parseTyped(text);
    // Reject invalid or out-of-range input outright (revert to the last good value) rather
    // than silently snapping to the limit — a typo shouldn't quietly become the max.
    if (parsed == null || parsed < min || parsed > max) {
      onCommit(value);
      flashInvalid();
    } else {
      onCommit(roundClamp(parsed));
    }
    setEditing(false);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (disabled) return;
    if (e.key === "ArrowUp") {
      e.preventDefault();
      step(1);
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      step(-1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (editing) commitText();
      inputRef.current?.blur();
    } else if (e.key === "Escape" && editing) {
      e.preventDefault();
      setEditing(false);
      onCommit(value); // discard the buffer, restore committed value
      inputRef.current?.blur();
    }
  };

  return (
    <input
      ref={inputRef}
      className={`${className ?? ""}${invalid ? " scrub-invalid" : ""}`}
      type="text"
      inputMode="decimal"
      role="spinbutton"
      aria-label={ariaLabel}
      aria-valuenow={value}
      aria-valuemin={min}
      aria-valuemax={max}
      aria-invalid={invalid || undefined}
      disabled={disabled}
      readOnly={!editing}
      value={editing ? text : suffix ? `${shown} ${suffix}` : shown}
      title={`${ariaLabel} — drag to scrub, click to type, ↑/↓ to step (${min}…${max})`}
      style={{ cursor: disabled ? "default" : editing ? "text" : "ew-resize", touchAction: "none", ...style }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerCancel}
      onChange={(e) => {
        setText(e.currentTarget.value);
        const parsed = parseTyped(e.currentTarget.value);
        // Live-preview only a valid, in-range value; out-of-range typing previews nothing
        // (and is rejected on commit) rather than snapping the chart to the limit.
        if (parsed != null && parsed >= min && parsed <= max) onInput(roundClamp(parsed));
      }}
      onBlur={() => editing && commitText()}
      onKeyDown={onKeyDown}
    />
  );
}

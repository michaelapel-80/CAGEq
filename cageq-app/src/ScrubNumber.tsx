import { useEffect, useRef, useState, type CSSProperties } from "react";
import { useTranslation } from "react-i18next";

/**
 * A numeric value that is **scrubbable, typeable, and arrow-steppable** — the control
 * the grid uses for Fc and Q (custom_filter_editor_ux memory: drag-the-value, not a
 * knob; fully keyboard-usable):
 *
 *   • **drag** left/right on the value to scrub it (multiplicative for Fc/Q, additive for
 *     gain) — the EQ-console feel without the imprecision of hitting a pixel on a log axis;
 *   • **click** (no drag) focuses it for direct type-in-place; a drag started from that
 *     edit state still scrubs (it drops out of edit mode on the first move);
 *   • **Tab** into it lands in the same type-in-place mode, value selected — focus by keyboard
 *     has no click to tell it apart, and a focused field that swallows typing is a dead end;
 *   • **↑/↓** step it (multiplicatively where `mode==="mult"`, so a fixed % feels equal at
 *     every frequency — the log-step rule), so it's fully usable from the keyboard.
 *
 * Scrubbing and stepping emit `onInput` live (every scrub frame / arrow press, so the chart
 * and audio track the gesture), then `onCommit` once at the end (drag release, or leaving the
 * field) for a final, un-throttled write. **Typed** text is not previewed: half-typed numbers
 * ("1" on the way to "1500") would audibly jump the filter around, so a typed value only takes
 * effect on Enter or blur. It is **validated, not clamped**: a value outside [min, max] (or
 * garbage) is rejected and the last good value is kept. **Escape** abandons the edit and
 * restores the value the field had when editing began, undoing any arrow steps along the way.
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
  /** Form-field name (DevTools flags inputs with neither id nor name). */
  name: string;
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
  name,
  ariaLabel,
  className,
  style,
  beginEditSignal,
}: ScrubNumberProps) {
  const { t } = useTranslation();
  const inputRef = useRef<HTMLInputElement>(null);
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState("");
  const [invalid, setInvalid] = useState(false);
  const invalidTimer = useRef<number | null>(null);
  // The current edit's bookkeeping. `origin`: the value when it began, what Escape restores.
  // `typed`: the text was edited and not yet committed. `live`: the last value arrow steps
  // previewed through `onInput`, or null if none did — then the caller is still at `origin`.
  // Tabbing through a field enters and leaves edit mode touching neither, and the commit that
  // ends an edit is a real write for the band grid, so an untouched edit ends without one.
  const edit = useRef<{ origin: number; typed: boolean; live: number | null }>({ origin: 0, typed: false, live: null });
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

  // A typed value not yet committed, if it is a valid one — what an arrow step continues from.
  const typedValue = (): number | null => {
    if (!editing || !edit.current.typed) return null;
    const parsed = parseTyped(text);
    return parsed != null && parsed >= min && parsed <= max ? roundClamp(parsed) : null;
  };

  const step = (dir: 1 | -1) => {
    const base = typedValue() ?? value;
    const next = mode === "mult" ? base * arrowStep ** dir : base + arrowStep * dir;
    let r = roundClamp(next);
    // A multiplicative step can be smaller than the display precision near the bottom of the
    // range (Fc: 2% of 24 Hz is under half a hertz, so it rounds straight back), which left the
    // arrows stuck there; move at least one unit of the shown precision instead.
    if (r === base) r = roundClamp(base + dir * 10 ** -decimals);
    onInput(r);
    if (editing) {
      setText(editText(r));
      edit.current.typed = false;
      edit.current.live = r;
    }
  };

  const beginEdit = () => {
    edit.current = { origin: value, typed: false, live: null };
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
        // Transition out of type-in-place into scrubbing: drop edit mode (and any typed text
        // with it; the scrub starts from the live value and commits on release), clear any text
        // selection the press started, and grab the pointer so it tracks outside the field.
        edit.current = { origin: value, typed: false, live: null };
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

  /** End the edit (Enter / blur): commit the typed value, or the stepped one, or nothing. */
  const commitText = () => {
    const { typed, live } = edit.current;
    if (typed) {
      const parsed = typedValue();
      // Reject invalid or out-of-range input outright rather than silently snapping to the
      // limit — a typo shouldn't quietly become the max. The last good value stays: the one
      // the arrows previewed, which still needs its commit, or else the untouched origin.
      if (parsed == null) {
        if (live != null) onCommit(live);
        flashInvalid();
      } else {
        onCommit(parsed);
      }
    } else if (live != null) {
      onCommit(live);
    }
    // Enter commits and then blurs, and the blur handler still sees `editing` from this render:
    // without this it would commit (and write) the same value a second time.
    edit.current = { origin: value, typed: false, live: null };
    setEditing(false);
  };

  /** Abandon the edit (Escape): back to the value it began with. Only arrow steps reached the
   *  caller, so only they need undoing; typed text was never previewed. */
  const revertEdit = () => {
    const { origin, live } = edit.current;
    if (live != null) onCommit(origin);
    edit.current = { origin, typed: false, live: null };
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
      revertEdit();
      inputRef.current?.blur();
    }
  };

  return (
    <input
      ref={inputRef}
      name={name}
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
      title={disabled ? ariaLabel : t("bands.scrubTitle", { label: ariaLabel, min, max })}
      style={{ cursor: disabled ? "default" : editing ? "text" : "ew-resize", touchAction: "none", ...style }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerCancel}
      // Typing only edits the text; it takes effect on Enter or blur (see the module doc).
      onChange={(e) => {
        edit.current.typed = true;
        setText(e.currentTarget.value);
      }}
      // Keyboard focus (Tab) enters type-in-place. A click never focuses a field that isn't
      // editing (onPointerDown prevents it; the release calls beginEdit itself), so this only
      // catches focus that arrived without one; a repeat beginEdit from that release is harmless.
      onFocus={() => !editing && !disabled && beginEdit()}
      onBlur={() => editing && commitText()}
      onKeyDown={onKeyDown}
    />
  );
}

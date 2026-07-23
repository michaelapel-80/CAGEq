import { useRef, useState } from "react";

/**
 * A numeric value that is **scrubbable, typeable, and arrow-steppable** — the control
 * the §5.2 grid uses for Fc and Q (custom_filter_editor_ux memory: drag-the-value, not a
 * knob; fully keyboard-usable):
 *
 *   • **drag** left/right on the value to scrub it (multiplicative for Fc/Q, additive for
 *     gain) — the EQ-console feel without the imprecision of hitting a pixel on a log axis;
 *   • **click** (no drag) focuses it for direct type-in-place;
 *   • **↑/↓** step it (multiplicatively where `mode==="mult"`, so a fixed % feels equal at
 *     every frequency — the §5.2 log-step rule), so it's fully usable from the keyboard.
 *
 * Emits `onInput` live (every scrub frame / keystroke, so the chart and audio track the
 * edit) and `onCommit` once at the end (blur / Enter / drag release) for a final,
 * un-throttled write.
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
};

const DRAG_THRESHOLD_PX = 3;
/** Scrub sensitivity: exp rate per px (mult) — ~200 px spans ≈ 2.2× — and dB per px (add). */
const MULT_PER_PX = 0.004;
const ADD_PER_PX = 0.05;

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

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
}: ScrubNumberProps) {
  const inputRef = useRef<HTMLInputElement>(null);
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState("");
  // Drag bookkeeping for the active pointer, if any. `last` is the most recent scrubbed
  // value, so the final commit doesn't depend on the `value` prop having re-rendered
  // between the last move and the release.
  const drag = useRef<{ startX: number; startVal: number; scrubbed: boolean; last: number } | null>(null);

  const round = (v: number) => {
    const f = 10 ** decimals;
    return clamp(Math.round(v * f) / f, min, max);
  };
  const shown = format ? format(value) : value.toFixed(decimals);

  const step = (dir: 1 | -1) => {
    const next = mode === "mult" ? value * arrowStep ** dir : value + arrowStep * dir;
    const r = round(next);
    onInput(r);
    if (editing) setText(format ? format(r) : String(r));
  };

  const onPointerDown = (e: React.PointerEvent<HTMLInputElement>) => {
    if (disabled || editing) return;
    // Block the native focus/selection on press so a horizontal drag scrubs instead of
    // selecting text; a click that never crosses the threshold re-focuses on release.
    e.preventDefault();
    // Capture so the drag keeps tracking if the pointer leaves the narrow field; guarded
    // because setPointerCapture throws if the pointer is already gone.
    try {
      (e.target as Element).setPointerCapture(e.pointerId);
    } catch {
      /* no capture — the move/up handlers still work while over the element */
    }
    drag.current = { startX: e.clientX, startVal: value, scrubbed: false, last: value };
  };
  const onPointerMove = (e: React.PointerEvent<HTMLInputElement>) => {
    const d = drag.current;
    if (!d) return;
    const dx = e.clientX - d.startX;
    if (!d.scrubbed && Math.abs(dx) < DRAG_THRESHOLD_PX) return;
    d.scrubbed = true;
    const next = round(mode === "mult" ? d.startVal * Math.exp(dx * MULT_PER_PX) : d.startVal + dx * ADD_PER_PX);
    d.last = next;
    onInput(next);
  };
  const onPointerUp = (e: React.PointerEvent<HTMLInputElement>) => {
    const d = drag.current;
    drag.current = null;
    if (!d) return;
    try {
      (e.target as Element).releasePointerCapture(e.pointerId);
    } catch {
      /* capture may have already been lost — nothing to release */
    }
    if (d.scrubbed) {
      onCommit(d.last);
    } else {
      // A plain click: enter type-in-place mode.
      setText(shown);
      setEditing(true);
      requestAnimationFrame(() => inputRef.current?.select());
    }
  };

  const commitText = () => {
    const parsed = parseFloat(text.replace(",", "."));
    if (Number.isFinite(parsed)) onCommit(round(parsed));
    else onCommit(value); // reject garbage, keep the last good value
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
      className={className}
      type="text"
      inputMode="decimal"
      role="spinbutton"
      aria-label={ariaLabel}
      aria-valuenow={value}
      aria-valuemin={min}
      aria-valuemax={max}
      disabled={disabled}
      readOnly={!editing}
      value={editing ? text : suffix ? `${shown} ${suffix}` : shown}
      title={`${ariaLabel} — drag to scrub, click to type, ↑/↓ to step`}
      style={{ cursor: disabled ? "default" : editing ? "text" : "ew-resize", touchAction: "none" }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onChange={(e) => {
        setText(e.currentTarget.value);
        const parsed = parseFloat(e.currentTarget.value.replace(",", "."));
        if (Number.isFinite(parsed)) onInput(clamp(parsed, min, max)); // live preview while typing
      }}
      onBlur={() => editing && commitText()}
      onKeyDown={onKeyDown}
    />
  );
}

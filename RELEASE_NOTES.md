A round of small robustness and accessibility fixes.

- **Fixed:** cageq-apo now sanitizes non-finite (NaN/Inf) input samples before the biquad cascade —
  a bad sample from anywhere upstream (another app, a mixer, an SRC) could otherwise poison the
  filter's carried-forward state permanently, with no recovery until restart.
- **Fixed:** a stalled meter/spectrum/scope stream (a dropped subscriber, a failed fetch) could
  freeze silently until the app was restarted. Streams now self-heal a few seconds after going
  quiet, and stopping/restarting monitoring can no longer race itself.
- **Fixed:** the tone generator window could default to the wrong output device instead of
  whatever the main window actually has selected.
- Cleared several accessibility warnings (unlabeled form fields, missing field names).
- Trimmed some unnecessary vertical padding the app window no longer needs.

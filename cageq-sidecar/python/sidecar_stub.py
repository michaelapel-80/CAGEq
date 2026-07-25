#!/usr/bin/env python3
"""Minimal CAGEq sidecar STUB — stands in for the real AutoEq DSP engine.

It speaks the exact line-delimited JSON-RPC 2.0 that the Rust core
(`cageq-sidecar`) expects: one request object per line on stdin, one response
object per line on stdout. That is enough to exercise the whole transport
end-to-end without pulling in NumPy/SciPy/AutoEq. The real sidecar keeps this
read/dispatch/flush loop and only replaces the method bodies in `handle`.

Run manually to poke at it:
    C:\\Python314\\python.exe -u sidecar_stub.py
    {"jsonrpc":"2.0","id":1,"method":"ping","params":null}
"""
import sys
import os
import time
import json
import threading


def handle(method, params):
    """Return the JSON-RPC *result* for a method, or raise KeyError if unknown."""
    if method == "ping":
        return {"pong": True}

    if method == "sleep_ms":
        # Block this long before replying — stands in for a slow AutoEq fit, and
        # lets the watchdog tests drive the busy-mode response timeout.
        ms = int(params.get("ms", 0))
        time.sleep(ms / 1000.0)
        return {"slept_ms": ms}

    if method == "exit":
        # Simulate a crash: leave *without* replying, so the Rust side sees EOF
        # (SidecarError::Exited). os._exit skips cleanup, like a real hard crash.
        sys.stdout.flush()
        os._exit(int(params.get("code", 1)))

    if method == "die_after_ms":
        # Reply immediately, then crash later from a timer — simulates a process
        # that dies while the Rust side is *idle* (between heartbeats), so only the
        # event-based exit waiter can notice it promptly.
        ms = int(params.get("ms", 0))
        code = int(params.get("code", 1))
        threading.Timer(ms / 1000.0, lambda: os._exit(code)).start()
        return {"scheduled_ms": ms}

    if method == "calculate_filters":
        # Canned answer, shaped exactly like cageq-config-writer's DeviceConfig
        # (device / preamp_db / filters[kind, freq_hz, gain_db, q]). The `kind`
        # strings match the Rust FilterType variant names on purpose, so the real
        # engine's output can later deserialize straight into DeviceConfig.
        return {
            "device": params.get("device", "USB DAC"),
            "preamp_db": -6.5,
            # Canned bands plus any `custom_filters` the caller passed, mirroring the
            # real engine's append behaviour. That lets tests build genuinely different
            # curves (needed to exercise the §5.3a tonal morph) without a DSP install.
            "filters": [
                {"kind": "LowShelf", "freq_hz": 105.0, "gain_db": 3.0, "q": 0.7},
                {"kind": "Peaking", "freq_hz": 2500.0, "gain_db": -2.4, "q": 1.4},
            ]
            + list(params.get("custom_filters") or []),
        }

    if method == "raw_measurement":
        # A canned raw curve so the nerd overlay degrades gracefully without a DSP install.
        return {"raw_curve": [{"f": 20.0, "db": 4.0}, {"f": 1000.0, "db": 0.0}, {"f": 20000.0, "db": -5.0}]}

    raise KeyError(method)


def reply(rid, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": rid}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()  # critical: piped stdout is block-buffered, so flush per reply


def main():
    for line in sys.stdin:  # yields one line per request; ends at EOF (stdin closed)
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue  # ignore malformed lines rather than crash the loop

        rid = req.get("id")
        method = req.get("method", "")

        if method == "shutdown":
            reply(rid, result={"bye": True})
            return  # clean exit -> Rust side sees the child go away

        try:
            reply(rid, result=handle(method, req.get("params") or {}))
        except KeyError as e:
            reply(rid, error={"code": -32601, "message": f"unknown method: {e.args[0]}"})
        except Exception as e:  # stay alive on unexpected errors; report them
            reply(rid, error={"code": -32603, "message": str(e)})


if __name__ == "__main__":
    main()

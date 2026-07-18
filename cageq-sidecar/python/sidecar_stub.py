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
import json


def handle(method, params):
    """Return the JSON-RPC *result* for a method, or raise KeyError if unknown."""
    if method == "ping":
        return {"pong": True}

    if method == "calculate_filters":
        # Canned answer, shaped exactly like cageq-config-writer's DeviceConfig
        # (device / preamp_db / filters[kind, freq_hz, gain_db, q]). The `kind`
        # strings match the Rust FilterType variant names on purpose, so the real
        # engine's output can later deserialize straight into DeviceConfig.
        return {
            "device": params.get("device", "USB DAC"),
            "preamp_db": -6.5,
            "filters": [
                {"kind": "LowShelf", "freq_hz": 105.0, "gain_db": 3.0, "q": 0.7},
                {"kind": "Peaking", "freq_hz": 2500.0, "gain_db": -2.4, "q": 1.4},
            ],
        }

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

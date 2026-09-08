//! Push a correction down the live control channel — the writer side, for verifying on a VM
//! that the channel actually works end to end before CAGEq itself can drive it.
//!
//! Deliberately reuses `cageq_apo`'s own `dsp::coefficients` and `channel::publish` rather
//! than reimplementing either. The point of the exercise is to prove the *real* protocol and
//! the *real* security descriptor work across a process boundary; a bespoke writer with its
//! own idea of the struct layout would prove something else.
//!
//! ```text
//! .\push.exe '{6cafe423-...}' -6.0 PK:120:12:1.0 HSC:8000:-3:0.7
//! .\push.exe '{6cafe423-...}' --watch    # just report heartbeat + what is published
//!
//! QUOTE the GUID: PowerShell parses an unquoted {...} as a ScriptBlock and strips the
//! braces, which used to yield a section name that never matched the APO's.
//! ```
//!
//! Run it as an ordinary user — that is the case being tested. Needing elevation would mean
//! the access control is wrong.

use cageq_apo::channel::ControlChannel;
use cageq_apo::control::{self, RawCoeffs, ReadOutcome, Snapshot};
use cageq_apo::dsp::{Band, FilterKind, coefficients};

fn parse_band(spec: &str) -> Result<Band, String> {
    let p: Vec<&str> = spec.split(':').collect();
    if p.len() != 4 {
        return Err(format!("expected TYPE:Fc:gain:Q, got {spec:?}"));
    }
    let kind = match p[0].to_ascii_uppercase().as_str() {
        "PK" => FilterKind::Peaking,
        "LSC" => FilterKind::LowShelf,
        "HSC" => FilterKind::HighShelf,
        "BP" => FilterKind::Bandpass,
        other => return Err(format!("unknown filter type {other:?}")),
    };
    let num = |i: usize, what: &str| {
        p[i].parse::<f64>().map_err(|_| format!("bad {what} in {spec:?}"))
    };
    Ok(Band { kind, freq_hz: num(1, "Fc")?, gain_db: num(2, "gain")?, q: num(3, "Q")? })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: push <endpoint-guid> [<preamp dB> [TYPE:Fc:gain:Q ...]]");
        eprintln!("       push <endpoint-guid> --watch");
        std::process::exit(2);
    }
    // Normalise before anything else, and complain specifically. PowerShell parses an
    // unquoted {...} as a ScriptBlock and strips the braces, so the GUID often arrives bare;
    // worse, a mis-parsed command line can deliver something that is not a GUID at all. Both
    // used to surface as "no control channel", which points the blame squarely at the APO.
    let Some(endpoint) = cageq_apo::config::normalize_endpoint_id(&args[0]) else {
        eprintln!("{:?} is not an endpoint GUID.", args[0]);
        eprintln!();
        eprintln!("QUOTE the GUID — PowerShell treats an unquoted {{...}} as a script block:");
        eprintln!("    .\\push.exe '{{6cafe423-cde5-4ec1-a1e2-e3fcec778349}}' --watch");
        std::process::exit(2);
    };
    let endpoint = &endpoint;

    // The section only exists while the APO is locked, i.e. while a stream is running on that
    // endpoint. Absence is the ordinary "nothing playing" state, not a failure — but it is a
    // very different problem from being denied access to a section that IS there, and
    // collapsing the two into "no channel" made a real VM failure impossible to diagnose.
    let ch = match ControlChannel::open(endpoint) {
        Ok(ch) => ch,
        Err(2) => {
            // ERROR_FILE_NOT_FOUND
            eprintln!("No section named CAGEqApo_{endpoint} exists.");
            eprintln!();
            eprintln!("The APO creates it at LockForProcess and it dies with its last handle,");
            eprintln!("so this means no APO instance is currently locked on that endpoint:");
            eprintln!("  - is audio actually playing on THIS endpoint right now?");
            eprintln!("  - does the APO log show 'control channel: created/attached'?");
            eprintln!("  - is the DLL on this machine current? the section name is derived");
            eprintln!("    from a normalised (lower-cased) GUID, so an older APO build makes");
            eprintln!("    a differently-named section that will never be found here.");
            std::process::exit(1);
        }
        Err(5) => {
            // ERROR_ACCESS_DENIED — the section exists, so the access control is the problem.
            eprintln!("Access denied opening CAGEqApo_{endpoint}.");
            eprintln!("The section EXISTS, so this is the security descriptor, not the APO.");
            eprintln!("Expected: authenticated users may read/write, medium integrity label.");
            eprintln!("Are you running at Low integrity (e.g. from a sandboxed host)?");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Could not open CAGEqApo_{endpoint}: Win32 error {e}.");
            std::process::exit(1);
        }
    };
    println!("opened Global\\CAGEqApo_{endpoint}");
    // Which DLL is actually loaded. The registered copy lives in %ProgramFiles% and is only
    // refreshed by `cageq-apo-setup register`, so a freshly built DLL in a working folder is
    // NOT the one audiodg loads — a distinction that has twice sent a hunt for DSP bugs that
    // were already fixed.
    match cageq_apo::control::build_stamp(ch.block()) {
        Some(stamp) => {
            let age = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().saturating_sub(stamp))
                .unwrap_or(0);
            println!("loaded APO was built {} min ago (stamp {stamp})", age / 60);
        }
        None => println!("loaded APO published no build stamp — it predates this check"),
    }

    if args.get(1).map(String::as_str) == Some("--watch") {
        // Two things worth seeing: that the APO is processing (heartbeat advancing), and what
        // it would read back from the block right now.
        let mut snap = Snapshot::default();
        let mut last = ch.heartbeat();
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let now = ch.heartbeat();
            let outcome = control::try_read(ch.block(), &mut snap);
            let state = match outcome {
                ReadOutcome::Updated(seq) => {
                    format!("seq {seq}, {} band(s), preamp {:.1} dB", snap.band_count, snap.preamp_db)
                }
                other => format!("{other:?}"),
            };
            println!(
                "heartbeat {now} (+{}) — {state}{}",
                now - last,
                if now == last { "   [NOT PROCESSING]" } else { "" },
            );
            last = now;
        }
        return;
    }

    let preamp_db: f64 = match args.get(1) {
        Some(v) => match v.parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("bad preamp {v:?}");
                std::process::exit(2);
            }
        },
        None => 0.0,
    };

    // Coefficients are computed HERE, by the writer, at the endpoint's rate — which the writer
    // has to know. 48 kHz is assumed for this test tool; CAGEq will read the real rate from
    // the endpoint. A mismatch is audible as a frequency-shifted correction, not a failure,
    // which is exactly why the real backend must not guess.
    const ASSUMED_RATE: f64 = 48_000.0;
    let mut coeffs = Vec::new();
    for spec in &args[2..] {
        match parse_band(spec) {
            Ok(b) => {
                let c = coefficients(&b, ASSUMED_RATE);
                coeffs.push(RawCoeffs { b0: c.b0, b1: c.b1, b2: c.b2, a1: c.a1, a2: c.a2 });
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
    }
    if ch.publish(preamp_db, &coeffs, false, false) {
        println!("published: preamp {preamp_db:.1} dB, {} band(s) @ {ASSUMED_RATE} Hz", coeffs.len());

        // Wait for the APO's verdict rather than assuming success. `publish` only checks what
        // a writer can know — finite, stable, in range — while the loudness ceiling applies to
        // the COMBINED chain and is enforced in the engine. A +39 dB filter is perfectly
        // stable, so it publishes happily and is then declined; without this the tool would
        // report success for a correction that never took effect.
        let published = control::sequence(ch.block());
        let mut verdict = None;
        for _ in 0..100 {
            let (seq, code) = control::ack(ch.block());
            if seq == published {
                verdict = Some(code);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        match verdict {
            Some(control::ACK_APPLIED) => {
                println!("APPLIED — the APO took it on its next buffer, no restart or reload.");
            }
            Some(control::ACK_TOO_LOUD) => {
                eprintln!("REFUSED by the engine: the combined chain exceeds the loudness");
                eprintln!("ceiling (+20 dB anywhere). Individually stable filters still stack.");
                eprintln!("The previous correction is still running — nothing was disturbed.");
                std::process::exit(1);
            }
            Some(other) => eprintln!("APO reported an unknown verdict ({other})."),
            None => {
                // No verdict means nothing is consuming the block.
                println!("(no verdict — is audio actually playing? the APO acks on its next buffer)");
            }
        }
    } else {
        // publish() applies the checks a WRITER can make — stability, finiteness, range —
        // so this is the writer catching what the APO would also have refused.
        eprintln!("REFUSED — unstable, non-finite, too many bands, or preamp out of range.");
        std::process::exit(1);
    }
}

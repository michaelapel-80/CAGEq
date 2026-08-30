//! Push a correction down the live control channel — the writer side, for verifying on a VM
//! that the channel actually works end to end before CAGEq itself can drive it.
//!
//! Deliberately reuses `cageq_apo`'s own `dsp::coefficients` and `channel::publish` rather
//! than reimplementing either. The point of the exercise is to prove the *real* protocol and
//! the *real* security descriptor work across a process boundary; a bespoke writer with its
//! own idea of the struct layout would prove something else.
//!
//! ```text
//! push.exe {6cafe423-...} -6.0 PK:120:12:1.0 HSC:8000:-3:0.7
//! push.exe {6cafe423-...} --watch          # just report heartbeat + what is published
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
    let endpoint = &args[0];

    // The section only exists while the APO is locked, i.e. while a stream is running on that
    // endpoint. Absence is the ordinary "nothing playing" state, not a failure.
    let Some(ch) = ControlChannel::open(endpoint) else {
        eprintln!("no control channel for {endpoint}.");
        eprintln!("The APO creates it at LockForProcess — is audio playing on that endpoint,");
        eprintln!("and is CAGEqApo.dll actually loaded there?");
        std::process::exit(1);
    };
    println!("opened Global\\CAGEqApo_{endpoint}");

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

    if ch.publish(preamp_db, &coeffs) {
        println!("published: preamp {preamp_db:.1} dB, {} band(s) @ {ASSUMED_RATE} Hz", coeffs.len());
        println!("(the APO applies it on its next buffer — no restart, no file, no reload)");
    } else {
        // publish() applies the same checks the reader would, so this is the writer being told
        // about something the APO would have refused anyway.
        eprintln!("REFUSED — unstable, non-finite, too many bands, or preamp out of range.");
        std::process::exit(1);
    }
}

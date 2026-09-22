//! Dump every render endpoint's APO registration, in human-readable form: which effect CLSID
//! sits in which slot, whether that CLSID actually resolves to a DLL on disk, and whether the
//! processing-modes declaration Windows silently requires is there — the state that normally
//! means opening `regedit`, guessing which of `MMDevices\Audio\Render\<guid>\FxProperties`'s
//! GUID-named values is which, and cross-referencing `SOFTWARE\Classes\CLSID` by hand.
//!
//! Generic on purpose: every render endpoint, every populated slot, whatever effect is in it —
//! not just CAGEq's own. When a slot happens to be CAGEq's APO, its persisted config and live
//! control-channel state are shown too, since that is state no registry dump can show at all.
//!
//! Every read here is unelevated (see `setup::status`'s own doc), so this needs no admin
//! rights and never changes anything.
//!
//! ```text
//! doctor                          # every render endpoint
//! doctor '{6cafe423-...}'         # just one — QUOTE the GUID, see push.rs's own note
//! ```

#[cfg(windows)]
fn main() {
    let filter = std::env::args().nth(1).and_then(|a| cageq_apo::config::normalize_endpoint_id(&a));
    if std::env::args().nth(1).is_some() && filter.is_none() {
        eprintln!("{:?} is not an endpoint GUID — quote it: '{{...}}'", std::env::args().nth(1).unwrap());
        std::process::exit(2);
    }

    let gate_open = cageq_apo_backend::setup::status().gate_open;
    println!(
        "DisableProtectedAudioDG gate: {}",
        if gate_open { "OPEN" } else { "CLOSED — no unsigned/self-signed APO can load at all until this is set" }
    );

    let endpoints = cageq_backend::list_render_devices();
    if endpoints.is_empty() {
        println!("\nno render endpoints found");
        return;
    }
    for d in &endpoints {
        if let Some(want) = &filter {
            if !d.id.eq_ignore_ascii_case(want) {
                continue;
            }
        }
        win::print_endpoint(&d.id, &d.name);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only — APO registration lives in the Windows registry.");
    std::process::exit(1);
}

/// Everything that touches the registry directly, kept in one place so `main` reads as the
/// generic top-level flow rather than being interleaved with `winreg` calls.
#[cfg(windows)]
mod win {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    /// `SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render` — every render
    /// endpoint's registration lives under here, one subkey per endpoint GUID.
    const RENDER_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";
    /// `SOFTWARE\Classes\CLSID` — where any COM object, APOs included, resolves to a DLL.
    const CLSID_KEY: &str = r"SOFTWARE\Classes\CLSID";

    /// MMDevAPI's per-slot effect-CLSID property. Standard Windows PROPERTYKEY, not anything
    /// CAGEq invented — `setup.rs` writes the same one to attach CAGEq's own APO.
    const FX_CLSID_PROP: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}";
    /// Per-slot supported-processing-modes property. Missing this for a populated slot makes
    /// Windows skip that slot's APO with no error anywhere — the single most common reason an
    /// APO "does nothing" despite being correctly attached.
    const FX_MODES_PROP: &str = "{d3993a3f-99c2-4402-b5ec-a92a0367664b}";
    /// `PKEY_AudioEndpoint_Disable_SysFx` — present means Windows bypasses the endpoint's whole
    /// effect chain, so nothing in any slot runs at all.
    const FX_DISABLE_SYSFX: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";

    struct Slot {
        /// The slot index as Windows names it (`"1"`..`"7"` in practice, but read verbatim
        /// rather than assumed — nothing about the property name limits it).
        number: String,
        clsid: String,
        modes_declared: bool,
    }

    /// What a `{d04e05a6-...},<N>` registry value actually means, for every `N` this tool has
    /// a source for. Two references, both cited inline below: <https://github.com/dechamps/APO>
    /// (§"FxProperties values") for the five processing-stage slots, and Microsoft's own
    /// <https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/implementing-audio-processing-objects>
    /// (the Bluetooth Audio Sample's string table, and the `PKEY_FX_Association` INF sample) for
    /// slots 0 and 3 — neither of which dechamps' table lists, and neither of which is an active
    /// DSP effect despite sharing the same property GUID prefix.
    enum SlotRole {
        /// A real processing stage — LFX/GFX (legacy) or SFX/MFX/EFX (modern). Its value is an
        /// effect CLSID, and Windows requires the processing-modes declaration alongside it.
        Effect { code: &'static str, what: &'static str },
        /// `PKEY_FX_UiClsid`, `,3` — the property-page CLSID shown in the endpoint's
        /// Enhancements/Advanced tab. A real COM object, worth resolving, but not a DSP effect:
        /// no processing-modes declaration applies to it.
        Ui,
        /// `PKEY_FX_Association`, `,0` — declares which KS node type this endpoint's FX entries
        /// apply to. Its value is a `KSNODETYPE_*` GUID, not a COM CLSID, so resolving it against
        /// `SOFTWARE\Classes\CLSID` would be meaningless.
        Association,
    }

    /// `None` for any slot number neither reference above documents: still read and shown, just
    /// without claiming to know what it is, rather than guessing.
    fn slot_role(number: &str) -> Option<SlotRole> {
        Some(match number {
            "0" => SlotRole::Association,
            "1" => SlotRole::Effect { code: "LFX", what: "Local Effect — pre-mix (legacy name for SFX)" },
            "2" => SlotRole::Effect { code: "GFX", what: "Global Effect — post-mix (legacy name for EFX)" },
            "3" => SlotRole::Ui,
            "5" => SlotRole::Effect { code: "SFX", what: "Stream Effect — per application stream, before mixing" },
            "6" => SlotRole::Effect { code: "MFX", what: "Mode Effect — on the mixed audio for a shared mode" },
            "7" => SlotRole::Effect { code: "EFX", what: "Endpoint Effect — on the device signal, after mixing" },
            _ => return None,
        })
    }

    /// The short pipeline code for a slot known to be [`SlotRole::Effect`] — used where the
    /// caller has already filtered to effect slots and just wants the label.
    fn effect_code(number: &str) -> &'static str {
        match slot_role(number) {
            Some(SlotRole::Effect { code, .. }) => code,
            _ => "?",
        }
    }

    /// Is `number` one of the modern three (SFX/MFX/EFX)? Per the same reference: "If any of
    /// SFX, MFX or EFX are present, then LFX and GFX are ignored" — so unlike every other slot,
    /// LFX/GFX being populated does not by itself mean they run.
    fn is_modern(number: &str) -> bool {
        matches!(number, "5" | "6" | "7")
    }
    fn is_legacy(number: &str) -> bool {
        matches!(number, "1" | "2")
    }

    pub fn print_endpoint(endpoint_id: &str, name: &str) {
        println!("\n=== {name} ===");
        println!("  id: {endpoint_id}");

        let Some(fx) = fx_properties(endpoint_id) else {
            println!("  (no FxProperties key — this endpoint has never had any effect attached)");
            return;
        };

        let disabled = fx.get_raw_value(FX_DISABLE_SYSFX).is_ok();
        println!(
            "  effect chain disabled (SysFx bypass): {}{}",
            yn(disabled),
            if disabled { "  <-- nothing in any slot below actually runs" } else { "" }
        );

        let mut slots = populated_slots(&fx);
        if slots.is_empty() {
            println!("  no slots populated — no APO attached here");
            return;
        }
        slots.sort_by_key(|s| s.number.parse::<u32>().unwrap_or(u32::MAX));

        if !disabled {
            let modern: Vec<&Slot> = slots.iter().filter(|s| is_modern(&s.number)).collect();
            let legacy: Vec<&Slot> = slots.iter().filter(|s| is_legacy(&s.number)).collect();
            // The pipeline stage(s) that actually run: legacy only matters when NO modern slot
            // is populated at all, per `is_modern`'s own doc.
            let active = if modern.is_empty() { legacy.clone() } else { modern.clone() };

            if !modern.is_empty() && !legacy.is_empty() {
                println!(
                    "  {} legacy slot(s) populated (LFX/GFX) but IGNORED — SFX/MFX/EFX take priority whenever any of them is present",
                    legacy.len()
                );
            }
            if active.len() > 1 {
                let stages: Vec<&str> = active.iter().map(|s| effect_code(&s.number)).collect();
                println!(
                    "  {} pipeline stage(s) active: {} — each runs in turn on the SAME audio (stream, then mix, then \
                     endpoint), so it is filtered once per stage, not as alternatives",
                    active.len(),
                    stages.join(" -> ")
                );
            }
        }

        for slot in &slots {
            print_slot(endpoint_id, slot);
        }
    }

    fn fx_properties(endpoint_id: &str) -> Option<RegKey> {
        let id = cageq_apo::config::normalize_endpoint_id(endpoint_id)?;
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(format!(r"{RENDER_KEY}\{id}\FxProperties")).ok()
    }

    /// Every slot with a CLSID actually set, discovered from the property names present rather
    /// than assumed from a fixed list — some other APO can occupy a slot CAGEq never touches.
    fn populated_slots(fx: &RegKey) -> Vec<Slot> {
        let prefix = format!("{FX_CLSID_PROP},");
        fx.enum_values()
            .flatten()
            .filter_map(|(name, _)| name.strip_prefix(&prefix).map(str::to_string))
            .filter_map(|number| {
                let clsid = fx.get_value::<String, _>(format!("{FX_CLSID_PROP},{number}")).ok()?;
                let clsid = clsid.trim().to_string();
                if clsid.is_empty() {
                    return None;
                }
                let modes_declared = fx.get_value::<Vec<String>, _>(format!("{FX_MODES_PROP},{number}")).is_ok();
                Some(Slot { number, clsid, modes_declared })
            })
            .collect()
    }

    fn print_slot(endpoint_id: &str, slot: &Slot) {
        let role = slot_role(&slot.number);
        match &role {
            Some(SlotRole::Effect { code, what }) => println!("  slot {} — {code} ({what}):", slot.number),
            Some(SlotRole::Ui) => println!(
                "  slot {} — UI (PKEY_FX_UiClsid: property-page CLSID for the endpoint's Enhancements/Advanced tab, \
                 not a DSP effect):",
                slot.number
            ),
            Some(SlotRole::Association) => {
                // Not a CLSID at all — a KSNODETYPE GUID — so there is nothing under
                // SOFTWARE\Classes\CLSID to resolve it against; printing the raw value is all
                // that is meaningful here.
                println!("  slot {} — Association (PKEY_FX_Association: KS node type this FX config applies to):", slot.number);
                println!(
                    "    value: {}{}",
                    slot.clsid,
                    if is_zero_guid(&slot.clsid) { "  [KSNODETYPE_ANY — applies to every node]" } else { "" }
                );
                return;
            }
            None => println!(
                "  slot {} — undocumented FX property (not a known SFX/MFX/EFX/LFX/GFX/UI/Association slot):",
                slot.number
            ),
        }
        println!("    CLSID: {}{}", slot.clsid, describe_clsid(&slot.clsid).map(|n| format!("  [{n}]")).unwrap_or_default());

        match clsid_dll(&slot.clsid) {
            Some(raw) => {
                println!("    DLL: {raw}");
                // `InprocServer32` is commonly REG_EXPAND_SZ (Windows' own built-in effects
                // register with a literal "%SystemRoot%\..."), and the registry read above
                // hands back that literal string unexpanded — checking it against the
                // filesystem as-is falsely reports every one of those as missing.
                let expanded = expand_env(&raw);
                if expanded != raw {
                    println!("      expands to: {expanded}");
                }
                println!("    present on disk: {}", yn(std::path::Path::new(&expanded).exists()));
            }
            None => println!(
                "    NOT REGISTERED — no InprocServer32 entry for this CLSID under SOFTWARE\\Classes\\CLSID; \
                 Windows will silently skip this slot"
            ),
        }
        // Only a real processing stage is known to need this at all — a UI or undocumented slot
        // might not be a DSP effect in the first place, so warning about it there would be a guess.
        if matches!(role, Some(SlotRole::Effect { .. })) {
            println!("    processing modes declared: {}", yn(slot.modes_declared));
            if !slot.modes_declared {
                println!("      ^ missing this makes Windows skip the APO in this slot with no error anywhere");
            }
        }

        if bare_eq(&slot.clsid, cageq_apo_backend::setup::CLSID) {
            crate::cageq::print_details(endpoint_id);
        }
    }

    /// Is `guid` the all-zero GUID — `KSNODETYPE_ANY` in the node-type vocabulary
    /// `PKEY_FX_Association`'s value is drawn from?
    fn is_zero_guid(guid: &str) -> bool {
        guid.trim().trim_start_matches('{').trim_end_matches('}').chars().all(|c| c == '0' || c == '-')
    }

    fn clsid_dll(clsid: &str) -> Option<String> {
        RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey(format!(r"{CLSID_KEY}\{clsid}\InprocServer32"))
            .ok()
            .and_then(|k| k.get_value::<String, _>("").ok())
    }

    /// Expand `%VAR%` references against the process environment. Just what
    /// `ExpandEnvironmentStringsW` would do for the common case (`%SystemRoot%`,
    /// `%ProgramFiles%`) without adding a Win32 FFI call for it; an unrecognised or unclosed
    /// `%...%` is left exactly as written rather than guessed at.
    fn expand_env(path: &str) -> String {
        let mut out = String::with_capacity(path.len());
        let mut chars = path.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let mut name = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '%' {
                    closed = true;
                    break;
                }
                name.push(c2);
            }
            match closed.then(|| std::env::var(&name)).and_then(Result::ok) {
                Some(value) => out.push_str(&value),
                None => {
                    out.push('%');
                    out.push_str(&name);
                    if closed {
                        out.push('%');
                    }
                }
            }
        }
        out
    }

    /// A friendly name for CLSIDs this tool happens to recognise. `None` for anything else —
    /// printing "unknown effect" would claim knowledge this tool does not have; the CLSID and
    /// DLL path above are already enough to look it up by hand.
    fn describe_clsid(clsid: &str) -> Option<&'static str> {
        if bare_eq(clsid, cageq_apo_backend::setup::CLSID) {
            Some("CAGEq")
        } else if cageq_backend::EQAPO_APO_CLSIDS.iter().any(|c| bare_eq(clsid, c)) {
            Some("Equalizer APO")
        } else {
            None
        }
    }

    fn bare_eq(a: &str, b: &str) -> bool {
        let strip = |s: &str| s.trim().trim_start_matches('{').trim_end_matches('}').to_ascii_lowercase();
        strip(a) == strip(b)
    }

    fn yn(b: bool) -> &'static str {
        if b { "yes" } else { "no" }
    }
}

/// CAGEq-specific detail, shown only for the slot that is actually CAGEq's own APO — the
/// persisted config and live control-channel state that no registry dump can show, since
/// neither lives in the registry.
#[cfg(windows)]
mod cageq {
    use cageq_apo::channel::ControlChannel;
    use cageq_apo::config::ApoConfig;
    use cageq_apo::control::{self, ReadOutcome, Snapshot};
    use cageq_apo::dsp::{self, Coeffs, FilterKind};

    pub fn print_details(endpoint_id: &str) {
        print_persisted_config(endpoint_id);
        print_live_channel(endpoint_id);
    }

    fn print_persisted_config(endpoint_id: &str) {
        print!("\n    persisted config ({}): ", cageq_apo::config::config_path(endpoint_id).display());
        match cageq_apo::config::load(endpoint_id) {
            Ok(None) => println!("none (no correction has ever been saved for this endpoint)"),
            Ok(Some(cfg)) => {
                println!();
                print_config(&cfg, "      ");
            }
            Err(e) => println!("COULD NOT BE READ — {} (line {})", e.reason, e.line),
        }
    }

    fn print_config(cfg: &ApoConfig, indent: &str) {
        println!("{indent}preamp: {:+.2} dB", cfg.preamp_db);
        if cfg.bands.is_empty() {
            println!("{indent}bands: none");
            return;
        }
        println!("{indent}bands ({}):", cfg.bands.len());
        for (i, b) in cfg.bands.iter().enumerate() {
            println!(
                "{indent}  #{:<2} {:<4} {:>9.1} Hz  gain {:+6.2} dB  Q {:.2}",
                i + 1,
                token(b.kind),
                b.freq_hz,
                b.gain_db,
                b.q
            );
        }
    }

    fn print_live_channel(endpoint_id: &str) {
        println!("\n    live control channel (Global\\CAGEqApo_{endpoint_id}):");
        let ch = match ControlChannel::open(endpoint_id) {
            Ok(ch) => ch,
            Err(2) => {
                println!("      no channel — nothing is currently playing on this endpoint (or the APO never loaded)");
                return;
            }
            Err(5) => {
                println!("      ACCESS DENIED opening the section — it exists, so this is the security descriptor, not the APO");
                return;
            }
            Err(e) => {
                println!("      could not open — Win32 error {e}");
                return;
            }
        };

        match control::build_stamp(ch.block()) {
            Some(stamp) => {
                let age = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs().saturating_sub(stamp))
                    .unwrap_or(0);
                println!("      loaded APO built: {} min ago", age / 60);
            }
            None => println!("      loaded APO published no build stamp (predates this check)"),
        }

        let rate = control::sample_rate(ch.block());
        println!(
            "      locked sample rate: {}",
            rate.map(|r| format!("{r} Hz")).unwrap_or_else(|| "not yet published".into())
        );

        let before = ch.heartbeat();
        std::thread::sleep(std::time::Duration::from_millis(80));
        let after = ch.heartbeat();
        println!(
            "      heartbeat: {after} (+{}) — {}",
            after - before,
            if after > before { "PROCESSING" } else { "stalled (silent right now, or the APO is stuck)" }
        );

        let mut snap = Snapshot::default();
        match control::try_read(ch.block(), &mut snap) {
            ReadOutcome::Updated(seq) => {
                println!(
                    "      live payload (seq {seq}): preamp {:+.2} dB, {} band(s), crossfade: {}",
                    snap.preamp_db, snap.band_count, yn(snap.crossfade)
                );
                for i in 0..snap.band_count {
                    let c = snap.coeffs[i];
                    println!(
                        "        #{:<2} raw biquad  b0 {:+.6}  b1 {:+.6}  b2 {:+.6}  a1 {:+.6}  a2 {:+.6}",
                        i + 1,
                        c.b0,
                        c.b1,
                        c.b2,
                        c.a1,
                        c.a2
                    );
                }
                let (ack_seq, ack_code) = control::ack(ch.block());
                if ack_seq == seq {
                    println!(
                        "      engine's verdict on this update: {}",
                        match ack_code {
                            control::ACK_APPLIED => "APPLIED".to_string(),
                            control::ACK_TOO_LOUD =>
                                "REFUSED — combined chain over the loudness ceiling; the PREVIOUS correction is still running"
                                    .to_string(),
                            other => format!("unknown code {other}"),
                        }
                    );
                }

                if let Some(rate) = rate {
                    compare_against_persisted(endpoint_id, rate, &snap);
                }
            }
            ReadOutcome::Unrecognised => println!(
                "      live payload: none published yet — the block is still zeroed since this APO locked.\n\
                 \x20       Normal whenever no app has pushed a live edit this session: the correction\n\
                 \x20       currently running is whatever LockForProcess loaded from the persisted config\n\
                 \x20       above, not from this channel."
            ),
            ReadOutcome::Torn => println!("      live payload: caught mid-write (torn read) — transient, try again"),
            ReadOutcome::Rejected => println!(
                "      live payload: REJECTED — structurally intact but unsafe (bad band count, non-finite\n\
                 \x20       value, unstable filter, or absurd preamp). The APO is refusing it and running\n\
                 \x20       whatever it had before."
            ),
        }
    }

    /// Recompute what the persisted config *should* be producing at the endpoint's live sample
    /// rate, and diff it against what is actually running. A real drift here — config on disk
    /// says one thing, the engine is running another — is exactly the kind of state a registry
    /// dump alone can never show, since neither side of it lives in the registry.
    fn compare_against_persisted(endpoint_id: &str, sample_rate: u32, live: &Snapshot) {
        let Ok(Some(cfg)) = cageq_apo::config::load(endpoint_id) else { return };
        let expected: Vec<Coeffs> = cfg.bands.iter().map(|b| dsp::coefficients(b, sample_rate as f64)).collect();

        let preamp_matches = (live.preamp_db - cfg.preamp_db).abs() < 0.01;
        let count_matches = live.band_count == expected.len();
        let coeffs_match = count_matches
            && expected.iter().zip(live.coeffs.iter()).all(|(e, l)| {
                close(e.b0, l.b0) && close(e.b1, l.b1) && close(e.b2, l.b2) && close(e.a1, l.a1) && close(e.a2, l.a2)
            });

        if preamp_matches && coeffs_match {
            println!("      vs persisted config: MATCH — the live engine is running exactly what is on disk");
        } else {
            println!("      vs persisted config: MISMATCH — a live edit has not been saved, or vice versa");
            if !preamp_matches {
                println!("        preamp differs: live {:+.2} dB vs config {:+.2} dB", live.preamp_db, cfg.preamp_db);
            }
            if !count_matches {
                println!("        band count differs: live {} vs config {}", live.band_count, expected.len());
            } else if !coeffs_match {
                for (i, (e, l)) in expected.iter().zip(live.coeffs.iter()).enumerate() {
                    if !(close(e.b0, l.b0) && close(e.b1, l.b1) && close(e.b2, l.b2) && close(e.a1, l.a1) && close(e.a2, l.a2)) {
                        println!("        band #{} coefficients differ from what the config would produce", i + 1);
                    }
                }
            }
        }
    }

    /// `1e-6` here was measured to be too tight and gives false MISMATCHes: the persisted
    /// config is written with `{:.4}` (see `config::render`), so recomputing coefficients from
    /// it starts from values already rounded to 4 decimal places, not the exact ones the live
    /// push used. Measured directly against `dsp::coefficients` (a small throwaway harness, not
    /// kept in the repo): realistic bands — the actual 21-band correction on a real machine,
    /// Q 0.21–6.00 — round-trip within `2.2e-5` worst case. `1e-4` leaves a comfortable margin
    /// above that while staying far tighter than a genuine mismatch (a different band entirely
    /// differs by orders of magnitude more). Only a pathological band — Q down at the parser's
    /// 0.05 floor right up against Nyquist, a combination `config.rs`'s own comment calls "far
    /// outside what a real correction uses" — pushes the rounding error as high as `6e-3`; such
    /// a band can still produce a false MISMATCH here, which is an accepted tradeoff for a
    /// diagnostic tool rather than a claim of exactness.
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-4
    }

    fn token(kind: FilterKind) -> &'static str {
        match kind {
            FilterKind::Peaking => "PK",
            FilterKind::LowShelf => "LSC",
            FilterKind::HighShelf => "HSC",
            FilterKind::Bandpass => "BP",
        }
    }

    fn yn(b: bool) -> &'static str {
        if b { "yes" } else { "no" }
    }
}

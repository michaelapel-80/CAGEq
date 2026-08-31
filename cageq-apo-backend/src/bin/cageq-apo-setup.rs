//! The elevated half of CAGEq's APO setup — a small executable that does the registry work
//! and exits.
//!
//! Separate from CAGEq itself so that the app never runs as administrator. Elevating a Tauri
//! app means elevating a webview, which is a poor trade for a handful of registry writes; this
//! way each change costs exactly one UAC prompt and nothing long-lived holds those rights.
//!
//! Invoked by [`cageq_apo_backend::setup::run_elevated`], but usable by hand — an installer
//! can call it for the machine-wide steps, and it is the recovery path if the app will not
//! start:
//!
//! ```text
//! cageq-apo-setup register            # register the COM server (needs CAGEqApo.dll alongside)
//! cageq-apo-setup open-gate           # DisableProtectedAudioDG=1  (machine-wide)
//! cageq-apo-setup attach "{guid}"     # attach to one endpoint
//! cageq-apo-setup detach "{guid}"     # and back off again
//! cageq-apo-setup status              # what is set up right now (needs no elevation)
//! ```

use cageq_apo_backend::setup::{self, Action};

fn main() -> std::process::ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // `--log <token>` is added by the parent when it launches us elevated. The token names a
    // file in OUR temp directory (see `setup::log_path_for`) — the parent never supplies a
    // path, because letting an unelevated caller choose where an elevated process writes is a
    // way to turn this helper into an arbitrary-file-write primitive.
    let log = take_log_token(&mut args).and_then(|t| setup::log_path_for(&t));
    let mut out = Output::new(log);

    if args.first().map(String::as_str) == Some("status") {
        print_status(&mut out);
        return out.finish(std::process::ExitCode::SUCCESS);
    }

    let Some(action) = Action::from_argv(&args) else {
        out.line(&usage());
        return out.finish(std::process::ExitCode::from(2));
    };

    // Self-elevate rather than failing with a bare access-denied. The helper is invoked two
    // ways — by the app via `runas` (already elevated) and by hand from a console, where
    // nothing has elevated it.
    //
    // Deliberately NOT a `requireAdministrator` manifest, which is the usual way: that would
    // force a UAC prompt for `status` too, and status is exactly the command that must work
    // without one.
    if !setup::is_elevated() {
        return match setup::run_elevated_self(&action) {
            // The elevated child could not print to this console — it did not have one — so
            // relay what it wrote. Without this the whole operation is silent, which reads as
            // "nothing happened" whether it worked or not.
            Ok(text) => {
                print!("{text}");
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("failed: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }

    match setup::perform(&action) {
        Ok(()) => {
            out.line(&format!("done: {}", action.describe()));
            // The audio service restart does not itself start a stream, and audiodg only loads
            // APOs when one is built. Saying so prevents the "it didn't work" report that is
            // really "nothing was playing yet".
            out.line("Play audio on the endpoint to let Windows load the effect.");
            out.finish(std::process::ExitCode::SUCCESS)
        }
        Err(e) => {
            out.line(&format!("failed: {e}"));
            out.finish(std::process::ExitCode::FAILURE)
        }
    }
}

/// Pull `--log <token>` out of the arguments, leaving the action's own.
fn take_log_token(args: &mut Vec<String>) -> Option<String> {
    let at = args.iter().position(|a| a == "--log")?;
    let token = args.get(at + 1).cloned();
    args.drain(at..=(at + 1).min(args.len() - 1));
    token
}

/// Prints, and also records for the parent when launched elevated.
struct Output {
    log: Option<std::path::PathBuf>,
    buffer: String,
}

impl Output {
    fn new(log: Option<std::path::PathBuf>) -> Self {
        Output { log, buffer: String::new() }
    }

    fn line(&mut self, s: &str) {
        println!("{s}");
        self.buffer.push_str(s);
        self.buffer.push('\n');
    }

    /// Flush to the parent's log, if there is one. A failure to write it is deliberately
    /// ignored: the operation itself already succeeded or failed on its own terms, and losing
    /// the transcript must not change that verdict.
    fn finish(self, code: std::process::ExitCode) -> std::process::ExitCode {
        if let Some(path) = self.log {
            let _ = std::fs::write(path, self.buffer.as_bytes());
        }
        code
    }
}

fn print_status(out: &mut Output) {
    let s = setup::status();

    out.line("MACHINE");
    match &s.registered_dll {
        Some(p) if s.dll_present => {
            // The DLL's own timestamp answers "is the build I made the one that would load?",
            // which the registry path alone does not.
            let age = std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|d| format!(", built {} min ago", d.as_secs() / 60))
                .unwrap_or_default();
            out.line(&format!("  effect registered   yes  {}{age}", p.display()));
        }
        Some(p) => out.line(&format!("  effect registered   NO - registered file is MISSING: {}", p.display())),
        None => out.line("  effect registered   no"),
    }
    out.line(&format!(
        "  unsigned effects    {}",
        if s.gate_open {
            "allowed (DisableProtectedAudioDG=1)"
        } else {
            "BLOCKED - the effect cannot load, whatever else is set up"
        },
    ));

    let eps = setup::endpoints();
    out.line("");
    out.line("PLAYBACK DEVICES");
    if eps.is_empty() {
        out.line("  (none found)");
    }
    for e in &eps {
        // What is attached, spelled out — "no endpoints" told you nothing about what the
        // choices even were.
        let who = match (e.cageq, e.eqapo) {
            (true, true) => "CAGEq + EqualizerAPO",
            (true, false) => "CAGEq",
            (false, true) => "EqualizerAPO",
            (false, false) => "-",
        };
        out.line(&format!("  {:<38} {who}", e.name));
        if e.double_filtered() {
            out.line("      ** BOTH are attached: audio is filtered TWICE and every");
            out.line("         measurement through this device is wrong. Detach one.");
        }
        if e.effects_disabled {
            out.line("      ** effects are switched off for this device, so nothing attached");
            out.line("         to it runs at all.");
        }
    }

    // What to do next, for the first device that is not ready — a list of facts is not the
    // same as knowing whether anything works.
    out.line("");
    if !s.machine_ready() {
        out.line(&format!("NEXT: {}", setup::Action::RegisterServer.describe()));
        if s.registered_dll.is_some() && s.dll_present && !s.gate_open {
            out.line(&format!("NEXT: {}", setup::Action::OpenGate.describe()));
        }
    } else if eps.iter().any(|e| e.cageq && !e.effects_disabled) {
        out.line("READY: CAGEq's engine is attached and can run.");
        out.line("(It is only actually loaded while audio is playing on that device.)");
    } else {
        out.line("NEXT: attach a device - cageq-apo-setup attach \"{device-guid}\"");
        for e in &eps {
            out.line(&format!("      {}  {}", e.id, e.name));
        }
    }
}

fn usage() -> String {
    format!(
        "CAGEq APO setup\n\n\
         Usage:\n  \
         cageq-apo-setup <command> [endpoint-guid]\n\n\
         Commands:\n  \
         status              what is set up right now (no elevation needed)\n  \
         register            {}\n  \
         unregister          {}\n  \
         open-gate           allow Windows to load effects not signed by Microsoft (machine-wide)\n  \
         close-gate          restore Windows' effect-signing requirement\n  \
         attach <guid>       attach the effect to one playback endpoint\n  \
         detach <guid>       remove it from one playback endpoint\n\n\
         Everything except `status` needs administrator rights, and will ask for them.",
        Action::RegisterServer.describe(),
        Action::UnregisterServer.describe(),
    )
}

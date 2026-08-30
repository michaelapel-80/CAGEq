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
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Reading needs no rights, so it is worth having here too: when something is wrong, this
    // is the one command a user can run without a prompt and paste back.
    if args.first().map(String::as_str) == Some("status") {
        print_status();
        return std::process::ExitCode::SUCCESS;
    }

    let Some(action) = Action::from_argv(&args) else {
        eprintln!("{}", usage());
        return std::process::ExitCode::from(2);
    };

    match setup::perform(&action) {
        Ok(()) => {
            println!("done: {}", action.describe());
            // Nothing takes effect until a stream is rebuilt, and the audio service restart
            // this performs does not itself start one. Saying so prevents the "it did not
            // work" report that is really "nothing was playing yet".
            println!("Play audio on the endpoint to let Windows load the effect.");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn print_status() {
    let s = setup::status();
    match &s.registered_dll {
        Some(p) if s.dll_present => println!("effect registered : {}", p.display()),
        Some(p) => println!("effect registered : {}  [MISSING ON DISK]", p.display()),
        None => println!("effect registered : no"),
    }
    println!(
        "unsigned effects  : {}",
        if s.gate_open { "allowed (DisableProtectedAudioDG=1)" } else { "BLOCKED — the effect cannot load" },
    );
    println!("machine ready     : {}", if s.machine_ready() { "yes" } else { "no" });
    if s.attached.is_empty() {
        println!("attached to       : (no endpoints)");
    } else {
        for id in &s.attached {
            let note = if s.effects_disabled.contains(id) {
                "  [effects disabled for this endpoint — it will not run]"
            } else {
                ""
            };
            println!("attached to       : {id}{note}");
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
         open-gate           {}\n  \
         close-gate          {}\n  \
         attach <guid>       attach the effect to one playback endpoint\n  \
         detach <guid>       remove it from one playback endpoint\n\n\
         Everything except `status` needs administrator rights.",
        Action::RegisterServer.describe(),
        Action::UnregisterServer.describe(),
        "allow Windows to load effects not signed by Microsoft (machine-wide)",
        "restore Windows' effect-signing requirement",
    )
}

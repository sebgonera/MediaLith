//! Prints a `/api/plex/sessions` reply, for looking at the console in a state the appliance
//! is not in.
//!
//! Most of the states this card can be in are states nobody can arrange on demand: a
//! software transcode needs a client that asks for one, three simultaneous streams need
//! three people, and "Plex is not claimed" needs an appliance somebody has signed out of.
//! A card that is only ever looked at in the one state that happens to be easy is a card
//! whose other states ship unseen — which is how four CSS faults reached a machine in one
//! afternoon during the redesign, every one of them green on every test here.
//!
//! ```sh
//! mkdir -p /tmp/canned
//! cargo run -p plexosd --example plex-activity-fixture -- three > /tmp/canned/api-plex-sessions.json
//! python3 tools/preview-console.py crates/plexosd/src/ui/console.html 192.168.2.102 8791 /tmp/canned
//! ```
//!
//! Kinds: `hardware`, `software`, `starting`, `direct`, `three`, `sparse`, `idle`,
//! `not-claimed`, `not-running`, and anything else for "Plex is not installed".
//!
//! The document comes from `plexactivity::sample`, so it is produced by the same serialiser
//! the route uses. A fixture written by hand would keep the old spelling of a renamed field
//! and show a preview with a line missing, which reads as a broken card rather than a stale
//! file.

fn main() {
    let kind = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "hardware".to_owned());
    let report = plexosd::plexactivity::sample(&kind);
    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("could not serialise the sample: {error}");
            std::process::exit(1);
        }
    }
}

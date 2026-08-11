//! `plexos-gpu` — report whether hardware transcoding will work on this machine.
//!
//! Runs on MediaLith at boot, and standalone on any Linux system, which is the point: the
//! premise of the whole project can be checked against real hardware before an image
//! exists to check it on.
//!
//! Exit status is meaningful, so `plexos-init` can gate on it without parsing anything:
//! `0` ready, `1` degraded, `2` unavailable.

use std::process::ExitCode;

use plexos_gpu::env::System;
use plexos_gpu::report::{Health, Report};

const USAGE: &str = "\
plexos-gpu — check whether hardware transcoding will work

USAGE:
    plexos-gpu [--json]

OPTIONS:
    --json    Emit the report as JSON for plexosd and the setup UI
    --help    Show this message

EXIT STATUS:
    0    ready        hardware transcoding is available
    1    degraded     works, but something will make it slower or worse
    2    unavailable  Plex will transcode on the CPU
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut json = false;
    for arg in &args {
        match arg.as_str() {
            "--json" => json = true,
            "--help" | "-h" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("plexos-gpu: unrecognised argument {other:?}\n");
                eprint!("{USAGE}");
                return ExitCode::from(64); // EX_USAGE
            }
        }
    }

    let report = Report::generate(&System);

    if json {
        match report.to_json() {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("plexos-gpu: could not serialise report: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        print!("{report}");
    }

    match report.health {
        Health::Ready => ExitCode::SUCCESS,
        Health::Degraded => ExitCode::from(1),
        Health::Unavailable => ExitCode::from(2),
    }
}

//! Prints what `/api/metrics` and `/api/metrics/processes` would answer, on this machine.
//!
//! The activity card is the first thing in this daemon whose whole subject is a *rate*, and
//! a rate cannot be checked by reading the code: the arithmetic is right or wrong against a
//! real `/proc` that is moving while it is read. So this exists to run the sampler against
//! whatever machine it is on and print both replies, with a real interval between the two
//! readings, so the second one carries percentages.
//!
//! It is also how the canned reply for `tools/preview-console.py` is produced. That matters
//! more than it sounds: a preview fed a JSON somebody typed is a preview of a page agreeing
//! with an imagination, and this repository has already paid for one of those — the
//! `resolv.conf` fixture whose comment rules were guessed, which passed its test while the
//! parser returned nonsense on the appliance.
//!
//! ```text
//! cargo run -p plexosd --example metrics-probe            # both replies, to stdout
//! cargo run -p plexosd --example metrics-probe -- metrics # just the metrics reply
//! cargo run -p plexosd --example metrics-probe -- metrics <before>/ <after>/
//! ```
//!
//! Reads `/proc`, `/sys` and calls `statvfs`. It starts nothing, writes nothing, and needs
//! no privilege — so it is safe to run on a development host, which is the point: the
//! appliance is not the only machine this has to be right about.
//!
//! # Reproducing another machine's report from a capture
//!
//! The two-directory form reads a pair of captured trees instead of this machine's `/proc`,
//! which is how the appliance's own numbers can be put through this code without a build and
//! a deploy. Both are needed because one reading is not a rate.
//!
//! The interval is taken from the difference between the two trees' `/proc/uptime` and slept
//! for real, which looks eccentric and is the only way the percentages come out true: the
//! sampler divides by the wall clock it observes, so a pair of readings 27 seconds apart
//! replayed 1 second apart would report every rate 27 times too high — plausible,
//! catastrophic, and invisible.
//!
//! `statvfs` is a syscall rather than a file, so free space in this mode is still about the
//! machine running the probe. Nothing else is.

use std::path::{Path, PathBuf};

use plexos_gpu::env::{Environment, System};
use plexosd::metrics::Sampler;

/// A machine as captured into a directory: every absolute path read under a prefix.
struct Captured(PathBuf);

impl Captured {
    fn at(&self, path: &Path) -> PathBuf {
        self.0.join(path.strip_prefix("/").unwrap_or(path))
    }
}

impl Environment for Captured {
    fn list_dir(&self, path: &Path) -> std::io::Result<Vec<PathBuf>> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(self.at(path))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            // Back to the absolute path the caller asked about, so what it does next -- join
            // a filename and read it -- lands here again rather than on the real machine.
            .map(|entry| path.join(entry.file_name()))
            .collect();
        entries.sort();
        Ok(entries)
    }

    fn read(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(self.at(path))
    }

    fn read_link(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::read_link(self.at(path))
    }

    fn run(&self, program: &str, _args: &[&str]) -> std::io::Result<String> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("a capture cannot run {program}"),
        ))
    }
}

/// Seconds since boot as a capture recorded them.
fn uptime_of(env: &impl Environment) -> f64 {
    env.read(Path::new("/proc/uptime"))
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse().ok())
        .unwrap_or_default()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let which = args.first().cloned().unwrap_or_else(|| "both".to_owned());
    let sampler = Sampler::new();

    let report = |metrics: &dyn Fn() -> String, processes: &dyn Fn() -> String| {
        if which != "processes" {
            println!("{}", metrics());
        }
        if which != "metrics" {
            println!("{}", processes());
        }
    };

    if let (Some(before), Some(after)) = (args.get(1), args.get(2)) {
        let (before, after) = (
            Captured(PathBuf::from(before)),
            Captured(PathBuf::from(after)),
        );

        let _ = sampler.sample(&before);
        let _ = sampler.processes(&before);

        // The real interval between the two captures. See the note at the top: sleeping the
        // wrong amount here does not fail, it silently scales every rate.
        let interval = (uptime_of(&after) - uptime_of(&before)).max(0.3);
        eprintln!("replaying a {interval:.2} s interval between the two captures");
        std::thread::sleep(std::time::Duration::from_secs_f64(interval));

        report(
            &|| serde_json::to_string_pretty(&sampler.sample(&after)).expect("serialises"),
            &|| serde_json::to_string_pretty(&sampler.processes(&after)).expect("serialises"),
        );
        return;
    }

    // The first reading of a since-boot counter is not a rate, by design: it establishes the
    // baseline and reports nulls. Sleeping past the sampler's own floor is what makes the
    // second one a measurement rather than a repeat.
    let _ = sampler.sample(&System);
    let _ = sampler.processes(&System);
    std::thread::sleep(std::time::Duration::from_millis(1200));

    report(
        &|| serde_json::to_string_pretty(&sampler.sample(&System)).expect("serialises"),
        &|| serde_json::to_string_pretty(&sampler.processes(&System)).expect("serialises"),
    );
}

//! What the machine is doing right now.
//!
//! Everything else the console reports is a state: which slot booted, whether a signature
//! verified, what the gate decided. This is the one view that is about a *moment* — how
//! busy the processor is, how much memory is gone, what Plex itself is using, how warm the
//! chassis is. It answers the question somebody asks after the appliance is working:
//! is it coping.
//!
//! # Nothing new is in the image because of this
//!
//! Not one program was added. `top`, `htop`, `iostat` and `sensors` are all absent and all
//! unnecessary: every number here is read out of `/proc` and `/sys`, which are files, and
//! the one figure that is not a file — free space — is a syscall that already lives in
//! [`plexos_sys::fs`]. That matters more here than it looks. "A program in the image is
//! not a program that can do the job" has cost this project three evenings, and a
//! dashboard is exactly the kind of feature that invites a package.
//!
//! # Rates need two readings, and the first one is not a rate
//!
//! `/proc/stat` counts ticks since boot. A percentage is the difference between two
//! readings divided by the time between them, so a single reading cannot produce one — and
//! the tempting thing to do, dividing a since-boot total by uptime, answers a question
//! nobody asked ("how busy has this machine been on average since July"). So the sampler
//! keeps the previous reading and returns differences, and the *first* request after a
//! start reports `null` rather than a number. `null` renders as "measuring"; a zero would
//! render as an idle machine, and this file's whole job is to be believed.
//!
//! Two browsers polling a second apart are the case that makes this awkward, and it is a
//! real one rather than a hypothetical: whoever opens the dashboard on a phone as well as a
//! laptop. Their requests interleave, so without a floor one of them computes a percentage
//! over a twenty-millisecond window — two ticks across eight processors, which quantises
//! into figures that jump between nothing and nonsense. `MIN_WINDOW` is that floor, and a
//! request arriving inside it is answered from the last computed set rather than from a
//! window too short to mean anything.
//!
//! # Units cancel, which is why `USER_HZ` is not in this file
//!
//! A process's share of the processor is its tick difference over the whole machine's tick
//! difference. Both come from the same clock in the same unit, so the unit divides out and
//! nothing here has to know that it is 100 — which is worth having avoided, because
//! `getconf` is not in this image and the constant could only have been recalled rather
//! than checked. The same reasoning applies to memory: RSS in `/proc/<pid>/stat` is a count
//! of pages and would need a page size, so this reads `VmRSS` from `/proc/<pid>/status`
//! instead, which is already in kB and says so.
//!
//! # What this machine cannot report, and says so
//!
//! There is no core temperature. `coretemp` is not in the kernel fragment, so the only
//! thermal zones are an ACPI one — a chassis reading, not a die reading — and the wireless
//! card's. And there is no true GPU load in percent: that needs the i915 PMU through
//! `perf_event_open`, which is a syscall, which under this project's rules means a function
//! in `plexos-sys` rather than an exception here. Frequency against the maximum the part
//! will clock to is the honest substitute and is what [`Gpu`] reports. Both absences are in
//! [`Metrics::notes`] rather than left for a reader to notice, because a dashboard with a
//! blank where a number should be is indistinguishable from one that is broken.
//!
//! # What has run
//!
//! **Every path and every format in here was captured from the reference laptop on
//! 2026-08-10**, through the console's own terminal, before a line of the parsers was
//! written — `/proc/stat` with its enormous `intr` line, `/proc/<pid>/stat` for a process
//! whose name contains spaces, the eight `gt_*_freq_mhz` files, both thermal zones, and
//! Plex's cgroup. The fixtures in the tests below are those captures rather than examples
//! composed to match the code, which is the distinction that cost this project a defect in
//! `resolv.conf` parsing while its test passed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use plexos_gpu::env::Environment;

/// Processor and context-switch counters, and the process totals.
const PROC_STAT: &str = "/proc/stat";

/// Memory, in kB, with the field names spelled out.
const PROC_MEMINFO: &str = "/proc/meminfo";

/// The three load averages, and the running/total process counts.
const PROC_LOADAVG: &str = "/proc/loadavg";

/// Seconds since boot, and seconds spent idle summed over all processors.
const PROC_UPTIME: &str = "/proc/uptime";

/// Per-device I/O counters.
const PROC_DISKSTATS: &str = "/proc/diskstats";

/// Where the kernel exposes thermal zones, each with a `type` and a `temp`.
const THERMAL_ZONES: &str = "/sys/class/thermal";

/// Where network interfaces and their `statistics/` counters live.
const NET_CLASS: &str = "/sys/class/net";

/// Where DRM devices live, and with them the i915 frequency files.
const DRM_CLASS: &str = "/sys/class/drm";

/// The filesystems worth a figure on the page.
///
/// `/var` because it is the only partition anything writes to and the one that fills, and
/// `/` because it is a tmpfs assembled at boot — a root that fills is a machine that stops
/// being able to do anything, and it is small enough for that to be reachable.
const FILESYSTEMS: [&str; 2] = ["/var", "/"];

/// The shortest window a rate may be computed over.
///
/// Below this the tick counters have not moved enough to divide. See the note at the top
/// about two browsers; this is the floor that keeps their interleaved polls from producing
/// percentages that are arithmetic noise wearing a unit.
const MIN_WINDOW: Duration = Duration::from_millis(250);

/// How many processes the process list returns.
///
/// The reference laptop runs about 135, so this is not a truncation today. It is a ceiling
/// against a machine that is misbehaving in exactly the way somebody would open this list
/// to look at — a fork loop — where an unbounded list is a large response produced by a
/// machine already in trouble. [`Processes::omitted`] says how many were left out, because
/// a list silently cut short reads as a complete one.
const PROCESS_LIMIT: usize = 60;

/// What `GET /api/metrics` reports.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Metrics {
    /// Seconds since boot, from `/proc/uptime`.
    pub uptime_seconds: Option<f64>,
    /// The load averages and process counts.
    pub load: Option<Load>,
    /// Processor use over the window, in total and per core.
    pub cpu: Cpu,
    /// Memory and swap.
    pub memory: Memory,
    /// What Plex itself is using, from its own cgroup.
    pub plex: Plex,
    /// The graphics part, as far as this driver will say.
    pub gpu: Option<Gpu>,
    /// Every thermal zone the kernel exposes, named as the kernel names it.
    pub temperatures: Vec<Temperature>,
    /// Free space on the filesystems that can fill.
    pub storage: Vec<Filesystem>,
    /// Throughput per interface.
    pub network: Vec<Interface>,
    /// Throughput per disk.
    pub disks: Vec<Disk>,
    /// Context switches per second, which is the cheapest sign of a machine thrashing.
    pub context_switches_per_second: Option<f64>,
    /// How long the window these rates cover was, so the page can say so.
    pub window_ms: Option<u64>,
    /// What this machine cannot report, and what it would take to change that.
    ///
    /// Assembled here rather than left to the page for the same reason `netdiag` assembles
    /// its remedies here: a reader of the JSON deserves the same explanation as a reader of
    /// the console, and a missing number with no reason beside it looks like a fault.
    pub notes: Vec<String>,
}

/// The load averages, and how many processes exist.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Load {
    /// One-minute average.
    pub one: f64,
    /// Five-minute average.
    pub five: f64,
    /// Fifteen-minute average.
    pub fifteen: f64,
    /// Processes currently runnable.
    pub running: u64,
    /// Processes in total.
    pub total: u64,
}

/// Processor use over the window.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Cpu {
    /// How busy every processor was together, 0 to 100, or `null` on the first reading.
    pub busy_percent: Option<f64>,
    /// The same per core, in the order the kernel lists them.
    pub cores: Vec<Option<f64>>,
    /// The share of the window spent waiting on I/O rather than computing.
    ///
    /// Separate from busy because they take opposite remedies: a machine pinned at 100%
    /// busy needs less work asked of it, and one pinned in iowait needs a faster disk or a
    /// smaller working set. Rolling them together is how "the CPU is the bottleneck" gets
    /// said about a disk.
    pub iowait_percent: Option<f64>,
    /// How many processors the kernel lists.
    pub cores_online: usize,
}

/// Memory, in bytes rather than the kB `/proc/meminfo` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Memory {
    /// Total usable memory.
    pub total: u64,
    /// What the kernel estimates is available without swapping.
    ///
    /// `MemAvailable`, not `MemFree`. Free counts only untouched pages, so a healthy Linux
    /// reports almost none of it and a reader concludes the machine is out of memory when
    /// most of what is "used" is reclaimable cache.
    pub available: u64,
    /// Page cache.
    pub cached: u64,
    /// Buffers.
    pub buffers: u64,
    /// Total swap, which is zero on this appliance and worth showing as such.
    pub swap_total: u64,
    /// Free swap.
    pub swap_free: u64,
}

impl Memory {
    /// Memory in use, meaning total minus what could be handed out.
    #[must_use]
    pub const fn used(self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    /// How full it is, 0 to 100, or `None` for a reading with no total.
    #[must_use]
    pub fn percent_used(self) -> Option<f64> {
        percent(self.used(), self.total)
    }
}

/// What Plex is using, measured on Plex rather than on the machine.
///
/// This is the figure the machine exists to produce, and it is the one a person actually
/// wants: "the appliance is at 60%" does not say whether that is Plex transcoding or the
/// console polling itself. ADR-0007 already puts Plex in a cgroup with a memory ceiling, so
/// the accounting is there to be read and needed nothing new.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Plex {
    /// Whether the cgroup exists at all, which is false until Plex is provisioned.
    pub present: bool,
    /// Plex's share of one processor over the window, so 200 means two cores' worth.
    ///
    /// Not capped at 100: a transcode uses several cores and a figure that hid that would
    /// be answering a different question. The page draws it against the core count.
    pub cpu_percent: Option<f64>,
    /// Memory charged to the cgroup, including page cache it caused.
    ///
    /// **This is not "how much memory Plex is using", and reporting it as though it were
    /// made the console alarming about a machine that was perfectly well.** On the
    /// reference appliance it read 4.33 GB against a 13.1 GB ceiling after an evening of
    /// playing, of which 534 MB was Plex and 3.78 GB was the page cache of the films it had
    /// read. `memory.current` counts everything the kernel charges here, and the kernel
    /// keeps file cache until something needs the memory — so the figure climbs all evening
    /// and drops to nothing on a restart, which reads as a leak and is the opposite: it is
    /// the machine using memory it would otherwise be wasting.
    ///
    /// Kept, because "what would this cgroup have to give back if the limit were reached"
    /// is a real question and this is its answer. [`Plex::memory_anon`] is the one the page
    /// draws.
    pub memory: Option<u64>,
    /// What Plex itself holds: anonymous pages, from `memory.stat`.
    ///
    /// Heap and stack — memory with nothing on disk behind it, which is the part that
    /// cannot be handed back and therefore the part that can push a cgroup into its limit.
    /// This is the number a person means by "how much memory is Plex using".
    pub memory_anon: Option<u64>,
    /// Page cache charged to the cgroup: the media it has read.
    ///
    /// Reclaimable, almost all of it — `inactive_file` was 3.0 GB of the 3.78 on the
    /// appliance — so it is reported beside the working set rather than added into it.
    pub memory_cache: Option<u64>,
    /// The ceiling ADR-0007 set, if one is set.
    pub memory_max: Option<u64>,
    /// The most it has ever held, which outlives a restart of Plex itself.
    pub memory_peak: Option<u64>,
    /// Processes and threads in the cgroup.
    pub pids: Option<u64>,
}

/// The graphics part, as far as the driver will say.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Gpu {
    /// Which DRM card this describes.
    pub card: String,
    /// What it is clocking at now.
    pub frequency_mhz: Option<u64>,
    /// The highest it will clock to, which is what the current figure means anything
    /// against.
    pub max_frequency_mhz: Option<u64>,
    /// The lowest, which is where an idle part sits — so "at minimum" and "doing nothing"
    /// are the same reading and the page should not call the second one a load of zero.
    pub min_frequency_mhz: Option<u64>,
}

impl Gpu {
    /// Frequency as a share of the maximum, 0 to 100.
    ///
    /// Explicitly **not** a utilisation figure, and named so it cannot be mistaken for one.
    /// A part can sit at maximum frequency doing very little and at minimum while a
    /// transcode is limited by something else.
    #[must_use]
    pub fn frequency_percent(&self) -> Option<f64> {
        percent(self.frequency_mhz?, self.max_frequency_mhz?)
    }
}

/// One thermal zone.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Temperature {
    /// The kernel's own name for it — `acpitz`, `iwlwifi_1`, `x86_pkg_temp`. Passed through
    /// rather than prettified, because a zone renamed here is a zone nobody can look up.
    pub zone: String,
    /// Which `thermal_zoneN` it is.
    ///
    /// Because the type is **not unique**, which this found out by being run rather than by
    /// being reasoned about: a development host answered with two zones both called `acpitz`
    /// at different temperatures, so a table keyed on the type alone shows two identical
    /// labels and no way to tell which is which. The same question the console page has been
    /// bitten by twice — not "does this name resolve" but "does it resolve to one thing".
    pub sensor: String,
    /// Degrees Celsius. The kernel reports millidegrees.
    pub celsius: f64,
}

/// Thermal-zone types that are a processor die rather than a board or a peripheral.
///
/// Both names verified against a kernel configuration rather than recalled:
/// `CONFIG_X86_PKG_TEMP_THERMAL` is what publishes an `x86_pkg_temp` *zone*, while
/// `CONFIG_SENSORS_CORETEMP` is the hwmon driver and publishes per-core readings somewhere
/// this module does not look. The distinction matters for the remedy, which is why the note
/// names the first one.
const DIE_TEMPERATURE_ZONES: [&str; 2] = ["x86_pkg_temp", "coretemp"];

/// One filesystem.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Filesystem {
    /// Where it is mounted.
    pub path: String,
    /// Total size in bytes.
    pub total: u64,
    /// What an unprivileged process may still write.
    pub available: u64,
    /// How full, 0 to 100.
    pub percent_used: Option<f64>,
}

/// One network interface, and what it has moved.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Interface {
    /// The interface name.
    pub name: String,
    /// Whether the kernel considers it up and carrying.
    pub operstate: Option<String>,
    /// Bytes per second in, over the window.
    pub rx_per_second: Option<f64>,
    /// Bytes per second out.
    pub tx_per_second: Option<f64>,
    /// Bytes in since boot.
    pub rx_total: u64,
    /// Bytes out since boot.
    pub tx_total: u64,
}

/// One disk, and what it has moved.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Disk {
    /// The kernel's device name.
    pub name: String,
    /// Bytes per second read, over the window.
    pub read_per_second: Option<f64>,
    /// Bytes per second written.
    pub write_per_second: Option<f64>,
}

/// What `POST /api/metrics/processes` reports.
///
/// A `POST` deliberately, and it is the only reason this is a separate route. Every `GET`
/// on this console answers without a credential, which is right for "why did my machine not
/// boot" and wrong for a list of what is running with what arguments — that is closer to
/// what the terminal exposes, and the terminal is all `POST` for exactly this reason
/// (ADR-0013, ADR-0014). The gate in `http::route` sits in front of the whole table, so
/// this is authenticated by being a `POST` rather than by anything in this file.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Processes {
    /// The processes, busiest first.
    pub processes: Vec<Process>,
    /// How many existed in total.
    pub total: usize,
    /// How many are not in the list, which is never left implicit.
    pub omitted: usize,
    /// The window the percentages cover.
    pub window_ms: Option<u64>,
}

/// One process.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Process {
    /// Its pid.
    pub pid: u32,
    /// Its parent's pid.
    pub ppid: Option<u32>,
    /// The name the kernel holds, truncated by the kernel to fifteen characters.
    pub name: String,
    /// The full command, when it could be read.
    ///
    /// `/proc/<pid>/comm` is truncated — the reference laptop's Plex is `Plex Media Serv`,
    /// which is a name no reader would search for. Read only for the processes actually
    /// returned, so the cost is bounded by `PROCESS_LIMIT` rather than by the process
    /// count.
    pub command: Option<String>,
    /// Its state letter: `R`, `S`, `D`, `Z`.
    pub state: Option<String>,
    /// Its share of one processor over the window, so 200 means two cores' worth.
    pub cpu_percent: Option<f64>,
    /// Resident memory in bytes, from `VmRSS`.
    pub memory: Option<u64>,
    /// How many threads it has.
    pub threads: Option<u64>,
    /// Whether it is inside Plex's cgroup.
    ///
    /// Plex is a tree rather than a process — a server, a script host, a transcoder, an
    /// audio encoder it downloaded itself — so "which of these is Plex" is a question the
    /// process names cannot answer and the cgroup can.
    pub plex: bool,
}

/// Counters as read at one moment, with the moment.
///
/// Private because it is meaningless on its own: every figure in it is a total since boot,
/// and the public types carry differences.
#[derive(Debug, Clone)]
struct Reading {
    taken: Instant,
    cpu: Option<Ticks>,
    cores: Vec<Ticks>,
    context_switches: Option<u64>,
    plex_cpu_usec: Option<u64>,
    network: Vec<(String, u64, u64)>,
    disks: Vec<(String, u64, u64)>,
}

/// The processor tick counters from one `cpu` line of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Ticks {
    /// Everything the line accounts for, idle included.
    total: u64,
    /// Ticks spent idle.
    idle: u64,
    /// Ticks spent waiting on I/O, which the kernel counts as not-busy.
    iowait: u64,
}

/// The rates a window produced, kept so a request arriving inside `MIN_WINDOW` can be
/// answered with the last real measurement rather than with noise.
#[derive(Debug, Clone)]
struct Rates {
    cpu: Option<f64>,
    cores: Vec<Option<f64>>,
    iowait: Option<f64>,
    context_switches: Option<f64>,
    plex_cpu: Option<f64>,
    network: HashMap<String, (f64, f64)>,
    disks: HashMap<String, (f64, f64)>,
    window_ms: u64,
}

/// Per-process tick counters from one moment.
#[derive(Debug, Clone)]
struct ProcessReading {
    taken: Instant,
    /// Machine-wide ticks, which is what a process's ticks are a share *of*.
    machine: Option<Ticks>,
    ticks: HashMap<u32, u64>,
}

/// Holds the previous reading, which is the whole reason this is a type and not a function.
#[derive(Debug, Default)]
pub struct Sampler {
    previous: Mutex<Option<Reading>>,
    last_rates: Mutex<Option<Rates>>,
    previous_processes: Mutex<Option<ProcessReading>>,
}

impl Sampler {
    /// A sampler that has seen nothing yet, and will therefore report no rates once.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the machine and returns what it is doing.
    ///
    /// Never fails. A dashboard that refuses to answer because one file was unreadable is
    /// worse than one with a gap in it, so every figure is independently optional and an
    /// unreadable source becomes `null` and, where it is worth explaining, a note.
    pub fn sample(&self, env: &impl Environment) -> Metrics {
        let now = Instant::now();
        let reading = read_counters(env, now);
        let rates = self.rates(&reading);

        let cores_online = reading.cores.len();
        let memory = read_memory(env).unwrap_or(Memory {
            total: 0,
            available: 0,
            cached: 0,
            buffers: 0,
            swap_total: 0,
            swap_free: 0,
        });

        let temperatures = read_temperatures(env);
        let gpu = read_gpu(env);
        let notes = notes_about(&temperatures, gpu.as_ref(), drm_card_present(env));

        Metrics {
            uptime_seconds: read_uptime(env),
            load: read_load(env),
            cpu: Cpu {
                busy_percent: rates.as_ref().and_then(|r| r.cpu),
                cores: rates.as_ref().map_or_else(
                    || vec![None; cores_online],
                    |r| {
                        let mut cores = r.cores.clone();
                        cores.resize(cores_online, None);
                        cores
                    },
                ),
                iowait_percent: rates.as_ref().and_then(|r| r.iowait),
                cores_online,
            },
            memory,
            plex: read_plex(env, rates.as_ref().and_then(|r| r.plex_cpu)),
            gpu,
            temperatures,
            storage: read_storage(),
            network: reading
                .network
                .iter()
                .map(|(name, rx, tx)| {
                    let rate = rates.as_ref().and_then(|r| r.network.get(name).copied());
                    Interface {
                        name: name.clone(),
                        operstate: env
                            .read(&Path::new(NET_CLASS).join(name).join("operstate"))
                            .ok()
                            .map(|s| s.trim().to_owned()),
                        rx_per_second: rate.map(|(rx, _)| rx),
                        tx_per_second: rate.map(|(_, tx)| tx),
                        rx_total: *rx,
                        tx_total: *tx,
                    }
                })
                .collect(),
            disks: reading
                .disks
                .iter()
                .map(|(name, _, _)| {
                    let rate = rates.as_ref().and_then(|r| r.disks.get(name).copied());
                    Disk {
                        name: name.clone(),
                        read_per_second: rate.map(|(read, _)| read),
                        write_per_second: rate.map(|(_, write)| write),
                    }
                })
                .collect(),
            context_switches_per_second: rates.as_ref().and_then(|r| r.context_switches),
            window_ms: rates.as_ref().map(|r| r.window_ms),
            notes,
        }
    }

    /// Turns this reading and the previous one into rates, or reuses the last set.
    ///
    /// Returns `None` only when nothing has ever been measured, which happens once per
    /// start of the daemon.
    fn rates(&self, reading: &Reading) -> Option<Rates> {
        let mut previous = self.previous.lock().ok()?;
        let mut last = self.last_rates.lock().ok()?;

        let Some(before) = previous.as_ref() else {
            *previous = Some(reading.clone());
            return None;
        };

        let elapsed = reading.taken.saturating_duration_since(before.taken);
        if elapsed < MIN_WINDOW {
            // Deliberately leaves `previous` alone. Replacing the baseline with a reading
            // this close to it would make the *next* window short as well, so one busy
            // client would keep every client's figures noisy.
            return last.clone();
        }

        let seconds = elapsed.as_secs_f64();
        let rates = Rates {
            cpu: busy_percent(before.cpu, reading.cpu),
            cores: reading
                .cores
                .iter()
                .enumerate()
                .map(|(index, now)| busy_percent(before.cores.get(index).copied(), Some(*now)))
                .collect(),
            iowait: iowait_percent(before.cpu, reading.cpu),
            context_switches: per_second(
                before.context_switches,
                reading.context_switches,
                seconds,
            ),
            plex_cpu: match (before.plex_cpu_usec, reading.plex_cpu_usec) {
                (Some(was), Some(now)) if now >= was => {
                    // Microseconds of processor time over microseconds of wall clock, so
                    // 100 is one core saturated and 800 is this laptop's eight.
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "a u64 of microseconds exceeds f64's exact range only after \
                                  285 years of accumulated processor time"
                    )]
                    Some((now - was) as f64 / (seconds * 1_000_000.0) * 100.0)
                }
                _ => None,
            },
            network: paired_rates(&before.network, &reading.network, seconds),
            disks: paired_rates(&before.disks, &reading.disks, seconds),
            window_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        };

        *previous = Some(reading.clone());
        *last = Some(rates.clone());
        Some(rates)
    }

    /// Lists what is running, busiest first.
    ///
    /// The percentages need a previous reading in the same way the machine-wide ones do, so
    /// the first call after a start reports `null` for every process rather than zero.
    pub fn processes(&self, env: &impl Environment) -> Processes {
        let taken = Instant::now();
        let plex_pids = plex_members(env);
        let stat = env
            .read(Path::new(PROC_STAT))
            .ok()
            .map(|text| parse_stat(&text));
        let machine = stat.as_ref().and_then(|s| s.all);
        let core_count = stat.as_ref().map(|s| s.cores.len()).filter(|n| *n > 0);

        let mut ticks = HashMap::new();
        let mut found = Vec::new();

        for pid in pids(env) {
            let Ok(stat) = env.read(&PathBuf::from("/proc").join(pid.to_string()).join("stat"))
            else {
                // A process that exited between being listed and being read. Utterly
                // normal, and the reason this loop cannot treat a read failure as an error.
                continue;
            };
            let Some(parsed) = parse_process_stat(&stat) else {
                continue;
            };
            ticks.insert(pid, parsed.ticks);
            found.push((pid, parsed));
        }

        let previous = self
            .previous_processes
            .lock()
            .ok()
            .and_then(|mut slot| {
                let before = slot.clone();
                *slot = Some(ProcessReading {
                    taken,
                    machine,
                    ticks: ticks.clone(),
                });
                before
            })
            .filter(|before| taken.saturating_duration_since(before.taken) >= MIN_WINDOW);

        // The denominator is the whole machine's tick difference, not elapsed time: both
        // numerator and denominator are then in USER_HZ and the unit cancels. Multiplying
        // by the core count turns "share of the machine" into "share of one core", which is
        // the figure every other tool shows and the one people compare against.
        let machine_delta = previous
            .as_ref()
            .and_then(|before| match (before.machine, machine) {
                (Some(was), Some(now)) if now.total >= was.total => Some(now.total - was.total),
                _ => None,
            })
            .filter(|delta| *delta > 0);

        let total = found.len();
        let mut processes: Vec<Process> = found
            .into_iter()
            .map(|(pid, parsed)| {
                let cpu_percent = match (
                    previous.as_ref().and_then(|b| b.ticks.get(&pid).copied()),
                    machine_delta,
                    core_count,
                ) {
                    (Some(was), Some(delta), Some(cores)) if parsed.ticks >= was =>
                    {
                        #[expect(
                            clippy::cast_precision_loss,
                            reason = "tick counts on any machine that has ever booted are far \
                                      inside f64's exact range"
                        )]
                        Some((parsed.ticks - was) as f64 / delta as f64 * cores as f64 * 100.0)
                    }
                    _ => None,
                };

                Process {
                    pid,
                    ppid: parsed.ppid,
                    name: parsed.name,
                    command: None,
                    state: parsed.state,
                    cpu_percent,
                    memory: read_vmrss(env, pid),
                    threads: parsed.threads,
                    plex: plex_pids.contains(&pid),
                }
            })
            .collect();

        // Busiest first, and by memory when nothing has a percentage yet — otherwise the
        // very first list a page shows is in pid order, which looks like a list that failed
        // to sort rather than one with nothing to sort by.
        processes.sort_by(|a, b| {
            b.cpu_percent
                .unwrap_or(-1.0)
                .total_cmp(&a.cpu_percent.unwrap_or(-1.0))
                .then_with(|| b.memory.unwrap_or(0).cmp(&a.memory.unwrap_or(0)))
        });

        let omitted = processes.len().saturating_sub(PROCESS_LIMIT);
        processes.truncate(PROCESS_LIMIT);

        for process in &mut processes {
            process.command = read_cmdline(env, process.pid);
        }

        Processes {
            processes,
            total,
            omitted,
            window_ms: previous.as_ref().map(|before| {
                u64::try_from(taken.saturating_duration_since(before.taken).as_millis())
                    .unwrap_or(u64::MAX)
            }),
        }
    }
}

/// What this machine cannot report, and what it would take to change that.
///
/// Derived from what was actually found rather than written once. The first version said "this
/// kernel has no coretemp driver", which was true of the appliance and false of the first
/// development host it ran on: that one has an `x86_pkg_temp` zone reading 46 °C, so the note
/// denied a number printed three lines above it. Same shape as the firmware list written for
/// one machine, and the `Unknown`-means-debugfs-is-unmounted guess, both already in the trap
/// list. A statement about the machine has to be computed from the machine.
/// `card_present` is the fact [`read_gpu`] used to throw away. It returns `None` both when
/// there is no DRM card on the machine and when there is one whose driver publishes none of
/// the i915 frequency files — two states with opposite meanings and opposite remedies,
/// collapsed into one value, which is the shape this repository already records twice.
fn notes_about(temperatures: &[Temperature], gpu: Option<&Gpu>, card_present: bool) -> Vec<String> {
    let mut notes = Vec::new();

    if !temperatures
        .iter()
        .any(|t| DIE_TEMPERATURE_ZONES.contains(&t.zone.as_str()))
    {
        let found = if temperatures.is_empty() {
            "no thermal zone at all".to_owned()
        } else {
            let names: Vec<&str> = temperatures.iter().map(|t| t.zone.as_str()).collect();
            format!("only these zones: {}", names.join(", "))
        };
        notes.push(format!(
            "No processor die temperature: this kernel publishes {found}, none of which is \
             the processor itself -- acpitz is a chassis sensor. Remedy: set \
             CONFIG_X86_PKG_TEMP_THERMAL=y in the kernel fragment. It has to be `y` rather \
             than `m`: this image does build modules now, for NVIDIA's out-of-tree ones, \
             but nothing in it loads a module it was not written to load -- there is no \
             udev and no modprobe -- so `m` here is the same as absent, and that is exactly \
             how this zone went missing once already."
        ));
    }

    notes.push(match (gpu, card_present) {
        (Some(_), _) => "GPU frequency is not GPU load. A true utilisation figure needs the \
             i915 PMU through perf_event_open, which is a syscall and so belongs in \
             plexos-sys; frequency against the part's maximum is what can be read from \
             sysfs today."
            .to_owned(),
        // A card is here and these figures are not. Which driver is bound decides that, and
        // this does not guess which: the old wording named two possible causes -- no card,
        // or `xe` -- and on the first machine anybody read it on, an RTX desktop, neither
        // was true. A card was present and its driver was `nvidia`. A note that enumerates
        // causes has to enumerate the one that is happening, and the honest statement is
        // about what is read rather than about what is bound.
        (None, true) => "No GPU frequency: these figures come from the i915 frequency files \
             in sysfs, and the driver bound to the card here does not publish them -- which \
             is the case for nvidia and amdgpu, and for xe, which puts them under the tile. \
             Remedy: /api/gpu reports which driver is bound and what it can do."
            .to_owned(),
        (None, false) => "No GPU frequency: this machine has no DRM card at all, so there \
             is no graphics device to report a frequency for. Remedy: /api/gpu tells apart \
             a machine with no card from one whose card has no driver bound."
            .to_owned(),
    });

    notes
}

/// Reads every counter the rates are computed from, at one moment.
fn read_counters(env: &impl Environment, taken: Instant) -> Reading {
    let stat = env.read(Path::new(PROC_STAT)).ok();
    let parsed = stat.as_deref().map(parse_stat);

    Reading {
        taken,
        cpu: parsed.as_ref().and_then(|s| s.all),
        cores: parsed.as_ref().map(|s| s.cores.clone()).unwrap_or_default(),
        context_switches: parsed.as_ref().and_then(|s| s.context_switches),
        plex_cpu_usec: env
            .read(&plex_cgroup().join("cpu.stat"))
            .ok()
            .and_then(|text| field(&text, "usage_usec")),
        network: read_interface_counters(env),
        disks: read_disk_counters(env),
    }
}

/// What one `/proc/stat` says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Stat {
    /// The aggregate `cpu` line.
    all: Option<Ticks>,
    /// The `cpu0`, `cpu1`, … lines, in order.
    cores: Vec<Ticks>,
    /// The `ctxt` counter.
    context_switches: Option<u64>,
}

/// Reads `/proc/stat`.
///
/// Tolerant on purpose. The real file's `intr` line on the reference laptop is over a
/// thousand fields long, and a parser that tried to understand every line would spend its
/// time on the one line nothing here wants.
fn parse_stat(text: &str) -> Stat {
    let mut stat = Stat::default();

    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };

        if key == "ctxt" {
            stat.context_switches = parts.next().and_then(|v| v.parse().ok());
            continue;
        }

        if !key.starts_with("cpu") {
            continue;
        }

        let values: Vec<u64> = parts.map(|v| v.parse().unwrap_or(0)).collect();
        // user nice system idle iowait irq softirq steal guest guest_nice. Summing whatever
        // is present rather than the first eight: the list has grown twice in Linux's
        // history, and a total that ignores a new column would drift as the kernel changes
        // under the same defconfig.
        let ticks = Ticks {
            total: values.iter().sum(),
            idle: values.get(3).copied().unwrap_or(0),
            iowait: values.get(4).copied().unwrap_or(0),
        };

        if key == "cpu" {
            stat.all = Some(ticks);
        } else {
            stat.cores.push(ticks);
        }
    }

    stat
}

/// Busy share of a window, 0 to 100.
///
/// Busy is everything that is neither idle nor iowait. Counting iowait as busy is the
/// commonest way a dashboard reports a saturated disk as a saturated processor.
fn busy_percent(before: Option<Ticks>, now: Option<Ticks>) -> Option<f64> {
    let (before, now) = (before?, now?);
    let total = now.total.checked_sub(before.total)?;
    let quiet = (now.idle + now.iowait).checked_sub(before.idle + before.iowait)?;
    let busy = total.checked_sub(quiet)?;
    percent(busy, total)
}

/// The share of a window spent waiting on I/O.
fn iowait_percent(before: Option<Ticks>, now: Option<Ticks>) -> Option<f64> {
    let (before, now) = (before?, now?);
    let total = now.total.checked_sub(before.total)?;
    let waiting = now.iowait.checked_sub(before.iowait)?;
    percent(waiting, total)
}

/// A counter's change per second, or `None` if it went backwards.
///
/// Backwards means the counter was reset — an interface that went away and came back, a
/// disk that was unplugged. Reporting the wrap as a rate produces a spike of some
/// exabytes per second, which is the shape of number that gets believed because it is on a
/// dashboard.
fn per_second(before: Option<u64>, now: Option<u64>, seconds: f64) -> Option<f64> {
    let (before, now) = (before?, now?);
    if seconds <= 0.0 {
        return None;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte and tick counters are exact in f64 to 9 petabytes"
    )]
    now.checked_sub(before).map(|delta| delta as f64 / seconds)
}

/// Matches two lists of named counters by name and returns their rates.
///
/// By name rather than by position, because an interface appearing or a disk going away
/// shifts every later entry — and the USB Ethernet adapter on this hardware enumerates
/// seconds after everything else, so a list that changes shape is the normal case here
/// rather than an edge one.
fn paired_rates(
    before: &[(String, u64, u64)],
    now: &[(String, u64, u64)],
    seconds: f64,
) -> HashMap<String, (f64, f64)> {
    let was: HashMap<&str, (u64, u64)> = before
        .iter()
        .map(|(name, a, b)| (name.as_str(), (*a, *b)))
        .collect();

    now.iter()
        .filter_map(|(name, a, b)| {
            let (was_a, was_b) = was.get(name.as_str()).copied()?;
            Some((
                name.clone(),
                (
                    per_second(Some(was_a), Some(*a), seconds)?,
                    per_second(Some(was_b), Some(*b), seconds)?,
                ),
            ))
        })
        .collect()
}

/// One number from a file of `key value` lines, as cgroup and meminfo files both are.
fn field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()?.trim_end_matches(':') == key).then(|| parts.next()?.parse().ok())?
    })
}

/// Reads `/proc/meminfo`, which is in kB.
fn read_memory(env: &impl Environment) -> Option<Memory> {
    let text = env.read(Path::new(PROC_MEMINFO)).ok()?;
    let kb = |key: &str| field(&text, key).unwrap_or(0).saturating_mul(1024);

    Some(Memory {
        total: kb("MemTotal"),
        available: kb("MemAvailable"),
        cached: kb("Cached"),
        buffers: kb("Buffers"),
        swap_total: kb("SwapTotal"),
        swap_free: kb("SwapFree"),
    })
}

/// Reads `/proc/loadavg`, whose fourth field is `running/total`.
fn read_load(env: &impl Environment) -> Option<Load> {
    let text = env.read(Path::new(PROC_LOADAVG)).ok()?;
    parse_loadavg(&text)
}

/// Parses `0.01 0.02 0.11 1/183 2213`.
fn parse_loadavg(text: &str) -> Option<Load> {
    let mut parts = text.split_whitespace();
    let one = parts.next()?.parse().ok()?;
    let five = parts.next()?.parse().ok()?;
    let fifteen = parts.next()?.parse().ok()?;
    let (running, total) = parts.next()?.split_once('/')?;

    Some(Load {
        one,
        five,
        fifteen,
        running: running.parse().ok()?,
        total: total.parse().ok()?,
    })
}

/// Reads seconds since boot from `/proc/uptime`.
fn read_uptime(env: &impl Environment) -> Option<f64> {
    env.read(Path::new(PROC_UPTIME))
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Where Plex's cgroup is, from the crate that creates it rather than from a literal here.
fn plex_cgroup() -> PathBuf {
    Path::new(plexos_plex::cgroup::CGROUP_ROOT).join(plexos_plex::cgroup::PLEX_CGROUP)
}

/// Reads what Plex is using, given its already-computed processor share.
fn read_plex(env: &impl Environment, cpu_percent: Option<f64>) -> Plex {
    let group = plex_cgroup();
    let number = |file: &str| {
        env.read(&group.join(file))
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
    };

    // The breakdown, which is what separates "Plex is using four gigabytes" from "the kernel
    // is holding four gigabytes of films it will hand back the moment anything wants them".
    let stat = env.read(&group.join("memory.stat")).unwrap_or_default();
    let field = |name: &str| {
        stat.lines()
            .find_map(|line| line.strip_prefix(name)?.trim().parse::<u64>().ok())
    };

    Plex {
        // `cpu.stat` rather than the directory: a cgroup directory that exists with nothing
        // in it is what a stopped Plex leaves behind, and reporting that as present would
        // make the card claim a running server.
        present: env.read(&group.join("cpu.stat")).is_ok(),
        cpu_percent,
        memory: number("memory.current"),
        // `anon ` and `file ` with the space, or `anon_thp` and `file_mapped` — both real
        // keys in this file — would match first and answer a different question.
        memory_anon: field("anon "),
        memory_cache: field("file "),
        // `max` is the literal in an unset ceiling, and parsing it as a number fails —
        // which is the correct answer to "what is the limit" but had to be deliberate.
        memory_max: number("memory.max"),
        memory_peak: number("memory.peak"),
        pids: number("pids.current"),
    }
}

/// The pids inside Plex's cgroup.
fn plex_members(env: &impl Environment) -> Vec<u32> {
    env.read(&plex_cgroup().join("cgroup.procs"))
        .ok()
        .map(|text| {
            text.lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Reads every thermal zone, named as the kernel names it.
fn read_temperatures(env: &impl Environment) -> Vec<Temperature> {
    let Ok(entries) = env.list_dir(Path::new(THERMAL_ZONES)) else {
        return Vec::new();
    };

    let mut zones: Vec<Temperature> = entries
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("thermal_zone"))
        })
        .filter_map(|path| {
            let zone = env.read(&path.join("type")).ok()?.trim().to_owned();
            let milli: i64 = env.read(&path.join("temp")).ok()?.trim().parse().ok()?;
            #[expect(
                clippy::cast_precision_loss,
                reason = "a millidegree reading is five digits"
            )]
            Some(Temperature {
                zone,
                sensor: path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_owned(),
                celsius: milli as f64 / 1000.0,
            })
        })
        .collect();

    // By zone then sensor, so the two `acpitz` entries a real machine produced sit together
    // and in a stable order rather than in whatever order the directory was listed.
    zones.sort_by(|a, b| a.zone.cmp(&b.zone).then_with(|| a.sensor.cmp(&b.sensor)));
    zones
}

/// Reads the graphics part's frequencies.
///
/// The file names here are i915's, captured from the reference laptop. `xe` puts the same
/// numbers under the tile with different names, and there has never been an `xe` part on
/// this desk — so rather than guess at a path, this returns `None` there and `sample` says
/// which driver to ask about. Reporting nothing is recoverable; reporting a wrong number
/// on a dashboard is not.
/// Whether this machine has a DRM card at all, which is a different question from whether
/// [`read_gpu`] found anything to read.
///
/// It exists because the two were the same `None`, and the note built on that `None` then
/// guessed at a cause and named the wrong one. `plexos_gpu::display_devices` makes the same
/// distinction for the same reason, one level up: nothing there, something there with no
/// driver, and a driver bound that produced no node are three states, not one.
fn drm_card_present(env: &impl Environment) -> bool {
    env.list_dir(Path::new(DRM_CLASS)).is_ok_and(|cards| {
        cards.iter().any(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("card") && !n.contains('-'))
        })
    })
}

fn read_gpu(env: &impl Environment) -> Option<Gpu> {
    let cards = env.list_dir(Path::new(DRM_CLASS)).ok()?;
    let card = cards.into_iter().find(|path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("card") && !n.contains('-'))
    })?;

    let mhz = |file: &str| {
        env.read(&card.join(file))
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
    };

    let frequency = mhz("gt_act_freq_mhz");
    // RP0 is the hardware's own ceiling; gt_max_freq_mhz is the policy currently in force
    // and can be below it. The ceiling is what a percentage should be against, because a
    // lowered policy is not a slower chip.
    let maximum = mhz("gt_RP0_freq_mhz").or_else(|| mhz("gt_max_freq_mhz"));

    if frequency.is_none() && maximum.is_none() {
        return None;
    }

    Some(Gpu {
        card: card
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("card")
            .to_owned(),
        frequency_mhz: frequency,
        max_frequency_mhz: maximum,
        min_frequency_mhz: mhz("gt_RPn_freq_mhz").or_else(|| mhz("gt_min_freq_mhz")),
    })
}

/// Reads free space, through the one syscall in this file's subject matter.
fn read_storage() -> Vec<Filesystem> {
    FILESYSTEMS
        .iter()
        .filter_map(|path| {
            let space = plexos_sys::fs::space(Path::new(path)).ok()?;
            Some(Filesystem {
                path: (*path).to_owned(),
                total: space.total,
                available: space.available,
                percent_used: space.percent_used(),
            })
        })
        .collect()
}

/// Reads the byte counters of every interface that is a piece of hardware.
///
/// Hardware means a `device` symlink in sysfs. Interface type cannot be the test: bridges
/// and `veth` pairs are `ARPHRD_ETHER` and report a carrier, and they sort before the real
/// interface by name — so a dashboard that trusted the type would draw `docker0`'s
/// throughput and call it the network. There are no bridges on this appliance today, which
/// is exactly why the discriminator is worth applying before something adds one.
fn read_interface_counters(env: &impl Environment) -> Vec<(String, u64, u64)> {
    let Ok(entries) = env.list_dir(Path::new(NET_CLASS)) else {
        return Vec::new();
    };

    let mut interfaces: Vec<(String, u64, u64)> = entries
        .into_iter()
        .filter(|path| env.read_link(&path.join("device")).is_ok())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            let counter = |file: &str| {
                env.read(&path.join("statistics").join(file))
                    .ok()
                    .and_then(|text| text.trim().parse::<u64>().ok())
            };
            Some((name, counter("rx_bytes")?, counter("tx_bytes")?))
        })
        .collect();

    interfaces.sort_by(|a, b| a.0.cmp(&b.0));
    interfaces
}

/// How many bytes one `/proc/diskstats` sector is.
///
/// Fixed at 512 in the kernel's own accounting regardless of what the device reports as its
/// sector size — `part_stat_read` counts in 512-byte units — so this is not the disk's
/// geometry and must not be read from it.
const DISKSTATS_SECTOR: u64 = 512;

/// Reads the I/O counters of every whole disk.
fn read_disk_counters(env: &impl Environment) -> Vec<(String, u64, u64)> {
    let Ok(text) = env.read(Path::new(PROC_DISKSTATS)) else {
        return Vec::new();
    };
    parse_diskstats(&text)
}

/// Parses `/proc/diskstats`, keeping whole disks and dropping their partitions.
///
/// A partition's counters are already inside its disk's, so listing both double-counts and
/// fills the card with `nvme0n1p1` through `p6` for one device. Whole disks are the ones
/// with no partition suffix, which for both naming schemes on this hardware — `sdX` and
/// `nvme0n1pN` — means a name that does not end in a digit unless it is an `nvme` namespace.
fn parse_diskstats(text: &str) -> Vec<(String, u64, u64)> {
    let mut disks: Vec<(String, u64, u64)> = text
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // major minor name reads merges sectors-read ms-reading writes merges
            // sectors-written …
            let name = (*fields.get(2)?).to_owned();
            if !is_whole_disk(&name) {
                return None;
            }
            let read = fields.get(5)?.parse::<u64>().ok()? * DISKSTATS_SECTOR;
            let written = fields.get(9)?.parse::<u64>().ok()? * DISKSTATS_SECTOR;
            Some((name, read, written))
        })
        .collect();

    disks.sort_by(|a, b| a.0.cmp(&b.0));
    disks
}

/// Whether a `/proc/diskstats` name is a whole disk rather than a partition.
fn is_whole_disk(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("nvme") {
        // `nvme0n1` is a whole disk and `nvme0n1p6` is a partition on it, so the `p` is the
        // discriminator here rather than the trailing digit.
        return !rest.contains('p');
    }
    // `loop0` and `ram0` end in a digit and are whole devices, but neither is a disk
    // anybody wants on this card — and every app image Plex runs from is a loop device, so
    // without this the list grows by one per installed version.
    if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("dm-") {
        return false;
    }
    !name.chars().last().is_some_and(char::is_numeric)
}

/// The numeric entries of `/proc`, which are the processes.
fn pids(env: &impl Environment) -> Vec<u32> {
    env.list_dir(Path::new("/proc"))
        .map(|entries| {
            entries
                .into_iter()
                .filter_map(|path| path.file_name()?.to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// What one `/proc/<pid>/stat` says.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessStat {
    name: String,
    state: Option<String>,
    ppid: Option<u32>,
    ticks: u64,
    threads: Option<u64>,
}

/// Parses one `/proc/<pid>/stat`.
///
/// The second field is the command in parentheses, and it may contain both spaces and
/// parentheses — the reference laptop's Plex is literally `(Plex Media Serv)`, and a
/// transcoder is worse. So the line is split at the **last** `)` and the fields counted
/// from there. Splitting on whitespace, which is what this obviously wants to be, silently
/// shifts every field for exactly the processes somebody opened this list to look at.
fn parse_process_stat(text: &str) -> Option<ProcessStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text.get(open + 1..close)?.to_owned();

    // After the name, fields resume at `state`, which is field 3 of the file — so the
    // file's field N is at index N-3 here.
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    let at = |field: usize| rest.get(field - 3).copied();

    let utime: u64 = at(14)?.parse().ok()?;
    let stime: u64 = at(15)?.parse().ok()?;

    Some(ProcessStat {
        name,
        state: at(3).map(str::to_owned),
        ppid: at(4).and_then(|v| v.parse().ok()),
        ticks: utime.saturating_add(stime),
        threads: at(20).and_then(|v| v.parse().ok()),
    })
}

/// Resident memory in bytes, from `VmRSS` in `/proc/<pid>/status`.
///
/// From `status` rather than from `stat`'s field 24, which is a count of pages and would
/// need a page size this image has no `getconf` to ask for. `VmRSS` is in kB and labelled,
/// so nothing has to be recalled. A kernel thread has no `VmRSS` line at all, which is why
/// this is optional rather than zero.
fn read_vmrss(env: &impl Environment, pid: u32) -> Option<u64> {
    let text = env
        .read(&PathBuf::from("/proc").join(pid.to_string()).join("status"))
        .ok()?;
    field(&text, "VmRSS").map(|kb| kb.saturating_mul(1024))
}

/// The full command line, with the NULs it is separated by turned into spaces.
///
/// Empty for a kernel thread, which is how this tells one from a process whose command
/// could not be read.
fn read_cmdline(env: &impl Environment, pid: u32) -> Option<String> {
    let text = env
        .read(&PathBuf::from("/proc").join(pid.to_string()).join("cmdline"))
        .ok()?;
    let joined = text
        .split('\0')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// A ratio as a percentage, clamped, or `None` when the denominator is zero.
///
/// Clamped because a counter that moved oddly should show a boundary rather than a figure
/// outside the range a reader assumes — `/proc` guarantees monotonic counters and a
/// suspended machine has produced 101% here in other projects.
fn percent(part: u64, whole: u64) -> Option<f64> {
    if whole == 0 {
        return None;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "exact to the unit up to 9 petabytes, well past any counter here"
    )]
    Some((part as f64 / whole as f64 * 100.0).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/proc/stat` from the reference laptop, 2026-08-10, with the `intr` line's real
    /// length cut down but its shape kept — over a thousand fields is what a parser has to
    /// step over, and the point of keeping it is that it must not become a `cpu` line.
    const STAT: &str = "\
cpu  23840 90 2683 1652593 27606 0 888 0 0 0
cpu0 1761 10 329 207190 4169 0 4 0 0 0
cpu1 4297 18 395 205264 3501 0 6 0 0 0
cpu2 3701 21 330 205175 4099 0 46 0 0 0
cpu3 2582 22 325 206702 3840 0 5 0 0 0
cpu4 914 2 429 210015 928 0 813 0 0 0
cpu5 6463 7 275 203184 3655 0 3 0 0 0
cpu6 1586 2 314 208597 3073 0 4 0 0 0
cpu7 2533 3 282 206463 4337 0 4 0 0 0
intr 1692326 0 6 0 0 0 0 0 0 0 22 0 0 0 0 0 0 0 0 0 0 0
ctxt 2384866
btime 1786401647
processes 2300
procs_running 1
procs_blocked 0
softirq 1050296 2 375695 46 60594 0 0 1042 313174 0 299743
";

    /// `/proc/meminfo` from the same capture.
    const MEMINFO: &str = "\
MemTotal:        7887264 kB
MemFree:         7028816 kB
MemAvailable:    7585632 kB
Buffers:            2240 kB
Cached:           689576 kB
SwapCached:            0 kB
Active:           369476 kB
Inactive:         385224 kB
SwapTotal:             0 kB
SwapFree:              0 kB
";

    /// Plex Media Server's own `/proc/182/stat`, captured from the appliance. Kept whole,
    /// because the name with spaces in it is the entire reason this fixture exists.
    const PLEX_STAT: &str = "182 (Plex Media Serv) S 135 0 0 0 -1 4194560 265102 880383 663 \
226 16430 475 6696 1061 20 0 20 0 4435 360222720 21052 18446744073709551615 140197259002496 \
140197274005504 140726255465136 0 0 0 81926 4097 66808 0 0 0 17 2 0 0 0 0 0 140197274757728 \
140197274774440 93825004589056 140726255472157 140726255472216 140726255472216 \
140726255472573 0";

    /// `/proc/diskstats` from the appliance, which has one physical disk.
    const DISKSTATS: &str = "\
 259       0 nvme0n1 4159 311 401495 713 6174 280 1313985 166532 0 1737 167673 0 0 0 0 89 427
 259       1 nvme0n1p1 120 0 5344 12 2 0 8 1 0 20 13 0 0 0 0 0 0
 259       6 nvme0n1p6 2100 200 200000 300 4000 100 900000 90000 0 900 90300 0 0 0 0 0 0
   7       0 loop0 10 0 800 2 0 0 0 0 0 4 2 0 0 0 0 0 0
";

    #[test]
    fn the_captured_stat_reads_as_eight_cores_and_a_total() {
        let stat = parse_stat(STAT);

        assert_eq!(stat.cores.len(), 8, "the reference laptop has eight");
        assert_eq!(stat.context_switches, Some(2_384_866));

        let all = stat.all.expect("an aggregate cpu line");
        assert_eq!(all.idle, 1_652_593, "field four is idle");
        assert_eq!(all.iowait, 27_606, "field five is iowait");
        // user + nice + system + idle + iowait + irq + softirq, with irq zero in this
        // capture. Summed rather than taken from a fixed count of columns, so a kernel that
        // grows the line — Linux has done it twice — is still fully accounted for.
        assert_eq!(
            all.total,
            23840 + 90 + 2683 + 1_652_593 + 27606 + 888,
            "the total sums every column present"
        );
    }

    #[test]
    fn the_enormous_intr_line_is_not_mistaken_for_a_processor() {
        // It begins with neither `cpu` nor `ctxt`, and it is over a thousand fields long on
        // the real machine. A parser that indexed positionally through the file rather than
        // keying off the first word would read it as something.
        let stat = parse_stat(STAT);
        assert_eq!(stat.cores.len(), 8, "not nine, and not one per interrupt");
    }

    #[test]
    fn a_first_reading_has_no_percentage_rather_than_a_zero() {
        // The distinction this whole module is built around: an idle machine and a machine
        // that has not been measured yet must not render the same.
        let ticks = parse_stat(STAT).all;
        assert_eq!(busy_percent(None, ticks), None);
    }

    #[test]
    fn a_busy_window_reads_as_busy_and_iowait_is_not_counted_as_work() {
        let before = Ticks {
            total: 1000,
            idle: 800,
            iowait: 100,
        };
        // 100 more ticks, all of them spent waiting on a disk.
        let now = Ticks {
            total: 1100,
            idle: 800,
            iowait: 200,
        };

        assert_eq!(
            busy_percent(Some(before), Some(now)),
            Some(0.0),
            "a window that was entirely iowait was not busy: reporting it as 100% busy is \
             how a slow disk gets diagnosed as a slow processor"
        );
        assert_eq!(iowait_percent(Some(before), Some(now)), Some(100.0));
    }

    #[test]
    fn a_counter_that_went_backwards_is_unknown_rather_than_enormous() {
        // An interface that went away and came back, or a disk unplugged. Wrapping a u64
        // subtraction here yields about 18 exabytes per second, which is a figure that gets
        // believed precisely because it is on a dashboard.
        assert_eq!(per_second(Some(500), Some(100), 1.0), None);

        let before = Ticks {
            total: 1000,
            idle: 900,
            iowait: 0,
        };
        let after_reset = Ticks {
            total: 10,
            idle: 5,
            iowait: 0,
        };
        assert_eq!(busy_percent(Some(before), Some(after_reset)), None);
    }

    #[test]
    fn a_zero_length_window_produces_nothing_rather_than_a_division() {
        assert_eq!(per_second(Some(0), Some(100), 0.0), None);
        assert_eq!(percent(1, 0), None);
    }

    #[test]
    fn meminfo_reports_available_rather_than_free() {
        let memory = parse_meminfo_for_test(MEMINFO);

        assert_eq!(memory.total, 7_887_264 * 1024);
        assert_eq!(
            memory.available,
            7_585_632 * 1024,
            "MemAvailable, not MemFree: on a healthy Linux MemFree is nearly nothing and a \
             reader would conclude the machine is out of memory"
        );
        assert_eq!(memory.swap_total, 0, "this appliance has no swap");
        assert!(
            memory.percent_used().is_some_and(|p| p < 10.0),
            "the captured machine had almost nothing in use: {:?}",
            memory.percent_used()
        );
    }

    /// The parsing half of [`read_memory`], without the file read.
    fn parse_meminfo_for_test(text: &str) -> Memory {
        let kb = |key: &str| field(text, key).unwrap_or(0).saturating_mul(1024);
        Memory {
            total: kb("MemTotal"),
            available: kb("MemAvailable"),
            cached: kb("Cached"),
            buffers: kb("Buffers"),
            swap_total: kb("SwapTotal"),
            swap_free: kb("SwapFree"),
        }
    }

    #[test]
    fn a_meminfo_key_is_matched_whole_and_not_by_prefix() {
        // `SwapCached` starts with `Swap`, `MemFree` and `MemAvailable` both start with
        // `Mem`, and `Cached` is a suffix of `SwapCached`. A prefix match anywhere here
        // reads one field as another and every figure stays plausible.
        assert_eq!(field(MEMINFO, "Cached"), Some(689_576));
        assert_eq!(field(MEMINFO, "SwapCached"), Some(0));
        assert_eq!(field(MEMINFO, "MemFree"), Some(7_028_816));
        assert_eq!(field(MEMINFO, "Mem"), None, "no such key, so no answer");
    }

    #[test]
    fn the_captured_loadavg_parses_including_the_process_counts() {
        let load = parse_loadavg("0.01 0.02 0.11 1/183 2213\n").expect("the captured line");

        assert!((load.one - 0.01).abs() < f64::EPSILON);
        assert!((load.fifteen - 0.11).abs() < f64::EPSILON);
        assert_eq!(load.running, 1, "the fourth field is running/total");
        assert_eq!(load.total, 183);
    }

    #[test]
    fn a_process_whose_name_contains_spaces_parses_and_this_is_not_hypothetical() {
        // `Plex Media Serv` — the kernel truncates comm to fifteen characters and does not
        // escape it. Splitting this line on whitespace shifts every field by two, so utime
        // becomes a page-fault count and the busiest process on the machine reports
        // nonsense.
        let plex = parse_process_stat(PLEX_STAT).expect("the captured line");

        assert_eq!(plex.name, "Plex Media Serv");
        assert_eq!(plex.state.as_deref(), Some("S"));
        assert_eq!(plex.ppid, Some(135));
        assert_eq!(
            plex.ticks,
            16430 + 475,
            "field 14 utime plus field 15 stime, counted from the closing parenthesis"
        );
        assert_eq!(plex.threads, Some(20), "field 20");
    }

    #[test]
    fn a_name_containing_a_parenthesis_still_parses() {
        // Splitting at the *first* `)` rather than the last is the other half of the same
        // trap, and a process can be named this way on purpose.
        let line = "42 (we (i)rd) R 1 0 0 0 -1 0 0 0 0 0 7 3 0 0 20 0 4 0 99 100 5";
        let parsed = parse_process_stat(line).expect("a name with parentheses in it");

        assert_eq!(parsed.name, "we (i)rd");
        assert_eq!(parsed.ticks, 10, "7 + 3");
    }

    #[test]
    fn diskstats_keeps_whole_disks_and_drops_their_partitions() {
        let disks = parse_diskstats(DISKSTATS);

        assert_eq!(
            disks.len(),
            1,
            "one physical disk, not one plus its partitions plus a loop device: {disks:?}"
        );
        let (name, read, written) = &disks[0];
        assert_eq!(name, "nvme0n1");
        assert_eq!(*read, 401_495 * 512, "field six is sectors read");
        assert_eq!(*written, 1_313_985 * 512, "field ten is sectors written");
    }

    #[test]
    fn an_app_image_loop_device_is_not_a_disk() {
        // Every installed Plex version is mounted from a loop device, so without this the
        // card grows an entry per version and none of them is a disk.
        assert!(!is_whole_disk("loop0"));
        assert!(!is_whole_disk("nvme0n1p6"));
        assert!(!is_whole_disk("sda1"));
        assert!(is_whole_disk("nvme0n1"));
        assert!(is_whole_disk("sda"));
    }

    #[test]
    fn gpu_frequency_is_a_share_of_the_hardware_ceiling_not_of_the_policy() {
        // Captured values: act 300, RP0 1100, and gt_max_freq_mhz was also 1100. A machine
        // whose max policy has been lowered is not a slower chip, so the ceiling is RP0.
        let gpu = Gpu {
            card: "card0".to_owned(),
            frequency_mhz: Some(550),
            max_frequency_mhz: Some(1100),
            min_frequency_mhz: Some(300),
        };
        assert_eq!(gpu.frequency_percent(), Some(50.0));

        let unknown = Gpu {
            card: "card0".to_owned(),
            frequency_mhz: None,
            max_frequency_mhz: Some(1100),
            min_frequency_mhz: None,
        };
        assert_eq!(unknown.frequency_percent(), None, "not zero");
    }

    #[test]
    fn a_percentage_is_clamped_into_the_range_a_reader_assumes() {
        assert_eq!(percent(150, 100), Some(100.0));
        assert_eq!(percent(0, 100), Some(0.0));
    }

    #[test]
    fn counters_are_paired_by_name_because_interfaces_come_and_go() {
        // The USB Ethernet adapter on this hardware enumerates seconds after everything
        // else, so a list that changes shape between two readings is the normal case. Paired
        // by position, `wlan0`'s counters would be attributed to whatever took its index.
        let before = vec![
            ("eth0".to_owned(), 1000, 2000),
            ("wlan0".to_owned(), 5000, 6000),
        ];
        let now = vec![("wlan0".to_owned(), 5500, 6500)];

        let rates = paired_rates(&before, &now, 1.0);

        assert_eq!(rates.len(), 1, "eth0 went away and is simply absent");
        let (rx, tx) = rates["wlan0"];
        assert!((rx - 500.0).abs() < f64::EPSILON, "500 bytes in one second");
        assert!((tx - 500.0).abs() < f64::EPSILON);
    }

    /// The same eight-core machine a moment later: 200 ticks of work on cpu0, and twenty
    /// idle ticks on every core including it.
    ///
    /// The idle ticks are the part worth spelling out. The first version of this fixture
    /// left the seven quiet cores byte-identical, which looked like the obvious way to say
    /// "these did nothing" and is a state no real machine can be in — an idle core still
    /// accrues *idle* ticks, twenty-five of them in 250 ms at 100 Hz. That fixture made the
    /// quiet cores report `None` rather than zero, and the honest reading of that failure is
    /// that the code was right and the fixture was fiction: a core with no tick movement at
    /// all has genuinely not been measured. Same trap as the imagined `resolv.conf`.
    const STAT_LATER: &str = "\
cpu  24040 90 2683 1652753 27606 0 888 0 0 0
cpu0 1961 10 329 207210 4169 0 4 0 0 0
cpu1 4297 18 395 205284 3501 0 6 0 0 0
cpu2 3701 21 330 205195 4099 0 46 0 0 0
cpu3 2582 22 325 206722 3840 0 5 0 0 0
cpu4 914 2 429 210035 928 0 813 0 0 0
cpu5 6463 7 275 203204 3655 0 3 0 0 0
cpu6 1586 2 314 208617 3073 0 4 0 0 0
cpu7 2533 3 282 206483 4337 0 4 0 0 0
ctxt 2390000
";

    /// A machine that answers with the captured files.
    fn machine(stat: &str) -> plexos_gpu::env::Fixture {
        plexos_gpu::env::Fixture::new()
            .file(PROC_STAT, stat)
            .file(PROC_MEMINFO, MEMINFO)
            .file(PROC_LOADAVG, "0.01 0.02 0.11 1/183 2213\n")
            .file(PROC_UPTIME, "2150.27 16525.94\n")
            .file(PROC_DISKSTATS, DISKSTATS)
    }

    #[test]
    fn a_machine_that_answers_nothing_still_produces_a_report() {
        // Every source unreadable, which is what a fixture with no files is. A dashboard
        // that refused to answer because one file was missing would take the console down
        // on exactly the machine somebody needed it for, so each figure is independently
        // optional.
        let metrics = Sampler::new().sample(&plexos_gpu::env::Fixture::new());

        assert_eq!(metrics.cpu.busy_percent, None);
        assert_eq!(metrics.cpu.cores_online, 0);
        assert_eq!(metrics.load, None);
        assert!(!metrics.plex.present, "no cgroup, so Plex is not running");
        assert!(
            !metrics.notes.is_empty(),
            "and it says what it could not measure rather than leaving blanks"
        );
    }

    #[test]
    fn the_first_reading_carries_no_rates_and_the_second_one_does() {
        let sampler = Sampler::new();

        let first = sampler.sample(&machine(STAT));
        assert_eq!(
            first.cpu.busy_percent, None,
            "a single reading of a since-boot counter is not a rate, and reporting zero \
             here would draw an idle machine"
        );
        assert_eq!(first.window_ms, None);
        assert_eq!(
            first.cpu.cores.len(),
            8,
            "the cores are still known -- it is only their percentages that are not"
        );

        // Past the floor, so this is a window the counters can be divided over.
        std::thread::sleep(MIN_WINDOW + Duration::from_millis(60));

        let second = sampler.sample(&machine(STAT_LATER));
        let busy = second.cpu.busy_percent.expect("two readings make a rate");
        assert!(
            busy > 0.0,
            "200 ticks of work between the readings and nothing else moved: {busy}"
        );
        assert!(second.window_ms.is_some_and(|ms| ms >= 250));
        assert!(
            second.cpu.cores[0].is_some_and(|p| p > 0.0),
            "cpu0 is where the work happened: {:?}",
            second.cpu.cores[0]
        );
        assert_eq!(
            second.cpu.cores[1],
            Some(0.0),
            "cpu1 did nothing, which is a measured zero rather than an absent one"
        );
    }

    #[test]
    fn a_second_browser_polling_at_once_gets_the_last_real_measurement() {
        // The case that makes the floor worth having: a phone and a laptop both polling.
        // Their requests interleave, and without this the one that lands just after the
        // other divides two ticks by twenty milliseconds and draws the result.
        let sampler = Sampler::new();
        sampler.sample(&machine(STAT));
        std::thread::sleep(MIN_WINDOW + Duration::from_millis(60));
        let measured = sampler.sample(&machine(STAT_LATER));

        let immediately = sampler.sample(&machine(STAT_LATER));

        assert_eq!(
            immediately.cpu.busy_percent, measured.cpu.busy_percent,
            "the second caller is answered from the last window rather than from one too \
             short to divide"
        );
        assert_eq!(immediately.window_ms, measured.window_ms);
    }

    #[test]
    fn the_processes_of_a_machine_are_listed_busiest_first() {
        let env = plexos_gpu::env::Fixture::new()
            .file(PROC_STAT, STAT)
            .file("/proc/182/stat", PLEX_STAT)
            .file(
                "/proc/182/status",
                "Name:\tPlex Media Serv\nVmRSS:\t   86208 kB\n",
            )
            .file(
                "/proc/182/cmdline",
                "/usr/lib/plexmediaserver/Plex Media Server\0",
            )
            .file(
                "/proc/1/stat",
                "1 (plexos-init) S 0 0 0 0 -1 0 0 0 0 0 3 1 0 0 20 0 1 0 5 100 400",
            )
            .file(
                "/proc/1/status",
                "Name:\tplexos-init\nVmRSS:\t    1024 kB\n",
            )
            .file(plex_cgroup().join("cgroup.procs"), "182\n");

        let report = Sampler::new().processes(&env);

        assert_eq!(report.total, 2, "pid 1 and pid 182");
        assert_eq!(report.omitted, 0);
        assert_eq!(
            report.window_ms, None,
            "the first listing has no previous reading, so no percentages"
        );

        let plex = report
            .processes
            .iter()
            .find(|p| p.pid == 182)
            .expect("Plex is listed");
        assert!(
            plex.plex,
            "and is known to be Plex by its cgroup, not its name"
        );
        assert_eq!(plex.memory, Some(86208 * 1024), "VmRSS, in bytes");
        assert_eq!(
            plex.command.as_deref(),
            Some("/usr/lib/plexmediaserver/Plex Media Server"),
            "the full command, because comm is truncated to fifteen characters"
        );
        assert_eq!(plex.threads, Some(20));

        // Nothing has a percentage yet, so the order falls back to memory -- otherwise the
        // first list a page draws is in pid order and looks like a sort that failed.
        assert_eq!(report.processes[0].pid, 182, "the larger of the two");
        assert!(
            report.processes.iter().all(|p| !p.plex || p.pid == 182),
            "only the cgroup's member is tagged"
        );
    }

    #[test]
    fn the_captured_plex_cgroup_reads_as_a_running_server() {
        // Values straight off the appliance: 528 MB held against a 6.0 GiB ceiling.
        let env = plexos_gpu::env::Fixture::new()
            .file(
                plex_cgroup().join("cpu.stat"),
                "usage_usec 258044260\nuser_usec 239843840\nsystem_usec 18200420\nnice_usec 920000\n",
            )
            .file(
                plex_cgroup().join("memory.current"),
                "554053632\n",
            )
            .file(
                plex_cgroup().join("memory.max"),
                "6461243392\n",
            )
            .file(
                plex_cgroup().join("pids.current"),
                "45\n",
            );

        let plex = read_plex(&env, Some(140.0));

        assert!(plex.present);
        assert_eq!(plex.memory, Some(554_053_632));
        assert_eq!(plex.memory_max, Some(6_461_243_392));
        assert_eq!(plex.pids, Some(45));
        assert_eq!(
            plex.cpu_percent,
            Some(140.0),
            "over 100 on purpose: a transcode uses more than one core, and capping it \
             would answer a different question"
        );
    }

    #[test]
    fn most_of_what_is_charged_to_plex_is_films_the_kernel_will_hand_back() {
        // Captured from the appliance after an evening of playing, and the reason the gauge
        // changed: `memory.current` read 4.33 GB against a 13.1 GB ceiling, which drew as a
        // third of the way to a limit and looked like a leak. 534 MB of it was Plex. The rest
        // was the page cache of the films it had read, and 3.0 GB of that was `inactive_file`
        // — memory the kernel returns the moment anything else wants it.
        //
        // Somebody asked what the figure meant, which is the sort of question a number nobody
        // can act on provokes.
        let env = plexos_gpu::env::Fixture::new()
            .file(plex_cgroup().join("cpu.stat"), "usage_usec 1\n")
            .file(plex_cgroup().join("memory.current"), "4330409984\n")
            .file(plex_cgroup().join("memory.max"), "13105426432\n")
            .file(
                plex_cgroup().join("memory.stat"),
                // Trimmed to the keys that matter, in the order the kernel writes them. The
                // two decoys are real and adjacent: a reader matching on `anon` alone takes
                // `anon_thp`, and one matching on `file` takes `file_mapped`.
                "anon 534048768\nfile 3775758336\nkernel 18046976\nslab 13659216\n\
                 sock 40960\nshmem 764751872\nfile_mapped 73232384\nfile_dirty 0\n\
                 inactive_file 2997583872\nactive_file 17551360\nanon_thp 0\n",
            );

        let plex = read_plex(&env, None);

        assert_eq!(
            plex.memory_anon,
            Some(534_048_768),
            "what Plex itself holds"
        );
        assert_eq!(
            plex.memory_cache,
            Some(3_775_758_336),
            "and the films, which are not Plex using memory"
        );
        assert_eq!(
            plex.memory,
            Some(4_330_409_984),
            "the total is kept: it answers what this cgroup would have to give back"
        );
        assert!(
            plex.memory_anon.unwrap() * 4 < plex.memory.unwrap(),
            "the whole point — the honest figure is a fraction of the alarming one"
        );
    }

    #[test]
    fn a_cgroup_with_no_breakdown_still_reports_what_it_has() {
        // An older kernel, or a file that cannot be read. The gauge falls back to the total
        // rather than to nothing, because a card that goes blank is worse than one that is
        // pessimistic.
        let env = plexos_gpu::env::Fixture::new()
            .file(plex_cgroup().join("cpu.stat"), "usage_usec 1\n")
            .file(plex_cgroup().join("memory.current"), "554053632\n");

        let plex = read_plex(&env, None);
        assert_eq!(plex.memory, Some(554_053_632));
        assert_eq!(plex.memory_anon, None);
        assert_eq!(plex.memory_cache, None);
    }

    #[test]
    fn a_stopped_plex_leaves_a_directory_that_is_not_a_running_server() {
        // The cgroup directory outlives the process, so its existence cannot be the test.
        // `cpu.stat` is, because a directory with no accounting in it is what a stopped Plex
        // leaves behind and reporting that as present would have the card claim a server.
        let env = plexos_gpu::env::Fixture::new().file(plex_cgroup().join("cgroup.procs"), "");

        assert!(!read_plex(&env, None).present);
    }

    #[test]
    fn the_captured_gpu_frequencies_read_as_a_part_sitting_at_idle() {
        // act 300, RP0 1100, RPn 300 -- the reference laptop with nothing to draw.
        let env = plexos_gpu::env::Fixture::new()
            .file("/sys/class/drm/card0/gt_act_freq_mhz", "300\n")
            .file("/sys/class/drm/card0/gt_RP0_freq_mhz", "1100\n")
            .file("/sys/class/drm/card0/gt_RPn_freq_mhz", "300\n");

        let gpu = read_gpu(&env).expect("a card with frequency files");

        assert_eq!(gpu.card, "card0");
        assert_eq!(gpu.frequency_mhz, Some(300));
        assert_eq!(gpu.max_frequency_mhz, Some(1100));
        assert_eq!(
            gpu.min_frequency_mhz,
            Some(300),
            "and the minimum equals the current, which is what idle looks like -- the page \
             must not call this a load of 27%"
        );
    }

    #[test]
    fn a_card_with_no_frequency_files_is_told_apart_from_no_card_at_all() {
        // `read_gpu` returns `None` for both, and the note built on that `None` used to
        // guess: it named "no DRM card" or "the driver is xe". The first machine anybody
        // read it on was an RTX desktop where a card was present and the driver was
        // `nvidia`, so the note listed two causes and neither was the one happening.
        //
        // Two states, opposite remedies -- attach a graphics card, versus ask which driver
        // is bound. The same distinction `plexos_gpu::display_devices` exists to make.
        let nvidia = plexos_gpu::env::Fixture::new()
            .file("/sys/class/drm/card0/device/vendor", "0x10de\n")
            .file("/sys/class/drm/card0/device/uevent", "DRIVER=nvidia\n");
        assert!(
            drm_card_present(&nvidia),
            "a card whose driver publishes no i915 frequency files is still a card"
        );
        assert!(
            read_gpu(&nvidia).is_none(),
            "and it has no frequency to read"
        );

        let headless = plexos_gpu::env::Fixture::new();
        assert!(!drm_card_present(&headless));

        let with_card = notes_about(&[], None, true).join(" ");
        let without = notes_about(&[], None, false).join(" ");

        assert!(
            with_card.contains("does not publish them"),
            "a card that is present must be reported as present: {with_card}"
        );
        assert!(
            !with_card.contains("no DRM card at all"),
            "and must never be described as absent: {with_card}"
        );
        assert!(
            without.contains("no DRM card at all"),
            "a machine with no card says so plainly: {without}"
        );
    }

    #[test]
    fn a_virtual_interface_is_not_the_network() {
        // Only hardware has a `device` symlink. Bridges and veth pairs are ARPHRD_ETHER and
        // report a carrier, and they sort before the real interface by name -- so a card
        // that trusted the type would draw docker0's throughput and call it the network.
        let env = plexos_gpu::env::Fixture::new()
            .file("/sys/class/net/docker0/statistics/rx_bytes", "999\n")
            .file("/sys/class/net/docker0/statistics/tx_bytes", "999\n")
            .file("/sys/class/net/wlan0/statistics/rx_bytes", "353993840\n")
            .file("/sys/class/net/wlan0/statistics/tx_bytes", "393785631\n")
            .link("/sys/class/net/wlan0/device", "../../../0000:00:14.3");

        let interfaces = read_interface_counters(&env);

        assert_eq!(
            interfaces.len(),
            1,
            "docker0 has no device symlink: {interfaces:?}"
        );
        assert_eq!(interfaces[0].0, "wlan0");
        assert_eq!(interfaces[0].1, 353_993_840);
    }

    #[test]
    fn both_captured_thermal_zones_are_reported_under_the_kernels_own_names() {
        // acpitz and iwlwifi_1, and neither is a processor die. A page that renamed them to
        // something friendlier would name a zone nobody can look up.
        let env = plexos_gpu::env::Fixture::new()
            .file("/sys/class/thermal/thermal_zone0/type", "acpitz\n")
            .file("/sys/class/thermal/thermal_zone0/temp", "27800\n")
            .file("/sys/class/thermal/thermal_zone1/type", "iwlwifi_1\n")
            .file("/sys/class/thermal/thermal_zone1/temp", "35000\n");

        let zones = read_temperatures(&env);

        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].zone, "acpitz");
        assert_eq!(zones[0].sensor, "thermal_zone0");
        assert!(
            (zones[0].celsius - 27.8).abs() < 0.001,
            "millidegrees to degrees"
        );
        assert_eq!(zones[1].zone, "iwlwifi_1");
    }

    #[test]
    fn two_zones_of_the_same_type_are_told_apart() {
        // Not hypothetical. The first development host this was run on answered with two
        // zones both typed `acpitz`, at 27.8 °C and 29.8 °C -- so a table keyed on the type
        // alone showed one label twice and no way to know which reading belonged to what.
        // The same distinction the console page has been bitten by twice: not whether a name
        // resolves, but whether it resolves to one thing.
        let env = plexos_gpu::env::Fixture::new()
            .file("/sys/class/thermal/thermal_zone0/type", "acpitz\n")
            .file("/sys/class/thermal/thermal_zone0/temp", "27800\n")
            .file("/sys/class/thermal/thermal_zone3/type", "acpitz\n")
            .file("/sys/class/thermal/thermal_zone3/temp", "29800\n");

        let zones = read_temperatures(&env);

        assert_eq!(zones.len(), 2, "both are reported");
        assert_eq!(zones[0].zone, zones[1].zone, "and they share a type");
        assert_ne!(
            zones[0].sensor, zones[1].sensor,
            "so the sensor is what tells them apart: {zones:?}"
        );
        assert_eq!(zones[0].sensor, "thermal_zone0", "in a stable order");
        assert_eq!(zones[1].sensor, "thermal_zone3");
    }

    #[test]
    fn a_machine_with_a_package_sensor_is_not_told_it_has_none() {
        // The note was first written as "this kernel has no coretemp driver", which was true
        // of the appliance and false of the machine it was next run on -- that one has an
        // `x86_pkg_temp` zone, so the note denied a number printed three lines above it.
        let env = plexos_gpu::env::Fixture::new()
            .file("/sys/class/thermal/thermal_zone0/type", "x86_pkg_temp\n")
            .file("/sys/class/thermal/thermal_zone0/temp", "46000\n");

        let notes = Sampler::new().sample(&env).notes.join("\n");

        assert!(
            !notes.contains("No processor die temperature"),
            "it has one, and saying otherwise is a diagnostic about a different machine: \
             {notes}"
        );
    }

    #[test]
    fn a_machine_with_no_die_sensor_says_so_rather_than_leaving_a_blank() {
        // Every diagnostic names a remedy, and "there is no number here" is a diagnostic.
        let metrics = Sampler::new().sample(&plexos_gpu::env::Fixture::new());
        let notes = metrics.notes.join("\n");

        assert!(
            notes.contains("CONFIG_X86_PKG_TEMP_THERMAL"),
            "it names the kernel option that would add one, and the right one: the zone \
             comes from the thermal driver, not from the hwmon one: {notes}"
        );
        assert!(
            notes.contains("perf_event_open") || notes.contains("driver"),
            "and says why there is no GPU load figure: {notes}"
        );
    }

    #[test]
    fn an_unset_memory_ceiling_is_not_a_number() {
        // cgroup v2 writes the literal `max` for no limit. Parsing that as a number fails,
        // which is the right answer, but it had to be a deliberate one rather than a zero.
        assert_eq!("max".trim().parse::<u64>().ok(), None);
    }
}

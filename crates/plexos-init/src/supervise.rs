//! Keeping the services running, and being the PID 1 that has to (ARCHITECTURE.md §2).
//!
//! Until this existed, `plexos-init` printed "no supervisor yet", `exec`ed a shell, and
//! stopped being a process of its own. Two things followed, and both were invisible until
//! something needed them:
//!
//! **Nothing restarted anything.** A `plexosd` that died took the console, the health
//! gate, the update path and Plex's supervision with it, on a machine with no keyboard
//! anybody is expected to use. The remedy for a crashed daemon was the power button.
//!
//! **Nothing reaped orphans.** Every process whose parent dies is reparented to PID 1 and
//! stays a zombie until PID 1 waits for it. A shell does that for its own children and not
//! for anything else, and this appliance produces orphans routinely — `plexosd` spawns
//! `curl`, `ip`, `udhcpc` and the confined Plex child. The exhaustion is slow and the
//! symptom is not "too many zombies": it is `fork` failing somewhere unrelated, weeks
//! later, on a machine nobody has logged into.
//!
//! # The shell is a service now
//!
//! It used to *be* PID 1. It is respawned like a getty instead, so typing `exit` gives
//! another one rather than panicking the kernel — which is what exiting PID 1 does. It
//! also means a shell that is killed comes back, and that PID 1 is a program whose
//! behaviour is written down here rather than whatever `ash` does.
//!
//! # Restarting is not free, and a crash loop must not be one
//!
//! A service that dies immediately and is restarted immediately is a machine that does
//! nothing else. The delay grows with consecutive failures and is reset once a service has
//! stayed up long enough to count as working, so an occasional crash costs nothing and a
//! permanent fault settles into a slow retry that leaves the console readable.
//!
//! # What has run
//!
//! **Nothing on hardware.** [`Supervisor::tick`] is exercised against a fake environment;
//! no appliance has yet booted with a PID 1 that stays alive.

use std::io;

use plexos_sys::process::Reaped;

/// How often the loop looks for work.
///
/// Five times a second. Fast enough that a restarted console is back before anybody
/// refreshes a page, and cheap enough to be invisible on a machine that is otherwise
/// transcoding video.
pub const POLL_MS: u64 = 200;

/// How long a service must stay up before its failure count is forgiven.
///
/// A minute. Below this, restarts are counted as one continuing failure rather than as
/// separate incidents — otherwise a service that crashes after five seconds, forever,
/// would reset its own backoff every time and retry as fast as it could.
pub const STABLE_MS: u64 = 60_000;

/// Delay before each successive restart attempt.
///
/// The first restart is immediate, because the overwhelming majority of restarts are the
/// shell being exited by somebody who wants another one.
pub const BACKOFF_MS: &[u64] = &[0, 1_000, 2_000, 5_000, 10_000, 30_000];

/// Something PID 1 keeps running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Service {
    /// What to call it in the log.
    pub name: &'static str,
    /// Absolute path to the program.
    ///
    /// Absolute, and not a name to be searched for: PID 1 gets the environment the kernel
    /// provides, which is empty, so there is no `PATH` and glibc's fallback finds only
    /// `/bin` and `/usr/bin`. A name here would fail with a bare `ENOENT` for programs
    /// that are present.
    pub program: &'static str,
    /// Its arguments.
    pub args: &'static [&'static str],
    /// Environment to give it, since it inherits none worth having.
    pub env: &'static [(&'static str, &'static str)],
    /// A terminal to give it instead of PID 1's own, if it should not share the screen.
    ///
    /// `None` means it inherits stdin, stdout and stderr from PID 1 — which is
    /// `/dev/console`, which is the foreground virtual terminal, which is the screen
    /// somebody is looking at. That was right while everything on this machine was a log.
    ///
    /// It stopped being right when the screen became a *drawing* (ADR-0019): a dashboard
    /// and a daemon's log cannot share a terminal, because the log wins and the result is a
    /// dashboard with lines through it. So the console shell and `plexosd`'s output go to
    /// `/dev/tty2` and the dashboard has `/dev/tty1` to itself, one Alt+F2 apart.
    ///
    /// This does not give the service a *controlling* terminal — nothing here calls
    /// `setsid` or `TIOCSCTTY`, and nothing did before either. Job control on the console
    /// shell is therefore exactly as absent as it has always been, which is worth saying
    /// out loud so that its absence is not read as something this change took away.
    pub tty: Option<&'static str>,
}

/// What the supervisor did, in the words the console gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A service was started.
    Started {
        /// Which service.
        name: &'static str,
        /// Its process.
        pid: u32,
    },
    /// A service exited, and will be started again after `delay_ms`.
    Exited {
        /// Which service.
        name: &'static str,
        /// How it went.
        reaped: Reaped,
        /// How long before the next attempt.
        delay_ms: u64,
    },
    /// A service could not be started at all.
    CouldNotStart {
        /// Which service.
        name: &'static str,
        /// Why.
        error: String,
        /// How long before the next attempt.
        delay_ms: u64,
    },
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Started { name, pid } => write!(f, "{name} started as pid {pid}"),
            Self::Exited {
                name,
                reaped,
                delay_ms: 0,
            } => write!(f, "{name}: {reaped}; starting another"),
            Self::Exited {
                name,
                reaped,
                delay_ms,
            } => write!(
                f,
                "{name}: {reaped}; starting another in {}s. Repeated failures are slowed \
                 down deliberately, so that a service which cannot run leaves the console \
                 usable instead of consuming the machine.",
                delay_ms / 1000
            ),
            Self::CouldNotStart {
                name,
                error,
                delay_ms,
            } => write!(
                f,
                "{name} could not be started: {error}. Retrying in {}s. Remedy: this is a \
                 program missing from the image or one that cannot be executed, not a \
                 configuration -- check that its path exists in /usr.",
                delay_ms / 1000
            ),
        }
    }
}

/// What a supervisor needs from the world.
///
/// A trait so that [`Supervisor::tick`] can be tested against services that die on
/// command. The alternative is a loop that can only be exercised by being PID 1 on a
/// machine, which is how this project has previously shipped logic that had never run in
/// the state it was written for.
pub trait Environment {
    /// Starts a service, returning its process id.
    ///
    /// # Errors
    /// If the program cannot be executed.
    fn spawn(&mut self, service: &Service) -> io::Result<u32>;

    /// Collects one exited child, if any has exited.
    ///
    /// # Errors
    /// Only for a failure that is not "nothing to collect".
    fn reap(&mut self) -> io::Result<Option<Reaped>>;
}

/// What the supervisor knows about one service.
#[derive(Debug, Default)]
struct State {
    pid: Option<u32>,
    /// Consecutive failures, capped at the length of [`BACKOFF_MS`].
    failures: usize,
    /// When the current process was started, for deciding whether it counts as stable.
    started_at_ms: u64,
    /// The earliest this may be started again.
    next_attempt_ms: u64,
}

/// Keeps a fixed set of services running.
#[derive(Debug)]
pub struct Supervisor {
    services: &'static [Service],
    states: Vec<State>,
}

impl Supervisor {
    /// A supervisor that has started nothing.
    #[must_use]
    pub fn new(services: &'static [Service]) -> Self {
        Self {
            services,
            states: services.iter().map(|_| State::default()).collect(),
        }
    }

    /// One pass: collect what has exited, then start whatever is due.
    ///
    /// `now_ms` is monotonic milliseconds since the supervisor started. Passed in rather
    /// than read, so that backoff can be tested without a test that waits thirty seconds.
    pub fn tick(&mut self, now_ms: u64, environment: &mut dyn Environment) -> Vec<Event> {
        let mut events = Vec::new();

        // Drained rather than collected one at a time. Several children can exit between
        // two ticks -- and most of what is collected here belongs to nobody in this table:
        // orphans reparented when their parent died, which are reaped silently because
        // logging every `curl` an update runs would bury everything worth reading.
        while let Ok(Some(reaped)) = environment.reap() {
            let Some(index) = self.states.iter().position(|s| s.pid == Some(reaped.pid)) else {
                continue;
            };

            let state = &mut self.states[index];
            state.pid = None;

            // A service that stayed up is forgiven its history. Without this, one crash a
            // week would eventually reach the longest delay and stay there.
            if now_ms.saturating_sub(state.started_at_ms) >= STABLE_MS {
                state.failures = 0;
            }

            let delay_ms = backoff(state.failures);
            state.failures = state.failures.saturating_add(1);
            state.next_attempt_ms = now_ms.saturating_add(delay_ms);

            events.push(Event::Exited {
                name: self.services[index].name,
                reaped,
                delay_ms,
            });
        }

        for (index, service) in self.services.iter().enumerate() {
            let state = &mut self.states[index];
            if state.pid.is_some() || now_ms < state.next_attempt_ms {
                continue;
            }

            match environment.spawn(service) {
                Ok(pid) => {
                    state.pid = Some(pid);
                    state.started_at_ms = now_ms;
                    events.push(Event::Started {
                        name: service.name,
                        pid,
                    });
                }
                Err(error) => {
                    // Counted as a failure, so a program missing from the image backs off
                    // exactly like one that crashes. Otherwise this retries as fast as the
                    // loop runs and fills the console with the same line.
                    let delay_ms = backoff(state.failures);
                    state.failures = state.failures.saturating_add(1);
                    state.next_attempt_ms = now_ms.saturating_add(delay_ms);
                    events.push(Event::CouldNotStart {
                        name: service.name,
                        error: error.to_string(),
                        delay_ms,
                    });
                }
            }
        }

        events
    }

    /// The process running each service, in the order the services were given.
    #[must_use]
    pub fn pids(&self) -> Vec<Option<u32>> {
        self.states.iter().map(|s| s.pid).collect()
    }
}

/// Three handles on one terminal, for a child's standard streams.
///
/// Three opens rather than one and two `try_clone`s, and the difference matters: cloned
/// descriptors share a file offset and a set of status flags, so a service that put its
/// terminal into a different mode would change it for the other two. Separate opens are
/// three independent descriptions of the same device, which is what a shell expects.
fn open_terminal(
    path: &str,
) -> io::Result<(
    std::process::Stdio,
    std::process::Stdio,
    std::process::Stdio,
)> {
    let open = || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(std::process::Stdio::from)
    };
    Ok((open()?, open()?, open()?))
}

/// The delay before attempt number `failures`.
fn backoff(failures: usize) -> u64 {
    BACKOFF_MS[failures.min(BACKOFF_MS.len() - 1)]
}

/// The world, as PID 1 finds it.
#[derive(Debug, Default)]
pub struct System {
    /// The children this has started.
    ///
    /// Held so that Rust does not close its half of anything early, and dropped when the
    /// process is reaped. `Child::drop` deliberately does *not* wait, so nothing here
    /// collects a status behind the supervisor's back — which would leave `reap` never
    /// seeing the exit and the service never being restarted.
    children: Vec<std::process::Child>,
}

impl Environment for System {
    fn spawn(&mut self, service: &Service) -> io::Result<u32> {
        let mut command = std::process::Command::new(service.program);
        command.args(service.args);
        for (key, value) in service.env {
            command.env(key, value);
        }

        // stdin, stdout and stderr are inherited unless the service asked for a terminal
        // of its own. PID 1 holds /dev/console, so a child that inherits them is talking to
        // the screen attached to the machine — which is exactly what the dashboard needs
        // nothing else to be doing.
        //
        // A terminal that cannot be opened leaves the service on PID 1's own console,
        // which is a machine whose screen is a log again rather than one that will not
        // boot. Nothing is said about it here that the service will not say better itself,
        // on the console it falls back to.
        if let Some(path) = service.tty
            && let Ok((stdin, stdout, stderr)) = open_terminal(path)
        {
            command.stdin(stdin).stdout(stdout).stderr(stderr);
        }

        let child = command.spawn()?;
        let pid = child.id();
        self.children.push(child);
        Ok(pid)
    }

    fn reap(&mut self) -> io::Result<Option<Reaped>> {
        let reaped = plexos_sys::process::reap()?;
        if let Some(reaped) = reaped {
            self.children.retain(|child| child.id() != reaped.pid);
        }
        Ok(reaped)
    }
}

/// Runs the services for the life of the machine.
///
/// Never returns, and must not: PID 1 exiting is a kernel panic, and a supervisor that
/// gave up on a machine somebody is trying to reach over the network would take the
/// console with it.
pub fn run(services: &'static [Service], log: &mut dyn FnMut(&str)) -> ! {
    let mut supervisor = Supervisor::new(services);
    let mut system = System::default();
    let started = std::time::Instant::now();

    loop {
        let now_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        for event in supervisor.tick(now_ms, &mut system) {
            log(&event.to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ONE: &[Service] = &[Service {
        name: "thing",
        program: "/bin/true",
        args: &[],
        env: &[],
        tty: None,
    }];

    static TWO: &[Service] = &[
        Service {
            name: "console",
            program: "/bin/true",
            args: &[],
            env: &[],
            tty: None,
        },
        Service {
            name: "shell",
            program: "/bin/true",
            args: &[],
            env: &[],
            tty: None,
        },
    ];

    /// A world in which services start when told and die when told.
    #[derive(Default)]
    struct Fake {
        next_pid: u32,
        exits: Vec<Reaped>,
        refuse: bool,
        spawned: Vec<&'static str>,
    }

    impl Fake {
        fn kill(&mut self, pid: u32, code: i32) {
            self.exits.push(Reaped {
                pid,
                code: Some(code),
                signal: None,
            });
        }
    }

    impl Environment for Fake {
        fn spawn(&mut self, service: &Service) -> io::Result<u32> {
            if self.refuse {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no such program"));
            }
            self.next_pid += 1;
            self.spawned.push(service.name);
            Ok(self.next_pid)
        }

        fn reap(&mut self) -> io::Result<Option<Reaped>> {
            Ok(self.exits.pop())
        }
    }

    #[test]
    fn every_service_is_started_on_the_first_pass() {
        let mut supervisor = Supervisor::new(TWO);
        let mut world = Fake::default();

        let events = supervisor.tick(0, &mut world);
        assert_eq!(events.len(), 2);
        assert_eq!(world.spawned, vec!["console", "shell"]);
        assert_eq!(supervisor.pids(), vec![Some(1), Some(2)]);
    }

    #[test]
    fn a_service_that_dies_is_started_again() {
        // The whole reason this module exists. A plexosd that died took the console, the
        // health gate and the update path with it, on a machine whose only other input is
        // the power button.
        let mut supervisor = Supervisor::new(ONE);
        let mut world = Fake::default();
        supervisor.tick(0, &mut world);

        world.kill(1, 1);
        let events = supervisor.tick(10, &mut world);

        assert!(matches!(
            events.first(),
            Some(Event::Exited { delay_ms: 0, .. })
        ));
        assert!(matches!(events.get(1), Some(Event::Started { pid: 2, .. })));
        assert_eq!(supervisor.pids(), vec![Some(2)]);
    }

    #[test]
    fn a_service_that_keeps_dying_is_retried_more_and_more_slowly() {
        // A crash loop restarted at full speed is a machine that does nothing else,
        // including nothing about being fixed.
        let mut supervisor = Supervisor::new(ONE);
        let mut world = Fake::default();
        supervisor.tick(0, &mut world);

        let mut delays = Vec::new();
        let mut now = 0;
        for _ in 0..6 {
            // Wait out whatever delay is in force. The jump is longer than the longest
            // backoff and shorter than the stability threshold, so the service is always
            // restarted and never looks like it survived.
            while supervisor.pids()[0].is_none() {
                now += 40_000;
                supervisor.tick(now, &mut world);
            }

            let pid = supervisor.pids()[0].expect("running");
            world.kill(pid, 1);
            now += 1;
            for event in supervisor.tick(now, &mut world) {
                if let Event::Exited { delay_ms, .. } = event {
                    delays.push(delay_ms);
                }
            }
        }

        assert_eq!(delays, vec![0, 1_000, 2_000, 5_000, 10_000, 30_000]);
    }

    #[test]
    fn a_service_that_stayed_up_is_forgiven_its_history() {
        // Otherwise one crash a week eventually reaches the longest delay and stays there,
        // so a machine that has been running for a year restarts its console half a minute
        // slower than one that booted this morning.
        let mut supervisor = Supervisor::new(ONE);
        let mut world = Fake::default();
        supervisor.tick(0, &mut world);

        world.kill(1, 1);
        supervisor.tick(1_000, &mut world);
        world.kill(2, 1);
        let events = supervisor.tick(2_000, &mut world);
        assert!(
            matches!(
                events.first(),
                Some(Event::Exited {
                    delay_ms: 1_000,
                    ..
                })
            ),
            "a second quick failure waits longer"
        );

        // Now let one live past the stability threshold.
        let pid = loop {
            if let Some(pid) = supervisor.pids()[0] {
                break pid;
            }
            supervisor.tick(10_000, &mut world);
        };
        world.kill(pid, 1);
        let events = supervisor.tick(10_000 + STABLE_MS, &mut world);
        assert!(
            matches!(events.first(), Some(Event::Exited { delay_ms: 0, .. })),
            "a service that stayed up is restarted immediately, {events:?}"
        );
    }

    #[test]
    fn nothing_is_started_before_its_delay_has_passed() {
        let mut supervisor = Supervisor::new(ONE);
        let mut world = Fake::default();
        supervisor.tick(0, &mut world);
        world.kill(1, 1);
        supervisor.tick(0, &mut world); // restarts at once: first failure
        world.kill(2, 1);
        supervisor.tick(0, &mut world); // second failure: one second

        let events = supervisor.tick(500, &mut world);
        assert!(events.is_empty(), "too early: {events:?}");
        assert_eq!(supervisor.pids(), vec![None]);

        let events = supervisor.tick(1_000, &mut world);
        assert!(matches!(events.first(), Some(Event::Started { .. })));
    }

    #[test]
    fn a_program_that_is_not_in_the_image_backs_off_like_a_crash() {
        // Otherwise this retries five times a second forever and fills the console with
        // one line, which is the state in which somebody most needs to read the console.
        let mut supervisor = Supervisor::new(ONE);
        let mut world = Fake {
            refuse: true,
            ..Fake::default()
        };

        let events = supervisor.tick(0, &mut world);
        match events.first() {
            Some(Event::CouldNotStart {
                error, delay_ms, ..
            }) => {
                assert!(error.contains("no such program"), "{error}");
                assert_eq!(*delay_ms, 0);
            }
            other => panic!("expected a failure to start, got {other:?}"),
        }

        // One immediate retry, because the commonest transient failure deserves it, and
        // then the same widening gap a crash gets.
        match supervisor.tick(1, &mut world).first() {
            Some(Event::CouldNotStart { delay_ms, .. }) => assert_eq!(*delay_ms, 1_000),
            other => panic!("expected one immediate retry, got {other:?}"),
        }
        assert!(supervisor.tick(2, &mut world).is_empty(), "then it waits");
        assert!(matches!(
            supervisor.tick(1_002, &mut world).first(),
            Some(Event::CouldNotStart { .. })
        ));
    }

    #[test]
    fn an_orphan_is_collected_without_disturbing_anything() {
        // Most of what PID 1 reaps belongs to nobody in the service table: plexosd's curl,
        // ip, udhcpc and the confined Plex child all end up here when their parent goes.
        // They must be collected -- that is the entire reason for reaping -- and they must
        // not be mistaken for a service exiting.
        let mut supervisor = Supervisor::new(ONE);
        let mut world = Fake::default();
        supervisor.tick(0, &mut world);

        world.kill(9_999, 0);
        let events = supervisor.tick(10, &mut world);

        assert!(events.is_empty(), "{events:?}");
        assert_eq!(supervisor.pids(), vec![Some(1)], "the service is untouched");
    }

    #[test]
    fn several_children_exiting_between_ticks_are_all_collected() {
        // The loop drains rather than taking one per pass. A tick that collected one would
        // leave a zombie for every extra exit until the next one, which on a busy update
        // is most of them.
        let mut supervisor = Supervisor::new(TWO);
        let mut world = Fake::default();
        supervisor.tick(0, &mut world);

        world.kill(1, 0);
        world.kill(2, 0);
        world.kill(4_242, 0);

        let events = supervisor.tick(10, &mut world);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Exited { .. }))
                .count(),
            2
        );
        assert!(world.exits.is_empty(), "nothing left uncollected");
    }

    #[test]
    fn every_message_a_reader_might_have_to_act_on_names_a_remedy() {
        // The rule plexos-gpu enforces with a test. A console line saying a service will
        // not start, and nothing else, has reproduced the problem it was reporting.
        let could_not_start = Event::CouldNotStart {
            name: "console",
            error: "No such file or directory".to_owned(),
            delay_ms: 5_000,
        };
        assert!(could_not_start.to_string().contains("Remedy:"));

        let slowed = Event::Exited {
            name: "console",
            reaped: Reaped {
                pid: 7,
                code: Some(1),
                signal: None,
            },
            delay_ms: 30_000,
        };
        assert!(
            slowed.to_string().contains("deliberately"),
            "a widening gap between restarts must read as a decision rather than as the \
             machine getting slower: {slowed}"
        );
    }

    #[test]
    fn the_backoff_table_is_ordered_and_ends_somewhere_a_machine_can_be_reached() {
        assert!(BACKOFF_MS.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(BACKOFF_MS[0], 0, "the common case is a shell being exited");
        assert!(
            *BACKOFF_MS.last().unwrap() <= 60_000,
            "a machine whose console is restarting must not take minutes to answer"
        );
        assert_eq!(backoff(usize::MAX), 30_000, "the cap holds past the table");
    }
}

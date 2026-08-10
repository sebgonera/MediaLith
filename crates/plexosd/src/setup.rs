//! What a freshly installed appliance still needs, in order (ADR-0016).
//!
//! Every step here already had an endpoint and a card on the console. What did not exist
//! was any sense of *sequence*, or of a machine that is not set up yet: a appliance five
//! minutes old showed exactly the page one that had been running for a year showed, with an
//! install button among the diagnostics and nothing saying that installing was the next
//! thing to do.
//!
//! # It computes, it does not remember
//!
//! There is no "setup progress" written anywhere. Every step is derived from the state it
//! is about — is there an app image, does Plex answer, is there a configuration file — so
//! the wizard cannot drift from the machine, cannot be completed by something that did not
//! happen, and cannot be reset by clearing a flag. A machine that has its Plex removed goes
//! back to needing one, which is true.
//!
//! That is also why the first step is not here. "Has somebody entered the device token" is
//! a fact about a browser, not about the appliance: the token is generated at first start
//! whether or not anyone has read it off the screen (ADR-0013). The page knows, and the
//! page asks.
//!
//! # What has run
//!
//! **All of it, on the reference laptop**, which has been through every step and reports
//! them from `/api/setup`. It also found the limit of "it computes, it does not remember":
//! computing is only as honest as the fact chosen, and the claim step was computed from
//! whether Plex answered. That is true of every running Plex, so the step was done the
//! moment Plex started and the appliance announced a finished setup while sitting signed
//! out. A derived step cannot drift from the machine; it can still be derived from the
//! wrong part of it.

use std::path::Path;

use plexos_types::paths;

/// Where a step has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Already true.
    Done,
    /// The thing to do next.
    Next,
    /// Not yet, and not the next thing either.
    Later,
}

/// One thing a new appliance needs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Step {
    /// Stable identifier, for the page to key on.
    pub id: &'static str,
    /// What it is called.
    pub title: &'static str,
    /// What is true, or what to do about it.
    pub detail: String,
    /// Where it has got to.
    pub state: State,
    /// Whether setup is unfinished without it.
    ///
    /// Shares are not required: a library can be on a disk in the machine, and an appliance
    /// that called itself unfinished for ever because nobody mounted a NAS would be wrong
    /// about its own state.
    pub required: bool,
}

/// What the appliance can see about itself.
///
/// Gathered by the caller so the ordering can be tested without a machine — which is the
/// only way to exercise the states this will spend most of its life *not* in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// Whether an address exists that somebody could have typed to get here.
    pub reachable: bool,
    /// Whether a configuration file has ever been written (ADR-0008).
    pub configured: bool,
    /// Whether any media share is configured.
    pub has_shares: bool,
    /// What Plex is doing.
    pub plex: Plex,
}

/// The three things worth knowing about Plex, and no two of them are the same thing.
///
/// Installed and answering differ in both directions and for twenty seconds at a time:
/// "install Plex" is the wrong advice for a server that is starting, and "it is starting"
/// is the wrong advice for one that was never installed.
///
/// Answering and claimed differ for as long as nobody signs in — which on this appliance
/// turned out to be *indefinitely*, because an interrupted stop can empty the file the
/// account token lives in. This field exists because the step below used `answering` for
/// the claim and therefore could never be anything but done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plex {
    /// Whether an app image is installed.
    pub installed: bool,
    /// Whether it is answering on loopback.
    pub answering: bool,
    /// Whether a Plex account owns it.
    pub claimed: bool,
}

impl Facts {
    /// Reads them off this machine.
    #[must_use]
    pub fn observe() -> Self {
        // Asked once and used twice. The claim question is only worth putting to a server
        // that is answering, and a second connection to a port with nothing behind it
        // costs the page a timeout for an answer already known.
        let answering = crate::plex::is_answering();

        Self {
            // If this is being asked, something reached the console. Reported anyway,
            // because the answer stops being obvious the moment somebody sets a static
            // address and reboots.
            reachable: true,
            configured: Path::new(paths::CONFIG_FILE).exists(),
            has_shares: !crate::shares::states().is_empty(),
            plex: Plex {
                installed: crate::plex::is_provisioned(Path::new(paths::PLEX_MOUNT)),
                answering,
                claimed: answering && crate::plex::is_claimed(),
            },
        }
    }
}

/// The steps, in the order they should be done, with exactly one marked `Next`.
///
/// Order is not arbitrary. Naming the machine before installing Plex means Plex registers
/// itself under the name it will keep; mounting shares before installing Plex means the
/// library is there to be scanned the first time rather than after somebody works out why
/// it is empty. Both are cheap to do in this order and awkward in the other.
#[must_use]
pub fn steps(facts: Facts) -> Vec<Step> {
    let mut steps = vec![
        Step {
            id: "network",
            title: "Reach the appliance",
            detail: if facts.reachable {
                "This page loaded, so the network works. A fixed address can be set in \
                 Settings if this machine should always be at the same one."
                    .to_owned()
            } else {
                "No address yet.".to_owned()
            },
            state: State::Done,
            required: true,
        },
        Step {
            id: "identity",
            title: "Name the machine and set its clock",
            detail: if facts.configured {
                "Done. Both can be changed later in Settings.".to_owned()
            } else {
                "Set a hostname and a timezone in Settings. Do it before installing Plex: \
                 Plex registers itself under the name the machine has at the time, and \
                 timestamps in its library come from this clock."
                    .to_owned()
            },
            state: State::Later,
            required: false,
        },
        Step {
            id: "shares",
            title: "Add the media",
            detail: if facts.has_shares {
                "A share is configured.".to_owned()
            } else {
                "Add the network share your films and programmes are on, under Media \
                 shares. Skip this if the library is on a disk inside the machine."
                    .to_owned()
            },
            state: State::Later,
            required: false,
        },
        Step {
            id: "plex",
            title: "Install Plex",
            detail: if facts.plex.installed {
                "Installed.".to_owned()
            } else {
                "Install Plex Media Server from the card below. It is downloaded from \
                 Plex, its signature is checked against a key built into this image, and \
                 it runs confined (ADR-0010)."
                    .to_owned()
            },
            state: State::Later,
            required: true,
        },
        Step {
            id: "claim-plex",
            title: "Sign Plex in",
            detail: if facts.plex.claimed {
                "Signed in. Plex is owned by a Plex account.".to_owned()
            } else if facts.plex.answering {
                "Plex is answering but is not signed in to any Plex account, so remote \
                 access and everything else it asks plex.tv for will fail. Open it and \
                 sign in to finish."
                    .to_owned()
            } else if facts.plex.installed {
                "Plex is installed and not answering yet. It takes a moment on first start."
                    .to_owned()
            } else {
                "Once Plex is installed, open it and sign in to your Plex account.".to_owned()
            },
            state: State::Later,
            required: true,
        },
    ];

    mark(&mut steps, facts);
    steps
}

/// Fills in `Done` and the single `Next`.
///
/// Exactly one `Next`, and it is the first thing not done — including optional steps, which
/// are suggested in their turn and skipped over once something later is the real blocker.
/// A wizard with two next steps is a list, and a list is what this replaces.
fn mark(steps: &mut [Step], facts: Facts) {
    let done = |id: &str| match id {
        "network" => facts.reachable,
        "identity" => facts.configured,
        "shares" => facts.has_shares,
        "plex" => facts.plex.installed,
        // Not `answering`. That made this step complete itself the moment Plex opened its
        // port, which is a step that cannot fail and therefore checks nothing -- and it
        // hid a real appliance sitting signed out, reporting its setup complete, with
        // plex.tv refusing every request it made.
        "claim-plex" => facts.plex.claimed,
        _ => false,
    };

    let mut next_taken = false;
    for step in steps.iter_mut() {
        if done(step.id) {
            step.state = State::Done;
        } else if next_taken {
            step.state = State::Later;
        } else {
            step.state = State::Next;
            next_taken = true;
        }
    }
}

/// Whether the appliance is set up.
///
/// Only the required steps count. An appliance that called itself unfinished for ever
/// because nobody mounted a NAS would be wrong about its own state, and a banner that never
/// goes away is a banner people stop reading.
#[must_use]
pub fn complete(steps: &[Step]) -> bool {
    steps
        .iter()
        .filter(|step| step.required)
        .all(|step| step.state == State::Done)
}

/// What `GET /api/setup` returns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    /// Whether there is nothing left to do.
    pub complete: bool,
    /// The steps, in order.
    pub steps: Vec<Step>,
}

impl Report {
    /// Reads the machine and works out where it is.
    #[must_use]
    pub fn observe() -> Self {
        let steps = steps(Facts::observe());
        Self {
            complete: complete(&steps),
            steps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine as the installer leaves it: reachable, and nothing else.
    fn fresh() -> Facts {
        Facts {
            reachable: true,
            configured: false,
            has_shares: false,
            plex: Plex {
                installed: false,
                answering: false,
                claimed: false,
            },
        }
    }

    fn finished() -> Facts {
        Facts {
            reachable: true,
            configured: true,
            has_shares: true,
            plex: Plex {
                installed: true,
                answering: true,
                claimed: true,
            },
        }
    }

    fn state_of<'a>(steps: &'a [Step], id: &str) -> &'a Step {
        steps.iter().find(|s| s.id == id).expect("a step")
    }

    #[test]
    fn a_freshly_installed_machine_is_told_one_thing_to_do() {
        // The whole point. Before this, a machine five minutes old showed the same page as
        // one running for a year, with an install button among the diagnostics and nothing
        // saying it was the next thing.
        let steps = steps(fresh());
        let next: Vec<&str> = steps
            .iter()
            .filter(|s| s.state == State::Next)
            .map(|s| s.id)
            .collect();
        assert_eq!(next, vec!["identity"], "exactly one next step");
        assert!(!complete(&steps));
    }

    #[test]
    fn an_appliance_that_is_set_up_says_so_and_stops_asking() {
        // A banner that never goes away is a banner people stop reading, so this has to be
        // reachable and has to be reached by ordinary use rather than by dismissing it.
        let steps = steps(finished());
        assert!(complete(&steps));
        assert!(steps.iter().all(|s| s.state == State::Done));
    }

    #[test]
    fn a_plex_that_answers_but_is_signed_out_leaves_the_setup_unfinished() {
        // The defect this replaces, found on a real appliance: an interrupted stop emptied
        // Preferences.xml, Plex started signed out, and the console reported the setup
        // complete because the step was marked done from `answering`. The machine was
        // getting 401 from plex.tv every five minutes while the page said it was ready.
        let mut facts = finished();
        facts.plex.claimed = false;

        let steps = steps(facts);
        assert!(
            !complete(&steps),
            "a setup whose last required step has not happened is not complete"
        );

        let claim = state_of(&steps, "claim-plex");
        assert_eq!(claim.state, State::Next, "and it is the thing to do next");
        assert!(
            claim.detail.contains("not signed in"),
            "the detail must say what is wrong rather than describe the state it is not \
             in; it read 'Plex is answering' while sitting beside the word done"
        );
    }

    #[test]
    fn answering_is_never_enough_to_finish_the_claim_step() {
        // Stated separately from the case above because it is the property that broke: the
        // step used to be computed from a fact that is true of every running Plex, so it
        // could not report anything but done and checked nothing at all.
        for installed in [false, true] {
            let facts = Facts {
                reachable: true,
                configured: true,
                has_shares: true,
                plex: Plex {
                    installed,
                    answering: true,
                    claimed: false,
                },
            };
            assert_ne!(
                state_of(&steps(facts), "claim-plex").state,
                State::Done,
                "answering says a port is open, not that an account owns the server"
            );
        }
    }

    #[test]
    fn the_optional_steps_do_not_hold_setup_open_for_ever() {
        // A library can be on a disk inside the machine, and a machine can keep the name it
        // was given. Neither absence means the appliance is unfinished.
        let mut facts = finished();
        facts.has_shares = false;
        facts.configured = false;

        let steps = steps(facts);
        assert!(complete(&steps), "shares and naming are suggestions");
        assert!(!state_of(&steps, "shares").required);
        assert!(!state_of(&steps, "identity").required);
        assert!(state_of(&steps, "plex").required);
        assert!(state_of(&steps, "claim-plex").required);
    }

    #[test]
    fn an_optional_step_is_still_offered_in_its_turn() {
        // Suggested when it is the first thing not done, and skipped over once something
        // later is the real blocker -- otherwise "optional" would mean "invisible", and the
        // one moment it is worth doing is before Plex scans a library that is not there.
        assert_eq!(state_of(&steps(fresh()), "identity").state, State::Next);

        let mut named = fresh();
        named.configured = true;
        let named = steps(named);
        assert_eq!(state_of(&named, "shares").state, State::Next);
    }

    #[test]
    fn naming_and_shares_come_before_installing_plex() {
        // Not presentation. Plex registers itself under the name the machine has at the
        // time, and a library that is not mounted when Plex first scans is one somebody has
        // to work out why is empty. Both are cheap in this order and awkward in the other.
        let steps = steps(fresh());
        let at = |id: &str| steps.iter().position(|s| s.id == id).expect("a step");
        assert!(at("identity") < at("plex"));
        assert!(at("shares") < at("plex"));
        assert!(at("plex") < at("claim-plex"));
    }

    #[test]
    fn removing_plex_puts_the_appliance_back_where_it_was() {
        // Nothing is remembered, so nothing can claim setup finished after the thing it was
        // about has gone. That is the property a stored progress flag would not have.
        let mut broken = finished();
        broken.plex.installed = false;
        broken.plex.answering = false;

        let steps = steps(broken);
        assert!(!complete(&steps));
        assert_eq!(state_of(&steps, "plex").state, State::Next);
    }

    #[test]
    fn a_plex_that_is_installed_and_silent_is_a_different_sentence() {
        // The twenty seconds after installing, and also the shape of a real fault. "Install
        // Plex" would be wrong advice in both.
        let mut starting = fresh();
        starting.plex.installed = true;

        let starting = steps(starting);
        let detail = &state_of(&starting, "claim-plex").detail;
        assert!(detail.contains("not answering yet"), "{detail}");
    }

    #[test]
    fn every_step_says_what_to_do_rather_than_only_what_is_wrong() {
        // The rule plexos-gpu enforces with a test, applied to the one screen somebody sees
        // before they know anything about this machine.
        for step in steps(fresh()) {
            assert!(!step.detail.is_empty(), "{} says nothing", step.id);
            assert!(
                step.detail.len() > 30,
                "{} is too terse to act on: {}",
                step.id,
                step.detail
            );
        }
    }
}

//! What Plex is playing *right now*, as MediaLith's own document.
//!
//! Everything else this console reports is a state: which slot booted, whether `/usr`
//! verified, how full `/var` is. This is the one question about a moment that a person
//! actually asks a media appliance — is anything playing, for whom, and is this machine
//! doing it the cheap way or the expensive one.
//!
//! # The browser never talks to Plex
//!
//! ```text
//! browser  --POST /api/plex/sessions (device token)-->  plexosd
//! plexosd  --GET /status/sessions (X-Plex-Token)---->  127.0.0.1:32400
//! ```
//!
//! Plex's API needs the *account* token, which lives in `Preferences.xml` on `/var` and is
//! the one secret on this appliance that cannot be recomputed from anything else. It is
//! read per request through [`crate::plex::account_token`], sent in a header, and never
//! stored, logged, or put in anything this module serialises — asserted by
//! `no_plex_credential_can_reach_the_browser`, which serialises a report built from a
//! response containing a token and greps the result.
//!
//! Nothing here proxies Plex's own document either. Plex's session response carries the
//! library path on disk, the owner's avatar URL, their public IP address and a `guid` that
//! identifies the account's copy of the item; a proxy would hand all of that to anything
//! that could read the page. What leaves this module is the model below and nothing else.
//!
//! # Why reading this needs the device token when reading `/api/status` does not
//!
//! The console deliberately answers `GET` without a credential, because a machine that
//! will not say why it is broken until you authenticate to it has defeated the reason the
//! console exists. That reasoning covers *diagnostics*. It does not cover **what somebody
//! is watching**: a title, a username, a device name and a position in a film are private
//! in a way that a root hash is not, and a household's viewing is readable from the LAN
//! for ever afterwards if this is open. So this route is a `POST` and the method-based
//! gate in [`crate::http::refusal`] applies to it — the same mechanism, and for the same
//! reason, as `POST /api/metrics/processes` and the terminal. ADR-0014 already established
//! that "read-only" and "safe to expose" are different properties.
//!
//! # Verified against a machine, not recalled
//!
//! Every field name below was read off Plex Media Server **1.43.3.10861** on the reference
//! appliance on 2026-08-11, by driving real sessions through it and capturing the answers.
//! Four findings shaped the model, and none of them is what a plausible guess would have
//! produced:
//!
//! * **A Direct Play session says nothing at all.** It has no `TranscodeSession` node and
//!   no `decision` field anywhere. Direct Play is therefore an *absence*, inferred, not
//!   read — see [`Decision`].
//! * **The hardware fields are not populated until the transcoder is actually running.**
//!   A session captured moments after it started reported `transcodeHwRequested: false`
//!   and `transcodeHwDecodingTitle: "Intel ()"` — the same shape a software transcode has.
//!   Seconds later, running, the same session reported `transcodeHwDecoding: "vaapi"`,
//!   `transcodeHwEncoding: "vaapi"`, `transcodeHwFullPipeline: true`. Reporting the first
//!   state as "software" would put an amber warning on a hardware transcode every time one
//!   started. [`Video::hardware`] is an `Option` for exactly this, and "not yet" is `null`.
//! * **A transcoding session's `Media` node describes the *output*.** `videoResolution` was
//!   `720p` and `container` `mpegts` for a 4K MKV. The source codec is in
//!   `TranscodeSession.sourceVideoCodec`; the source *resolution* is nowhere in the session
//!   document except inside a display string (`"4K (HEVC Main)"`), which is composed for
//!   humans and partly localised. So it is fetched from the library instead — see
//!   [`source_from_metadata`] — and is `null` when that fetch fails.
//! * **HDR is structured, in the library.** `colorTrc: "smpte2084"`, `bitDepth: 10`,
//!   `DOVIPresent: true`, `DOVIProfile: 8`. Plex's own display string for that file is
//!   `"4K DoVi/HDR10"`, which is what `hdr_format` reproduces from the fields.
//!
//! # This is observability, and must never become a dependency
//!
//! Plex stalling must not stall the console. Every request here is bounded by
//! [`TIMEOUT`], reads at most [`MAX_BODY`], and turns every failure into a [`State`] with a
//! remedy rather than an error that propagates. Nothing in the boot health gate,
//! `/api/status` or the supervisor calls into this module, and nothing here can make a
//! boot fail.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Plex's own endpoint for what is playing. Verified: 401 without a token, and
/// `{"MediaContainer":{"size":0}}` when nothing is playing.
pub const SESSIONS_PATH: &str = "/status/sessions";

/// How long any single request to Plex may take.
///
/// Short, and it is a ceiling on the whole exchange rather than a target: this talks to a
/// process on the same machine over loopback, where the observed answer took milliseconds.
/// Anything slower is Plex being unwell, and the console has to keep answering while it is.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// The most of Plex's answer that will ever be read.
///
/// The captured responses were 8.5 KB for one session and 14.5 KB for another — Plex sends
/// the whole metadata record, cast list included. 512 KiB is far above any plausible
/// answer and far below anything that could hurt this daemon, which is the point: a bound
/// exists so that a server that decides to send megabytes cannot make the console the
/// thing that fails.
pub const MAX_BODY: u64 = 512 * 1024;

/// What MediaLith can say about live playback, and why when it cannot.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Report {
    /// Whether [`Report::sessions`] is an answer rather than an explanation.
    ///
    /// Its own field so the page never has to decide by matching on a string.
    pub available: bool,
    /// Which of the states below this machine is in.
    pub state: State,
    /// What that means, and what to do about it. Never empty, for any state.
    pub detail: String,
    /// How many sessions are active. Zero whenever `available` is false.
    pub active: usize,
    /// The sessions themselves, most-recently-started first as Plex returns them.
    pub sessions: Vec<Session>,
}

impl Report {
    /// A report that explains itself instead of answering.
    fn of(state: State) -> Self {
        Self {
            available: false,
            state,
            detail: state.detail().to_owned(),
            active: 0,
            sessions: Vec::new(),
        }
    }
}

/// Every distinguishable answer to "what is Plex playing".
///
/// These are separate states because they take **different remedies**, which is the test
/// for whether a distinction is worth carrying: an unclaimed server needs signing in, a
/// missing token needs signing in again, an unreachable one needs Plex restarted, and an
/// unreadable answer needs somebody to look at MediaLith rather than at Plex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Plex is not installed on this appliance.
    NotProvisioned,
    /// Plex is installed but nothing is listening on loopback.
    NotRunning,
    /// Plex is running and no Plex account owns it, so there is no token to ask with.
    NotClaimed,
    /// Plex is claimed, but its preferences carry no usable account token.
    NoToken,
    /// The request to Plex could not be completed.
    Unreachable,
    /// Plex refused the token this appliance holds.
    Refused,
    /// Plex answered with something this parser does not recognise.
    Unreadable,
    /// Plex is answering and nothing is playing.
    Idle,
    /// Plex is answering and something is playing.
    Playing,
}

impl State {
    /// What the state means and what to do about it.
    ///
    /// Every one of these names a remedy, including the two that are not faults, because a
    /// message that reports a condition and stops has reproduced the problem this project
    /// exists to fix. `every_state_names_a_remedy` holds it to that.
    #[must_use]
    pub fn detail(self) -> &'static str {
        match self {
            Self::NotProvisioned => {
                "Plex is not installed on this appliance, so there is nothing to be playing. \
                 Remedy: install it from the Plex view."
            }
            Self::NotRunning => {
                "Plex is installed but is not answering on 127.0.0.1:32400. Remedy: the Plex \
                 view reports whether it is running and restarts it; a server that keeps \
                 dying leaves its own output there."
            }
            Self::NotClaimed => {
                "Plex is running but no Plex account owns it yet, and its API cannot be \
                 asked without one. Remedy: open Plex and sign in; live activity appears on \
                 its own afterwards."
            }
            Self::NoToken => {
                "Plex reports itself as claimed, but its preferences carry no usable account \
                 token — which is what a server that has been signed out looks like. \
                 Remedy: open Plex and sign in again."
            }
            Self::Unreachable => {
                "Plex did not answer the request for what it is playing within two seconds. \
                 Remedy: this is Plex being busy or unwell rather than the appliance; the \
                 Plex view reports whether it is running, and everything else on this page \
                 keeps working meanwhile."
            }
            Self::Refused => {
                "Plex refused this appliance's account token, which means it was signed out \
                 or the token was replaced. Remedy: open Plex and sign in again."
            }
            Self::Unreadable => {
                "Plex answered with something this version of MediaLith does not recognise, \
                 which usually means Plex changed its API. Remedy: report it — nothing on \
                 the appliance is wrong, and every other view is unaffected."
            }
            Self::Idle => {
                "Plex is ready and nothing is playing. Remedy: none needed; start something \
                 on any client and it appears here within a few seconds."
            }
            Self::Playing => {
                "Plex is playing. Remedy: none needed — the Plex view lists every stream in \
                 full."
            }
        }
    }
}

/// One thing somebody is watching.
///
/// Every field but the identifier is optional, and that is deliberate: a session missing a
/// title is still worth showing for its user, its player and its transcode decision. A
/// model that required any of them would drop whole sessions over one absent field, which
/// is the failure this shape exists to prevent.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct Session {
    /// Plex's `sessionKey` — stable for the life of the session, and what the page keys its
    /// redraws on.
    pub id: Option<String>,
    /// The library item, so the page can tell two sessions of the same film apart and so a
    /// source lookup has something to ask about. Not a secret: it is an integer that means
    /// nothing off this machine.
    pub rating_key: Option<String>,
    /// `movie`, `episode`, `track`…
    pub kind: Option<String>,
    /// What is playing.
    pub title: Option<String>,
    /// The series, for an episode. `grandparentTitle`.
    pub series: Option<String>,
    /// `S02E05`, composed from `parentIndex` and `index` only when both are present.
    pub episode: Option<String>,
    /// The Plex account watching.
    pub user: Option<String>,
    /// What they are watching on — the device's given name.
    pub player: Option<String>,
    /// The platform that device runs, `tvOS`, `webOS`, `Chrome`.
    pub platform: Option<String>,
    /// The Plex application, `Plex for Apple TV`.
    pub product: Option<String>,
    /// `playing`, `paused`, `buffering`.
    pub state: Option<String>,
    /// Whether the player is on this LAN.
    pub local: Option<bool>,
    /// How far in, from `viewOffset`.
    pub position_ms: Option<u64>,
    /// How long the item is.
    pub duration_ms: Option<u64>,
    /// What is happening to the video.
    pub video: Video,
    /// What is happening to the audio.
    pub audio: Audio,
    /// What the session as a whole is: the one fact the badge is drawn from.
    pub decision: Decision,
    /// The bit rate of the file being played, when it is known.
    pub source_bitrate_kbps: Option<u64>,
    /// The bit rate being sent to the player.
    pub stream_bitrate_kbps: Option<u64>,
    /// How far through the transcode is, and whether it is keeping up. Absent unless
    /// something is being transcoded.
    pub transcode: Option<Transcode>,
}

/// What is happening to a session's video.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct Video {
    /// Passed through, remuxed, or re-encoded.
    pub decision: Decision,
    /// The codec of the file.
    pub source_codec: Option<String>,
    /// `4K`, `1080p`, `720p` — the file's, not the stream's.
    pub source_resolution: Option<String>,
    /// `HDR10`, `HLG`, `Dolby Vision`, `Dolby Vision / HDR10`. `None` means SDR **or** not
    /// established, which is why the page says nothing at all rather than "SDR".
    pub source_hdr: Option<String>,
    /// The codec being sent.
    pub target_codec: Option<String>,
    /// The resolution being sent.
    pub target_resolution: Option<String>,
    /// Whether the GPU is doing the work.
    ///
    /// Three-valued on purpose. `Some(true)` is Plex naming a hardware decoder or encoder;
    /// `Some(false)` is a transcoder that is demonstrably running without one; `None` is
    /// "not established yet", which is what every hardware transcode looks like for its
    /// first second or two. Collapsing `None` into `false` would put a software-transcode
    /// warning on the appliance's best case every time somebody pressed play.
    ///
    /// **This answers a different question from `/api/gpu`.** That one says whether this
    /// machine *can* transcode in hardware; this says whether this *stream* is. Both are
    /// worth knowing and they disagree in the interesting cases.
    pub hardware: Option<bool>,
    /// Plex's own words for it — `Intel (VA API)` — kept because it names the vendor and
    /// the API, which is what somebody comparing this against `/api/gpu` needs.
    pub hardware_detail: Option<String>,
    /// Whether Plex reports decode *and* encode on the GPU rather than one of the two.
    pub full_pipeline: Option<bool>,
}

/// What is happening to a session's audio.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct Audio {
    /// Passed through or re-encoded.
    pub decision: Decision,
    /// The codec of the file: `truehd`, `eac3`.
    pub source_codec: Option<String>,
    /// How many channels the file has.
    pub source_channels: Option<u64>,
    /// The codec being sent.
    pub target_codec: Option<String>,
    /// How many channels are being sent — the number that turns 7.1 into stereo.
    pub target_channels: Option<u64>,
}

/// How a stream is reaching its player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// The file is being sent as it is. **Inferred from the absence of a transcode
    /// session**, because that is all Plex says: a direct-play session captured from the
    /// appliance had no `TranscodeSession` node and no `decision` field anywhere in it.
    DirectPlay,
    /// The stream is being passed through into a different container. Plex calls this
    /// `copy`, and it is what a video decision of `copy` beside an audio decision of
    /// `transcode` means.
    DirectStream,
    /// The stream is being re-encoded.
    Transcode,
    /// Plex said something else, or nothing. Distinct from Direct Play so that a future
    /// version of Plex inventing a fourth answer is visible rather than silently reported
    /// as the best case.
    #[default]
    Unknown,
}

impl Decision {
    /// Plex's own vocabulary, as observed: `transcode`, `copy`, `directplay`.
    fn read(word: Option<&str>) -> Self {
        match word {
            Some("transcode") => Self::Transcode,
            Some("copy") => Self::DirectStream,
            Some("directplay") => Self::DirectPlay,
            _ => Self::Unknown,
        }
    }
}

/// How a transcode is coping.
///
/// `speed` is the one worth watching: below 1.0 means the transcoder is producing video
/// more slowly than it is being watched, which is the number that becomes buffering. It is
/// also legitimately 0.0 for a *throttled* transcode that has run far enough ahead, which
/// is why `throttled` is beside it — reporting 0.0 as trouble would call the healthy case a
/// fault, and this was observed on the appliance with `throttled: true, speed: 0.0`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Transcode {
    /// Per cent of the item transcoded so far.
    pub progress: Option<f64>,
    /// Multiple of real time. Below 1.0 while not throttled is trouble.
    pub speed: Option<f64>,
    /// Whether Plex has paused the transcoder because it is far enough ahead.
    pub throttled: Option<bool>,
    /// Whether Plex has flagged the transcode as failed.
    pub error: Option<bool>,
}

/// The characteristics of the file itself, which a transcoding session does not carry.
///
/// Fetched from the library and cached, because it cannot change without the file changing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Source {
    /// `hevc`, `h264`.
    pub codec: Option<String>,
    /// `4K`, `1080p`.
    pub resolution: Option<String>,
    /// `HDR10`, `Dolby Vision / HDR10`.
    pub hdr: Option<String>,
    /// The file's overall bit rate.
    pub bitrate_kbps: Option<u64>,
    /// `truehd`, `eac3`.
    pub audio_codec: Option<String>,
    /// The file's channel count, which is what makes "7.1 → 2.0" sayable.
    pub audio_channels: Option<u64>,
}

/// Why a request to Plex did not produce an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trouble {
    /// Nothing is listening. Connection refused is Plex being down, not a network fault.
    NotRunning,
    /// Connected, and then did not complete.
    Unreachable,
    /// Plex answered 401.
    Refused,
    /// Plex answered something else, or something unparseable.
    Unreadable,
}

impl From<Trouble> for State {
    fn from(trouble: Trouble) -> Self {
        match trouble {
            Trouble::NotRunning => Self::NotRunning,
            Trouble::Unreachable => Self::Unreachable,
            Trouble::Refused => Self::Refused,
            Trouble::Unreadable => Self::Unreadable,
        }
    }
}

/// Asks Plex one question, with the appliance's credential, and bounds every part of it.
///
/// Hand-written for the same reason [`crate::plex::is_answering`] is: one question, one
/// server, on the same machine, and it has to work when nothing else does. HTTP/1.0 with
/// `Connection: close` so there is no keep-alive state to get wrong.
///
/// The token goes in a **header**. Plex accepts it as a query parameter too, and that would
/// put a credential into any log or proxy on the path — there is no proxy on loopback
/// today, which is exactly the sort of assumption that stops being true quietly.
fn ask(path: &str, token: &str) -> Result<String, Trouble> {
    let address = crate::plex::LOOPBACK_ADDRESS
        .parse()
        .map_err(|_| Trouble::Unreachable)?;

    let mut stream = std::net::TcpStream::connect_timeout(&address, TIMEOUT).map_err(|e| {
        if e.kind() == std::io::ErrorKind::ConnectionRefused {
            Trouble::NotRunning
        } else {
            Trouble::Unreachable
        }
    })?;
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));

    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nAccept: application/json\r\n\
         X-Plex-Token: {token}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|_| Trouble::Unreachable)?;

    let mut raw = Vec::new();
    stream
        .take(MAX_BODY)
        .read_to_end(&mut raw)
        .map_err(|_| Trouble::Unreachable)?;

    // Lossy is right here and wrong for a signed document: this is a display path, and a
    // film title in a script this build cannot represent must not lose the session it is
    // attached to. `plexos-update` fetches bytes for the opposite reason.
    let answer = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = answer.split_once("\r\n\r\n").ok_or(Trouble::Unreadable)?;

    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or(Trouble::Unreadable)?;

    match status {
        "200" => Ok(body.to_owned()),
        "401" | "403" => Err(Trouble::Refused),
        _ => Err(Trouble::Unreadable),
    }
}

/// What Plex is playing, or why that cannot be said.
///
/// The order of the checks is the order of the remedies: there is no point reporting an
/// unreachable API on a machine where Plex is not installed. Exactly one request to Plex
/// happens in the ordinary case; a source lookup adds one per *item never seen before*,
/// not one per poll.
#[must_use]
pub fn observe(mount: &Path) -> Report {
    if !crate::plex::is_provisioned(mount) {
        return Report::of(State::NotProvisioned);
    }

    // Read, use, drop. The token exists in this process for the length of one request and
    // is never held anywhere that outlives it.
    let Ok(preferences) = std::fs::read_to_string(crate::plex::preferences_file()) else {
        // No preferences file at all, on a provisioned machine: Plex has never started, or
        // is starting now. "Not running" is the honest reading and names the right remedy.
        return Report::of(State::NotRunning);
    };
    let Some(token) = crate::plex::account_token(&preferences) else {
        return Report::of(if crate::plex::is_claimed() {
            State::NoToken
        } else {
            State::NotClaimed
        });
    };

    let body = match ask(SESSIONS_PATH, token) {
        Ok(body) => body,
        Err(trouble) => return Report::of(trouble.into()),
    };

    let Ok(mut sessions) = parse(&body) else {
        return Report::of(State::Unreadable);
    };

    enrich(&mut sessions, &mut |key| library_source(key, token));

    Report {
        available: true,
        state: if sessions.is_empty() {
            State::Idle
        } else {
            State::Playing
        },
        detail: if sessions.is_empty() {
            State::Idle.detail().to_owned()
        } else {
            State::Playing.detail().to_owned()
        },
        active: sessions.len(),
        sessions,
    }
}

/// Turns Plex's answer into MediaLith's model.
///
/// Separated from every socket so it can be run against captured responses, which is the
/// only way the shapes in this file were established in the first place.
///
/// # Errors
/// Only when the document is not a `MediaContainer` at all. A *session* that cannot be
/// understood does not fail the parse and does not disappear: every field is optional, so
/// what survives is reported and the rest is `null`.
pub fn parse(body: &str) -> Result<Vec<Session>, String> {
    let document: Value = serde_json::from_str(body).map_err(|e| format!("not JSON: {e}"))?;
    let container = document
        .get("MediaContainer")
        .ok_or_else(|| "no MediaContainer".to_owned())?;

    // No `Metadata` key at all is how Plex says "nothing is playing" — `{"size":0}` and
    // nothing else. That is an empty list, not a failure.
    let Some(entries) = container.get("Metadata").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    Ok(entries.iter().map(session).collect())
}

/// One entry of `MediaContainer.Metadata`.
fn session(entry: &Value) -> Session {
    let transcode_node = entry.get("TranscodeSession");
    let media = entry
        .get("Media")
        .and_then(Value::as_array)
        .and_then(|list| list.first());
    let part = media
        .and_then(|m| m.get("Part"))
        .and_then(Value::as_array)
        .and_then(|list| list.first());

    let streams = part
        .and_then(|p| p.get("Stream"))
        .and_then(Value::as_array)
        .map_or_else(Vec::new, Clone::clone);
    let video_stream = streams.iter().find(|s| number(s, "streamType") == Some(1));
    let audio_stream = streams.iter().find(|s| number(s, "streamType") == Some(2));

    let video = video(transcode_node, media, video_stream, part);
    let audio = audio(transcode_node, media, audio_stream);

    Session {
        id: text(entry, "sessionKey"),
        rating_key: text(entry, "ratingKey"),
        kind: text(entry, "type"),
        title: text(entry, "title"),
        series: text(entry, "grandparentTitle"),
        episode: episode(entry),
        user: entry.get("User").and_then(|u| text(u, "title")),
        player: entry.get("Player").and_then(|p| text(p, "title")),
        platform: entry.get("Player").and_then(|p| text(p, "platform")),
        product: entry.get("Player").and_then(|p| text(p, "product")),
        state: entry.get("Player").and_then(|p| text(p, "state")),
        local: entry
            .get("Player")
            .and_then(|p| p.get("local"))
            .map_or_else(
                || None,
                |v| {
                    v.as_bool()
                        .or_else(|| v.as_str().map(|s| s == "1" || s == "true"))
                },
            ),
        position_ms: number(entry, "viewOffset"),
        duration_ms: number(entry, "duration"),
        decision: overall(transcode_node, video.decision, audio.decision),
        // The bit rate of the file is only in the session when the file is what is being
        // sent; a transcoding session's Media node is the output. `enrich` fills it in
        // from the library for the rest.
        source_bitrate_kbps: if transcode_node.is_none() {
            media.and_then(|m| number(m, "bitrate"))
        } else {
            None
        },
        stream_bitrate_kbps: entry
            .get("Session")
            .and_then(|s| number(s, "bandwidth"))
            .or_else(|| {
                transcode_node
                    .is_some()
                    .then(|| media.and_then(|m| number(m, "bitrate")))
                    .flatten()
            }),
        transcode: transcode_node.map(|node| Transcode {
            progress: node.get("progress").and_then(Value::as_f64),
            speed: node.get("speed").and_then(Value::as_f64),
            throttled: node.get("throttled").and_then(Value::as_bool),
            error: node.get("error").and_then(Value::as_bool),
        }),
        video,
        audio,
    }
}

/// The video path, from whichever of the three places actually carries each fact.
fn video(
    transcode: Option<&Value>,
    media: Option<&Value>,
    stream: Option<&Value>,
    part: Option<&Value>,
) -> Video {
    let decision = stream_decision(transcode, "videoDecision", stream, part);

    // With no transcode session the Media node *is* the file, so source and target are the
    // same thing and saying it twice would invite the page to draw an arrow between two
    // identical halves.
    let direct = transcode.is_none();

    Video {
        decision,
        source_codec: transcode
            .and_then(|t| text(t, "sourceVideoCodec"))
            .or_else(|| {
                direct
                    .then(|| media.and_then(|m| text(m, "videoCodec")))
                    .flatten()
            }),
        source_resolution: direct
            .then(|| media.and_then(|m| resolution(&text(m, "videoResolution")?)))
            .flatten(),
        // Only ever established from the library: a transcoding session reports the
        // *output* transfer characteristics, so reading `colorTrc` here would report a
        // tone-mapped stream as an SDR file.
        source_hdr: direct.then(|| stream.and_then(hdr_format)).flatten(),
        target_codec: transcode
            .and_then(|t| text(t, "videoCodec"))
            .or_else(|| media.and_then(|m| text(m, "videoCodec"))),
        target_resolution: media
            .and_then(|m| text(m, "videoResolution"))
            .as_deref()
            .and_then(resolution),
        // Only a *video* transcode has a hardware verdict. Found on the appliance, on a real
        // Direct Stream — video copied, audio re-encoded — where the transcode session had
        // progressed and named no decoder, so the rule below answered `false` about a
        // picture nothing was decoding. The page does not draw it for a direct stream, so
        // nothing showed; the field was wrong anyway, and a field that is wrong only where
        // nothing reads it is one that becomes wrong somewhere that does.
        hardware: (decision == Decision::Transcode)
            .then(|| hardware(transcode))
            .flatten(),
        hardware_detail: (decision == Decision::Transcode)
            .then(|| transcode.and_then(hardware_detail))
            .flatten(),
        full_pipeline: (decision == Decision::Transcode)
            .then(|| {
                transcode.and_then(|t| t.get("transcodeHwFullPipeline").and_then(Value::as_bool))
            })
            .flatten(),
    }
}

/// The audio path.
fn audio(transcode: Option<&Value>, media: Option<&Value>, stream: Option<&Value>) -> Audio {
    let decision = stream_decision(transcode, "audioDecision", stream, None);
    let direct = transcode.is_none();

    Audio {
        decision,
        source_codec: transcode
            .and_then(|t| text(t, "sourceAudioCodec"))
            .or_else(|| {
                direct
                    .then(|| media.and_then(|m| text(m, "audioCodec")))
                    .flatten()
            }),
        source_channels: direct
            .then(|| media.and_then(|m| number(m, "audioChannels")))
            .flatten(),
        target_codec: transcode
            .and_then(|t| text(t, "audioCodec"))
            .or_else(|| media.and_then(|m| text(m, "audioCodec"))),
        target_channels: transcode
            .and_then(|t| number(t, "audioChannels"))
            .or_else(|| media.and_then(|m| number(m, "audioChannels")))
            .or_else(|| stream.and_then(|s| number(s, "channels"))),
    }
}

/// What is being done to one stream.
///
/// The transcode session is asked first because it is unambiguous. The per-stream
/// `decision` is the fallback, and the absence of both is Direct Play — which is a real
/// answer here rather than a default, because a session with no transcode session is one
/// Plex is not transcoding, and that was verified on the machine.
fn stream_decision(
    transcode: Option<&Value>,
    key: &str,
    stream: Option<&Value>,
    part: Option<&Value>,
) -> Decision {
    if let Some(node) = transcode {
        let named = Decision::read(node.get(key).and_then(Value::as_str));
        if named != Decision::Unknown {
            return named;
        }
    }

    let per_stream = Decision::read(
        stream
            .and_then(|s| s.get("decision"))
            .and_then(Value::as_str),
    );
    if per_stream != Decision::Unknown {
        return per_stream;
    }

    let per_part = Decision::read(part.and_then(|p| p.get("decision")).and_then(Value::as_str));
    match per_part {
        Decision::Unknown if transcode.is_none() => Decision::DirectPlay,
        other => other,
    }
}

/// What the session is, as one word for the badge.
///
/// Video decides it: a stream whose picture is being re-encoded is a transcode whatever is
/// happening to its sound. Audio only gets a say when the video is being passed through,
/// which is exactly Plex's own "Direct Stream" — and the distinction matters to somebody
/// reading this, because one of the two is nearly free and the other is not.
fn overall(transcode: Option<&Value>, video: Decision, audio: Decision) -> Decision {
    if transcode.is_none() {
        return Decision::DirectPlay;
    }
    match (video, audio) {
        (Decision::Transcode, _) => Decision::Transcode,
        (Decision::DirectStream | Decision::DirectPlay, Decision::Transcode) => {
            Decision::DirectStream
        }
        (Decision::DirectStream, _) => Decision::DirectStream,
        (Decision::DirectPlay, _) => Decision::DirectPlay,
        (Decision::Unknown, _) => Decision::Unknown,
    }
}

/// Whether the GPU is doing this transcode, or `None` while that is not yet established.
///
/// The rule comes from a capture rather than from documentation. A session that has not
/// begun transcoding reports `transcodeHwRequested: false` with empty hardware titles —
/// indistinguishable from software — and the *same session*, seconds later, reported
/// `transcodeHwDecoding: "vaapi"`. So the presence of a named decoder or encoder is the
/// only positive evidence, and "no evidence" means `false` only once the transcoder has
/// demonstrably produced something.
fn hardware(transcode: Option<&Value>) -> Option<bool> {
    let node = transcode?;
    let named = |key: &str| {
        node.get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    };
    if named("transcodeHwDecoding") || named("transcodeHwEncoding") {
        return Some(true);
    }

    // Throttled transcodes legitimately report a speed of 0.0, so progress is what says
    // work has actually happened.
    let started = node
        .get("progress")
        .and_then(Value::as_f64)
        .is_some_and(|p| p > 0.0)
        || node
            .get("speed")
            .and_then(Value::as_f64)
            .is_some_and(|s| s > 0.0);

    started.then_some(false)
}

/// Plex's own name for the hardware in use, when it is using any.
///
/// `transcodeHwDecodingTitle` reads `Intel ()` — with the API missing from the parentheses
/// — before the transcoder starts, so the title is only worth repeating once there is a
/// decoder or encoder to attach it to.
fn hardware_detail(node: &Value) -> Option<String> {
    let used = |key: &str| {
        node.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    used("transcodeHwDecoding")
        .or_else(|| used("transcodeHwEncoding"))
        .and_then(|_| {
            text(node, "transcodeHwDecodingTitle")
                .or_else(|| text(node, "transcodeHwEncodingTitle"))
        })
}

/// `S02E05`, and only when both halves are there.
///
/// Half of it would be worse than none: `S02` beside a title says nothing a person can use,
/// and `E05` without a season is ambiguous on any series with more than one.
fn episode(entry: &Value) -> Option<String> {
    let season = number(entry, "parentIndex")?;
    let number = number(entry, "index")?;
    Some(format!("S{season:02}E{number:02}"))
}

/// Plex's resolution words, in the form people say them.
///
/// Plex writes `4k`, `1080`, `720`, `sd` in the library and `720p` in a session — the same
/// field in two spellings, which is why this normalises rather than passes through.
fn resolution(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bare = trimmed.strip_suffix('p').unwrap_or(trimmed);
    Some(match bare.to_ascii_lowercase().as_str() {
        "4k" => "4K".to_owned(),
        "8k" => "8K".to_owned(),
        "sd" => "SD".to_owned(),
        other if other.chars().all(|c| c.is_ascii_digit()) => format!("{other}p"),
        _ => trimmed.to_owned(),
    })
}

/// The high-dynamic-range format of a source video stream, from structured fields only.
///
/// Verified against the library on the appliance: an HDR file carries
/// `colorTrc: "smpte2084"` with `bitDepth: 10`, and a Dolby Vision one adds
/// `DOVIPresent: true`. Plex's own display string for the file that has both is
/// `4K DoVi/HDR10`, which is what this reproduces — from the fields rather than by reading
/// that string, which is composed for people and partly localised.
///
/// `None` means SDR *or* nothing established, and the page says nothing rather than "SDR"
/// for that reason.
fn hdr_format(stream: &Value) -> Option<String> {
    let trc = stream.get("colorTrc").and_then(Value::as_str);
    let dolby = stream
        .get("DOVIPresent")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    match (dolby, trc) {
        (true, Some("smpte2084")) => Some("Dolby Vision / HDR10".to_owned()),
        (true, _) => Some("Dolby Vision".to_owned()),
        (false, Some("smpte2084")) => Some("HDR10".to_owned()),
        (false, Some("arib-std-b67")) => Some("HLG".to_owned()),
        (false, _) => None,
    }
}

/// A string field, whether Plex wrote it as a string or a number.
///
/// Plex is inconsistent about this in a way that matters: `ratingKey` is `"118"` in a
/// session and `118` in the library, and the same document has `"id":"289"` beside
/// `"id":289`. A reader that insisted on one of the two would work until it was pointed at
/// the other endpoint.
fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A whole number, from either spelling, and never negative.
///
/// `size: -1` appears on a transcode session that has not sized itself yet, and a
/// duration that arrives as a float is truncated rather than dropped.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the range check is what makes the one cast total: the value is finite, not \
              negative, and below 2^53, where every integer is exactly representable as an \
              f64. Truncating is deliberate -- a duration that arrives as 1.9 is a duration, \
              and dropping it would lose the field rather than a fraction of a millisecond."
)]
fn number(value: &Value, key: &str) -> Option<u64> {
    /// Above this an `f64` can no longer represent every integer, so a cast would invent
    /// precision. Nothing Plex reports comes close: it is 285,000 years in milliseconds.
    const EXACT: f64 = 9_007_199_254_740_992.0;

    match value.get(key)? {
        Value::Number(n) => n.as_u64().or_else(|| {
            let float = n.as_f64()?;
            (float.is_finite() && (0.0..EXACT).contains(&float)).then_some(float as u64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Fills in what a transcoding session cannot say about the file it is playing.
///
/// Only transcodes are looked up: a direct-play session's `Media` node already *is* the
/// file, and asking the library about it again would be a request per poll for an answer
/// already in hand.
///
/// The lookup is a parameter so this can be tested without a Plex, which is the same
/// boundary every other module here draws.
pub fn enrich(sessions: &mut [Session], lookup: &mut dyn FnMut(&str) -> Option<Source>) {
    for session in sessions {
        if session.decision == Decision::DirectPlay {
            continue;
        }
        let Some(key) = session.rating_key.clone() else {
            continue;
        };
        let Some(source) = lookup(&key) else {
            continue;
        };

        // `or` and not assignment: anything the session itself said is better evidence than
        // a library record that may describe a file replaced since playback began.
        session.video.source_codec = session.video.source_codec.take().or(source.codec);
        session.video.source_resolution =
            session.video.source_resolution.take().or(source.resolution);
        session.video.source_hdr = session.video.source_hdr.take().or(source.hdr);
        session.audio.source_codec = session.audio.source_codec.take().or(source.audio_codec);
        session.audio.source_channels = session.audio.source_channels.or(source.audio_channels);
        session.source_bitrate_kbps = session.source_bitrate_kbps.or(source.bitrate_kbps);
    }
}

/// What the library says about one item.
///
/// # Errors
/// Returns `None` for anything unrecognised, because this is an enrichment: a session that
/// cannot be enriched is still a session worth showing.
#[must_use]
pub fn source_from_metadata(body: &str) -> Option<Source> {
    let document: Value = serde_json::from_str(body).ok()?;
    let entry = document
        .get("MediaContainer")?
        .get("Metadata")?
        .as_array()?
        .first()?;
    let media = entry.get("Media")?.as_array()?.first()?;
    let video_stream = media
        .get("Part")
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(|part| part.get("Stream"))
        .and_then(Value::as_array)
        .and_then(|streams| {
            streams
                .iter()
                .find(|s| number(s, "streamType") == Some(1))
                .cloned()
        });

    Some(Source {
        codec: text(media, "videoCodec"),
        resolution: text(media, "videoResolution")
            .as_deref()
            .and_then(resolution),
        hdr: video_stream.as_ref().and_then(hdr_format),
        bitrate_kbps: number(media, "bitrate"),
        audio_codec: text(media, "audioCodec"),
        audio_channels: number(media, "audioChannels"),
    })
}

/// A report of each kind, for looking at the page in a state the appliance is not in.
///
/// Two callers, and neither is decoration. `examples/plex-activity-fixture.rs` prints these
/// for `tools/preview-console.py`, which is how a card gets *rendered* before it ships —
/// four CSS faults in one afternoon during the console redesign were invisible to every test
/// in this repository and obvious in a screenshot. And the console's own test reads the
/// serialised form to check that every field the page reaches for is one the server sends.
///
/// Built here rather than hand-written as JSON on purpose. A fixture written by hand is a
/// fixture that agrees with whoever wrote it: it would keep the old spelling of a renamed
/// field and show a preview with a line missing, which reads as a broken card rather than a
/// stale file.
#[must_use]
pub fn sample(kind: &str) -> Report {
    let full = |sessions: Vec<Session>| Report {
        available: true,
        state: State::Playing,
        detail: State::Playing.detail().to_owned(),
        active: sessions.len(),
        sessions,
    };

    match kind {
        "hardware" => full(vec![sample_transcode(Some(true))]),
        "software" => full(vec![sample_transcode(Some(false))]),
        "starting" => full(vec![sample_transcode(None)]),
        "direct" => full(vec![sample_direct()]),
        "three" => full(vec![
            sample_transcode(Some(true)),
            sample_direct(),
            sample_transcode(Some(false)),
        ]),
        // A session Plex has never actually produced: everything optional missing. The
        // card has to come out shorter rather than full of the word "unknown".
        "sparse" => full(vec![Session {
            id: Some("9".to_owned()),
            decision: Decision::DirectPlay,
            ..Session::default()
        }]),
        "idle" => Report {
            available: true,
            state: State::Idle,
            detail: State::Idle.detail().to_owned(),
            active: 0,
            sessions: Vec::new(),
        },
        "not-claimed" => Report::of(State::NotClaimed),
        "not-running" => Report::of(State::NotRunning),
        _ => Report::of(State::NotProvisioned),
    }
}

/// The fields every sample session shares: somebody, watching something, part way through.
fn sample_watcher(title: &str, user: &str, player: &str) -> Session {
    Session {
        id: Some("1".to_owned()),
        rating_key: Some("118".to_owned()),
        kind: Some("movie".to_owned()),
        title: Some(title.to_owned()),
        user: Some(user.to_owned()),
        player: Some(player.to_owned()),
        platform: Some("tvOS".to_owned()),
        product: Some("Plex for Apple TV".to_owned()),
        state: Some("playing".to_owned()),
        local: Some(true),
        position_ms: Some(5_538_000),
        duration_ms: Some(10_143_000),
        ..Session::default()
    }
}

/// 4K HDR to 1080p, with the hardware verdict as the caller wants it.
///
/// `None` is the state every hardware transcode passes through for its first second or
/// two, and is the one worth looking at before it ships: it must not draw as a warning.
fn sample_transcode(hardware: Option<bool>) -> Session {
    Session {
        decision: Decision::Transcode,
        source_bitrate_kbps: Some(24_399),
        stream_bitrate_kbps: Some(2798),
        // Throttled with a speed of 0.0, which is what the appliance actually reported on a
        // stream that was playing perfectly.
        transcode: Some(Transcode {
            progress: Some(1.3),
            speed: Some(0.0),
            throttled: Some(true),
            error: Some(false),
        }),
        video: Video {
            decision: Decision::Transcode,
            source_codec: Some("hevc".to_owned()),
            source_resolution: Some("4K".to_owned()),
            source_hdr: Some("HDR10".to_owned()),
            target_codec: Some("h264".to_owned()),
            target_resolution: Some("1080p".to_owned()),
            hardware,
            hardware_detail: hardware
                .unwrap_or(false)
                .then(|| "Intel (VA API)".to_owned()),
            full_pipeline: hardware,
        },
        audio: Audio {
            decision: Decision::Transcode,
            source_codec: Some("truehd".to_owned()),
            source_channels: Some(8),
            target_codec: Some("aac".to_owned()),
            target_channels: Some(2),
        },
        ..sample_watcher("Test Feature", "Sebastian", "Living Room TV")
    }
}

/// An episode, paused, playing exactly as it sits on the disk.
fn sample_direct() -> Session {
    Session {
        decision: Decision::DirectPlay,
        source_bitrate_kbps: Some(2085),
        video: Video {
            decision: Decision::DirectPlay,
            source_codec: Some("hevc".to_owned()),
            source_resolution: Some("1080p".to_owned()),
            target_codec: Some("hevc".to_owned()),
            target_resolution: Some("1080p".to_owned()),
            ..Video::default()
        },
        audio: Audio {
            decision: Decision::DirectPlay,
            source_codec: Some("ac3".to_owned()),
            source_channels: Some(6),
            target_codec: Some("ac3".to_owned()),
            target_channels: Some(6),
        },
        state: Some("paused".to_owned()),
        series: Some("Test Series".to_owned()),
        episode: Some("S02E05".to_owned()),
        ..sample_watcher("The One With The Long Title", "Ada", "Bedroom iPad")
    }
}

/// How long a remembered source stays trusted.
///
/// A library item's codec and resolution cannot change without the file changing, so this
/// could be for ever — except that replacing a file with a better copy is a thing people do
/// to a media library, and a cache with no expiry would report the old one until the
/// appliance was restarted.
const SOURCE_TTL: Duration = Duration::from_secs(30 * 60);

/// How many items are remembered at once.
///
/// A handful of simultaneous streams is what this appliance is for; the cap exists so that
/// a long-running daemon cannot accumulate a library's worth of records.
const SOURCE_CACHE: usize = 32;

/// Remembered sources, oldest first.
///
/// A `Vec` rather than a map because it holds tens of entries at most, and `Vec::new` is a
/// const constructor — so this needs no lazy initialisation and no dependency.
static SOURCES: Mutex<Vec<(String, Instant, Source)>> = Mutex::new(Vec::new());

/// The file behind a session, fetched once and remembered.
fn library_source(rating_key: &str, token: &str) -> Option<Source> {
    // A rating key is an integer. Checking that is not defensive tidiness: it is
    // interpolated into a request line, and anything carrying a space or a newline would
    // let a value from Plex's answer write headers of its own.
    if rating_key.is_empty() || !rating_key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    {
        let mut cache = SOURCES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|(_, at, _)| at.elapsed() < SOURCE_TTL);
        if let Some((_, _, source)) = cache.iter().find(|(key, _, _)| key == rating_key) {
            return Some(source.clone());
        }
    }

    let body = ask(&format!("/library/metadata/{rating_key}"), token).ok()?;
    let source = source_from_metadata(&body)?;

    let mut cache = SOURCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.len() >= SOURCE_CACHE {
        cache.remove(0);
    }
    cache.push((rating_key.to_owned(), Instant::now(), source.clone()));
    Some(source)
}

// ------------------------------------------------------------------------------------
// ARTWORK.
//
// A poster is the same class of information as a title: it says what somebody in this house
// is watching this evening. So it is behind the same credential, and the browser reaches it
// the same way -- an authenticated POST, whose bytes it turns into an object URL, because
// `<img src>` cannot send an Authorization header and a GET on this console needs no
// credential at all.
//
// **This is not a proxy.** The browser supplies one thing, a rating key, and it is a number.
// It cannot name a host, a port, a path, a URL or a token. Everything else is resolved here
// from Plex's own metadata, and the path that comes back is refused unless it is a path on
// the local server -- Plex publishes absolute URLs for some artwork, and following one would
// turn this into exactly the general fetcher it must not be.
// ------------------------------------------------------------------------------------

/// The largest poster this will hand to a browser.
///
/// Plex's own thumbnails are tens of kilobytes; four megabytes is far above anything it
/// serves and far below anything that matters to this machine's memory. The read is bounded
/// rather than trusted, so a Plex that answered with a gigabyte would be cut off rather than
/// held.
pub const MAX_ARTWORK: u64 = 4 * 1024 * 1024;

/// The image formats this appliance will serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// JPEG, which is what Plex serves for almost everything.
    Jpeg,
    /// PNG.
    Png,
    /// WebP.
    Webp,
}

impl ImageKind {
    /// The `Content-Type` this appliance will put on it.
    ///
    /// A fixed string from a closed set, never a value echoed from Plex: a `Content-Type`
    /// copied out of somebody else's answer is a header this machine did not write.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
        }
    }

    /// What the bytes actually are.
    ///
    /// Decided from the signature rather than from what the server claimed, because the
    /// claim is a string from another program and the bytes are the thing being handed to a
    /// browser. A `Content-Type` of `image/jpeg` on something that is not one is exactly the
    /// case worth refusing.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some(Self::Jpeg);
        }
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(Self::Png);
        }
        if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            return Some(Self::Webp);
        }
        None
    }
}

/// Whether a rating key is one this appliance will ask Plex about.
///
/// Digits only, and bounded. It is interpolated into a request line, so anything carrying a
/// space, a newline or a slash could write headers of its own or address a different
/// resource -- which is the whole attack this endpoint exists to not have.
#[must_use]
pub fn valid_rating_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 12 && key.bytes().all(|b| b.is_ascii_digit())
}

/// The artwork path Plex publishes for an item, if it publishes a local one.
///
/// `thumb` first and `art` never: the poster is what a person recognises, and the backdrop
/// is a different picture that would look like the wrong one. `grandparentThumb` is the
/// series poster and is right for an episode, which usually has no poster of its own.
///
/// Refused unless it is a path on this server. Plex hands out absolute `https://` URLs for
/// artwork it has fetched from its own services, and those carry a token in the query --
/// following one would both leak the credential and make this a fetcher of arbitrary hosts.
#[must_use]
pub fn local_artwork_path(metadata: &str) -> Option<String> {
    let document: Value = serde_json::from_str(metadata).ok()?;
    let entry = document
        .get("MediaContainer")?
        .get("Metadata")?
        .as_array()?
        .first()?;

    let candidate = text(entry, "thumb").or_else(|| text(entry, "grandparentThumb"))?;

    // A path on this server, and nothing else. One leading slash, no scheme, no authority,
    // no traversal, and no query -- a query is where a token would ride.
    if !candidate.starts_with('/')
        || candidate.starts_with("//")
        || candidate.contains("://")
        || candidate.contains("..")
        || candidate.contains('?')
        || candidate.contains('\r')
        || candidate.contains('\n')
        || candidate.contains(' ')
    {
        return None;
    }
    Some(candidate)
}

/// Reads a response whose body is bytes rather than text.
///
/// Separate from [`ask`] because that one is lossy by design -- a film title in a script
/// this build cannot represent must not lose the session it belongs to -- and lossy is
/// exactly wrong for an image, where every byte is the thing.
fn ask_bytes(path: &str, token: &str) -> Result<Vec<u8>, Trouble> {
    let address = crate::plex::LOOPBACK_ADDRESS
        .parse()
        .map_err(|_| Trouble::Unreachable)?;

    let mut stream = std::net::TcpStream::connect_timeout(&address, TIMEOUT).map_err(|e| {
        if e.kind() == std::io::ErrorKind::ConnectionRefused {
            Trouble::NotRunning
        } else {
            Trouble::Unreachable
        }
    })?;
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));

    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nAccept: image/*\r\n\
         X-Plex-Token: {token}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|_| Trouble::Unreachable)?;

    // Bounded before it is read, not after. One extra byte so an image exactly at the limit
    // is told apart from one over it.
    let mut raw = Vec::new();
    stream
        .take(MAX_ARTWORK + 1)
        .read_to_end(&mut raw)
        .map_err(|_| Trouble::Unreachable)?;

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(Trouble::Unreadable)?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = raw.split_off(split + 4);

    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or(Trouble::Unreadable)?;

    match status {
        // A redirect is refused rather than followed. Plex answers some artwork requests
        // with one, and the target is chosen by Plex rather than by this appliance -- which
        // is the general fetcher this endpoint must not become, and is also where a
        // credential-bearing URL would appear.
        "200" => Ok(body),
        "401" | "403" => Err(Trouble::Refused),
        _ => Err(Trouble::Unreadable),
    }
}

/// The poster for a session's item, read with the credential this process holds.
///
/// The token is read, used and dropped inside this call, exactly as [`observe`] does: it
/// exists for the length of one request and is never held anywhere that outlives it, and it
/// is never returned, logged or put in a header.
///
/// `None` when the machine is not provisioned, Plex is not running or not claimed, the key
/// is not one this appliance will ask about, or the answer is not an image it serves. One
/// answer for all of them: the caller has one thing to draw either way, and the differences
/// are facts about somebody's library.
#[must_use]
pub fn poster_for(mount: &Path, rating_key: &str) -> Option<(ImageKind, Vec<u8>)> {
    if !valid_rating_key(rating_key) {
        return None;
    }
    if !crate::plex::is_provisioned(mount) {
        return None;
    }
    let preferences = std::fs::read_to_string(crate::plex::preferences_file()).ok()?;
    let token = crate::plex::account_token(&preferences)?;
    poster(rating_key, token)
}

/// The poster for one library item, as bytes this appliance is willing to serve.
///
/// `None` for every failure, and deliberately without distinguishing them: the page draws
/// its own placeholder either way, and a message naming which internal step failed is a
/// message about Plex's library and its filesystem.
fn poster(rating_key: &str, token: &str) -> Option<(ImageKind, Vec<u8>)> {
    if !valid_rating_key(rating_key) {
        return None;
    }
    let metadata = ask(&format!("/library/metadata/{rating_key}"), token).ok()?;
    let path = local_artwork_path(&metadata)?;
    let bytes = ask_bytes(&path, token).ok()?;

    if bytes.len() as u64 > MAX_ARTWORK {
        return None;
    }
    let kind = ImageKind::sniff(&bytes)?;
    Some((kind, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing playing. Captured verbatim from the appliance: this really is the whole
    /// document, and the absence of a `Metadata` key is what "idle" looks like.
    const IDLE: &str = r#"{"MediaContainer":{"size":0}}"#;

    /// A hardware transcode, captured from Plex 1.43.3.10861 on the appliance and then
    /// redacted: the film, the account, the avatar and the file path are invented, and the
    /// token that a real capture carries is replaced by [`PLANTED_TOKEN`] so that the
    /// no-credentials test has something to fail on. Every *key*, and every value that
    /// describes the machine's behaviour, is exactly as the appliance produced it —
    /// including `speed: 0.0` beside `throttled: true`, which is a healthy transcode that
    /// has run far enough ahead.
    const HARDWARE_TRANSCODE: &str = r#"{"MediaContainer":{"size":1,"Metadata":[{
      "addedAt":1770650883,"duration":10143000,"guid":"plex://movie/0000",
      "key":"/library/metadata/118","librarySectionTitle":"Films","ratingKey":"118",
      "sessionKey":"1","title":"Test Feature","type":"movie","viewOffset":5538000,
      "year":2024,
      "Media":[{"audioChannels":2,"audioCodec":"aac","bitrate":2664,"container":"mpegts",
        "height":692,"id":"289","protocol":"hls","videoCodec":"h264","videoFrameRate":"24p",
        "videoProfile":"main","videoResolution":"720p","width":1280,"selected":true,
        "Part":[{"bitrate":2664,"container":"mpegts","decision":"transcode","duration":10143000,
          "height":692,"id":"289","protocol":"hls","width":1280,"selected":true,
          "Stream":[{"bitDepth":8,"bitrate":2566,"codec":"h264","colorTrc":"bt709",
            "decision":"transcode","displayTitle":"4K (HEVC Main 10)","frameRate":23.976,
            "height":692,"id":"791","language":"English","location":"segments-av",
            "streamType":1,"width":1280},
           {"bitrate":98,"channels":2,"codec":"aac","decision":"transcode",
            "displayTitle":"English (TRUEHD 7.1)","id":"792","language":"English",
            "location":"segments-av","selected":true,"streamType":2},
           {"codec":"srt","decision":"ignore","id":"793","language":"polski",
            "location":"sidecar-subs","selected":true,"streamType":3}]}]}],
      "User":{"id":"1","thumb":"https://plex.tv/users/0000/avatar?X-Plex-Token=SECRET-PLEX-TOKEN","title":"Sebastian"},
      "Player":{"address":"127.0.0.1","device":"AppleTV","machineIdentifier":"abc",
        "platform":"tvOS","product":"Plex for Apple TV","profile":"tvOS","state":"playing",
        "title":"Living Room TV","local":true,"relayed":false,"secure":false,"userID":1,
        "remotePublicAddress":"203.0.113.7"},
      "Session":{"id":"abc","bandwidth":2798,"location":"lan"},
      "TranscodeSession":{"key":"/transcode/sessions/abc","throttled":true,"complete":false,
        "progress":1.3,"size":-1,"speed":0.0,"error":false,"duration":10143000,
        "remaining":44136,"context":"streaming","sourceVideoCodec":"hevc",
        "sourceAudioCodec":"truehd","videoDecision":"transcode","audioDecision":"transcode",
        "subtitleDecision":"ignore","protocol":"hls","container":"mpegts","videoCodec":"h264",
        "audioCodec":"aac","audioChannels":2,"transcodeHwRequested":true,
        "transcodeHwDecoding":"vaapi","transcodeHwEncoding":"vaapi",
        "transcodeHwDecodingTitle":"Intel (VA API)","transcodeHwFullPipeline":true,
        "transcodeHwEncodingTitle":"Intel (VA API)"}}]}}"#;

    /// The token planted in the fixtures. Not a real one — a real one is 20 characters from
    /// Plex and lives only on the appliance.
    const PLANTED_TOKEN: &str = "SECRET-PLEX-TOKEN";

    /// The same transcode a second after it started, before ffmpeg has produced anything.
    /// Captured from the appliance, and the point of it is that it is **indistinguishable
    /// from software** unless the absence is treated as an absence.
    const TRANSCODE_NOT_STARTED: &str = r#"{"MediaContainer":{"size":1,"Metadata":[{
      "ratingKey":"1","sessionKey":"2","title":"Test Feature","type":"movie","duration":6906064,
      "viewOffset":120000,
      "Media":[{"audioChannels":2,"audioCodec":"aac","bitrate":3538,"container":"mpegts",
        "videoCodec":"h264","videoResolution":"720p","selected":true,
        "Part":[{"decision":"transcode","id":"1","Stream":[{"codec":"h264","decision":"transcode",
          "streamType":1}]}]}],
      "User":{"id":"1","title":"Sebastian"},
      "Player":{"platform":"Chrome","product":"Plex Web","state":"playing","title":"Study","local":true},
      "TranscodeSession":{"key":"/transcode/sessions/x","throttled":false,"complete":false,
        "progress":0.0,"size":0,"speed":0.0,"error":false,"context":"streaming",
        "sourceVideoCodec":"hevc","sourceAudioCodec":"eac3","videoDecision":"transcode",
        "audioDecision":"transcode","subtitleDecision":"ignore","protocol":"hls",
        "container":"mpegts","videoCodec":"h264","audioCodec":"aac","audioChannels":2,
        "transcodeHwRequested":false,"transcodeHwDecodingTitle":"Intel ()",
        "transcodeHwFullPipeline":false,"transcodeHwEncodingTitle":"Intel ()"}}]}}"#;

    /// A software transcode: running — progress has moved — and naming no hardware.
    const SOFTWARE_TRANSCODE: &str = r#"{"MediaContainer":{"size":1,"Metadata":[{
      "ratingKey":"7","sessionKey":"9","title":"Test Feature","type":"movie","duration":100000,
      "viewOffset":1000,
      "Media":[{"videoCodec":"h264","videoResolution":"1080","audioCodec":"aac","selected":true,
        "Part":[{"decision":"transcode","Stream":[{"codec":"h264","streamType":1}]}]}],
      "Player":{"state":"playing","title":"Laptop"},
      "TranscodeSession":{"progress":12.5,"speed":1.4,"throttled":false,"error":false,
        "sourceVideoCodec":"hevc","sourceAudioCodec":"ac3","videoDecision":"transcode",
        "audioDecision":"copy","videoCodec":"h264","audioCodec":"ac3",
        "transcodeHwRequested":false,"transcodeHwDecodingTitle":"","transcodeHwEncodingTitle":""}}]}}"#;

    /// Direct Play, captured from the appliance. The whole point of this fixture is what it
    /// does **not** contain: no `TranscodeSession`, no `decision`, no `Session` node. Plex
    /// says nothing, and Direct Play has to be read out of that silence.
    const DIRECT_PLAY: &str = r#"{"MediaContainer":{"size":1,"Metadata":[{
      "ratingKey":"118","sessionKey":"3","title":"Test Feature","type":"movie",
      "duration":5692256,"viewOffset":300000,
      "Media":[{"aspectRatio":"1.85","audioChannels":6,"audioCodec":"ac3","bitrate":2085,
        "container":"mkv","duration":5692256,"height":1038,"id":"289","videoCodec":"hevc",
        "videoFrameRate":"24p","videoProfile":"main","videoResolution":"1080","width":1920,
        "Part":[{"container":"mkv","duration":5692256,"file":"/var/media/nas/Test.mkv",
          "id":"289","key":"/library/parts/289/1/file.mkv","size":1483721609,
          "Stream":[{"bitDepth":8,"bitrate":1701,"codec":"hevc","colorTrc":"smpte2084",
            "DOVIPresent":true,"colorPrimaries":"bt2020","default":true,"height":1038,
            "id":"791","language":"English","streamType":1,"width":1920},
           {"channels":6,"codec":"ac3","id":"792","language":"English","streamType":2,
            "selected":true}]}]}],
      "User":{"id":"1","title":"Sebastian"},
      "Player":{"address":"127.0.0.1","device":"AppleTV","platform":"tvOS",
        "product":"Plex for Apple TV","state":"paused","title":"Living Room TV","local":true}}]}}"#;

    /// A session carrying almost nothing. Plex has never produced this; it is the shape a
    /// future version producing less would have, and the model has to survive it.
    const SPARSE: &str = r#"{"MediaContainer":{"size":1,"Metadata":[{"sessionKey":"11"}]}}"#;

    /// A library record for a Dolby Vision file, from the appliance. `colorTrc`,
    /// `bitDepth`, `DOVIPresent` and `DOVIProfile` are as captured; Plex's own display
    /// string for this file was `4K DoVi/HDR10`.
    const LIBRARY_DOLBY_VISION: &str = r#"{"MediaContainer":{"size":1,"Metadata":[{
      "ratingKey":"2","title":"Test Feature","type":"movie",
      "Media":[{"id":2,"duration":6560387,"bitrate":24399,"width":3832,"height":1596,
        "audioChannels":6,"audioCodec":"eac3","videoCodec":"hevc","videoResolution":"4k",
        "container":"mkv","videoProfile":"main 10",
        "Part":[{"id":2,"key":"/library/parts/2/1/file.mkv","file":"/var/media/nas/Test.mkv",
          "Stream":[{"id":27,"streamType":1,"codec":"hevc","bitrate":23631,"DOVIPresent":true,
            "DOVIProfile":8,"bitDepth":10,"colorPrimaries":"bt2020","colorSpace":"bt2020nc",
            "colorTrc":"smpte2084","height":1596,"profile":"main 10","width":3832,
            "displayTitle":"4K DoVi/HDR10"},
           {"id":28,"streamType":2,"codec":"eac3","channels":6}]}]}]}]}}"#;

    fn one(fixture: &str) -> Session {
        let mut sessions = parse(fixture).expect("the fixture parses");
        assert_eq!(sessions.len(), 1, "one session per fixture");
        sessions.pop().unwrap()
    }

    #[test]
    fn nothing_playing_is_an_empty_list_rather_than_a_failure() {
        // Plex says `{"size":0}` and omits `Metadata` entirely. Treating a missing key as a
        // malformed document would report a healthy idle appliance as a broken one.
        assert_eq!(parse(IDLE), Ok(Vec::new()));
    }

    #[test]
    fn a_hardware_transcode_is_reported_as_one() {
        let session = one(HARDWARE_TRANSCODE);

        assert_eq!(session.decision, Decision::Transcode);
        assert_eq!(session.video.decision, Decision::Transcode);
        assert_eq!(session.video.source_codec.as_deref(), Some("hevc"));
        assert_eq!(session.video.target_codec.as_deref(), Some("h264"));
        assert_eq!(session.video.target_resolution.as_deref(), Some("720p"));
        assert_eq!(
            session.video.hardware,
            Some(true),
            "a named decoder and encoder is the only positive evidence there is"
        );
        assert_eq!(
            session.video.hardware_detail.as_deref(),
            Some("Intel (VA API)")
        );
        assert_eq!(session.video.full_pipeline, Some(true));
        assert_eq!(session.audio.source_codec.as_deref(), Some("truehd"));
        assert_eq!(session.audio.target_codec.as_deref(), Some("aac"));
        assert_eq!(session.audio.target_channels, Some(2));
        assert_eq!(session.user.as_deref(), Some("Sebastian"));
        assert_eq!(session.player.as_deref(), Some("Living Room TV"));
        assert_eq!(session.platform.as_deref(), Some("tvOS"));
        assert_eq!(session.state.as_deref(), Some("playing"));
        assert_eq!(session.position_ms, Some(5_538_000));
        assert_eq!(session.duration_ms, Some(10_143_000));
        assert_eq!(session.stream_bitrate_kbps, Some(2798));
    }

    #[test]
    fn a_transcode_that_has_not_started_is_not_called_software() {
        // The finding that shaped the field. This capture and the hardware one above are the
        // *same session* seconds apart: before ffmpeg runs, Plex reports
        // `transcodeHwRequested: false` and `Intel ()`, which is exactly what a software
        // transcode reports. Answering `false` here would put an amber "software transcode"
        // warning on this appliance's best case every time somebody pressed play.
        let session = one(TRANSCODE_NOT_STARTED);
        assert_eq!(session.decision, Decision::Transcode);
        assert_eq!(
            session.video.hardware, None,
            "not established is not the same answer as no"
        );
        assert_eq!(session.video.hardware_detail, None);
    }

    #[test]
    fn only_a_video_transcode_has_a_hardware_verdict() {
        // Found on the appliance, on a real Direct Stream: the video was copied and only the
        // audio re-encoded, the transcode session had progressed, and it named no decoder —
        // so "has it named hardware" answered `false` about a picture nothing was decoding.
        //
        // `SOFTWARE_TRANSCODE` has `audioDecision: "copy"` with the video transcoded, so this
        // builds the mirror image of it rather than reusing a fixture that is the other case.
        let direct_stream = SOFTWARE_TRANSCODE
            .replace(
                "\"videoDecision\":\"transcode\"",
                "\"videoDecision\":\"copy\"",
            )
            .replace(
                "\"audioDecision\":\"copy\"",
                "\"audioDecision\":\"transcode\"",
            );
        let session = one(&direct_stream);

        assert_eq!(session.decision, Decision::DirectStream);
        assert_eq!(session.video.decision, Decision::DirectStream);
        assert_eq!(
            session.video.hardware, None,
            "nothing is decoding the picture, so there is no hardware verdict to give"
        );
        assert_eq!(session.video.hardware_detail, None);
        assert_eq!(session.video.full_pipeline, None);
        assert_eq!(
            session.audio.decision,
            Decision::Transcode,
            "and the audio, which is what is actually being worked on, still says so"
        );
    }

    #[test]
    fn a_running_transcode_with_no_hardware_named_is_software() {
        // The other half: progress has moved, so the transcoder has demonstrably done work,
        // and it named no decoder or encoder. That is a real amber.
        let session = one(SOFTWARE_TRANSCODE);
        assert_eq!(session.video.hardware, Some(false));
        assert_eq!(
            session.decision,
            Decision::Transcode,
            "video re-encoded, whatever the audio is doing"
        );
        assert_eq!(session.audio.decision, Decision::DirectStream);
    }

    #[test]
    fn direct_play_is_read_out_of_plexs_silence() {
        // Verified on the appliance: a direct-play session has no TranscodeSession node and
        // no `decision` anywhere. There is nothing to read, so the absence is the answer —
        // and `Decision::default()` is deliberately `Unknown` so this cannot be arrived at
        // by accident.
        let session = one(DIRECT_PLAY);
        assert_eq!(session.decision, Decision::DirectPlay);
        assert_eq!(session.video.decision, Decision::DirectPlay);
        assert_eq!(session.audio.decision, Decision::DirectPlay);
        assert!(session.transcode.is_none());

        // With nothing being transcoded the file *is* the stream, so the source is readable
        // straight out of the session.
        assert_eq!(session.video.source_codec.as_deref(), Some("hevc"));
        assert_eq!(session.video.source_resolution.as_deref(), Some("1080p"));
        assert_eq!(
            session.video.source_hdr.as_deref(),
            Some("Dolby Vision / HDR10")
        );
        assert_eq!(session.audio.source_channels, Some(6));
        assert_eq!(session.source_bitrate_kbps, Some(2085));
    }

    #[test]
    fn a_paused_session_says_so() {
        // Paused is the state a person is most likely to be looking at when they open this
        // page, and it is the Player's word rather than anything about the transcode.
        assert_eq!(one(DIRECT_PLAY).state.as_deref(), Some("paused"));
    }

    #[test]
    fn a_session_missing_everything_optional_still_survives() {
        // The rule that stops one absent field costing a whole session. What is left is a
        // row saying something is playing and admitting it knows nothing else, which is a
        // better answer than a stream that vanished.
        let session = one(SPARSE);
        assert_eq!(session.id.as_deref(), Some("11"));
        assert_eq!(session.title, None);
        assert_eq!(session.user, None);
        assert_eq!(
            session.decision,
            Decision::DirectPlay,
            "no transcode session means Plex is not transcoding it"
        );
        assert!(session.transcode.is_none());
    }

    #[test]
    fn a_malformed_answer_is_an_error_and_not_a_panic() {
        // Every one of these has to come back as a refusal to answer rather than take the
        // console down with it. The route turns them into `State::Unreadable`, which names
        // reporting it as the remedy — because nothing on the appliance is wrong.
        for rubbish in [
            "",
            "not json at all",
            "{}",
            r#"{"MediaContainer":"a string"}"#,
            r#"{"MediaContainer":{"Metadata":"not a list"}}"#,
        ] {
            let outcome = parse(rubbish);
            assert!(
                outcome.is_err() || outcome == Ok(Vec::new()),
                "{rubbish:?} must not panic and must not invent sessions"
            );
        }
    }

    /// A metadata document shaped like Plex's, with a token planted where a real one lives.
    const ARTWORK_METADATA: &str = r#"{"MediaContainer":{"size":1,"Metadata":[{
        "ratingKey":"51231","title":"A film",
        "thumb":"/library/metadata/51231/thumb/1699887766",
        "art":"/library/metadata/51231/art/1699887766",
        "Media":[{"Part":[{"Stream":[]}]}]}]}}"#;

    #[test]
    fn a_rating_key_is_a_number_and_nothing_else() {
        // It is interpolated into a request line, so a value carrying a space, a newline or
        // a slash could write headers of its own or address a different resource. This is
        // the whole of what the browser gets to choose, which is why it is the whole of what
        // has to be checked.
        assert!(valid_rating_key("51231"));
        assert!(valid_rating_key("1"));

        for bad in [
            "",
            "51231 ",
            "51231\r\nX-Plex-Token: stolen",
            "../../etc/passwd",
            "51231/thumb",
            "abc",
            "-1",
            "5123151231512315", // longer than any key Plex issues
            "http://elsewhere.invalid/x",
        ] {
            assert!(!valid_rating_key(bad), "{bad:?} must not be asked about");
        }
    }

    #[test]
    fn artwork_is_taken_from_the_metadata_and_only_when_it_is_local() {
        assert_eq!(
            local_artwork_path(ARTWORK_METADATA).as_deref(),
            Some("/library/metadata/51231/thumb/1699887766"),
            "the poster comes from `thumb`, which is the picture a person recognises"
        );

        // Plex hands out absolute URLs for artwork it fetched from its own services, and
        // those carry a token in the query. Following one would leak the credential *and*
        // turn this endpoint into a fetcher of arbitrary hosts, which is the one thing it
        // must never be.
        for hostile in [
            r#"{"MediaContainer":{"Metadata":[{"thumb":"https://plex.tv/photo?X-Plex-Token=SECRET"}]}}"#,
            r#"{"MediaContainer":{"Metadata":[{"thumb":"//evil.invalid/x.jpg"}]}}"#,
            r#"{"MediaContainer":{"Metadata":[{"thumb":"/library/../../etc/passwd"}]}}"#,
            r#"{"MediaContainer":{"Metadata":[{"thumb":"/library/metadata/1/thumb?X-Plex-Token=SECRET"}]}}"#,
            r#"{"MediaContainer":{"Metadata":[{"thumb":"/library/metadata/1\r\nHost: evil"}]}}"#,
            r#"{"MediaContainer":{"Metadata":[{"thumb":""}]}}"#,
            r#"{"MediaContainer":{"size":0}}"#,
        ] {
            assert_eq!(local_artwork_path(hostile), None, "must refuse: {hostile}");
        }
    }

    #[test]
    fn only_images_this_appliance_recognises_are_served() {
        // Decided from the bytes rather than from what the server claimed: a `Content-Type`
        // of `image/jpeg` on something that is not one is exactly the case worth refusing,
        // and the claim is a string from another program.
        assert_eq!(
            ImageKind::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(
            ImageKind::sniff(b"\x89PNG\r\n\x1a\nrest"),
            Some(ImageKind::Png)
        );
        assert_eq!(
            ImageKind::sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageKind::Webp)
        );

        for not_an_image in [
            &b"<!DOCTYPE html><html>"[..],
            &b"{\"MediaContainer\":{}}"[..],
            &b"GIF89a"[..],
            &b""[..],
            &b"\xFF\xD8"[..], // truncated below the signature
        ] {
            assert_eq!(ImageKind::sniff(not_an_image), None);
        }

        // The type on the wire is one of three fixed strings this appliance wrote, never a
        // value echoed out of Plex's answer.
        for kind in [ImageKind::Jpeg, ImageKind::Png, ImageKind::Webp] {
            assert!(kind.mime().starts_with("image/"));
        }
    }

    #[test]
    fn the_poster_path_never_carries_a_credential() {
        // The endpoint's whole surface, checked for the invariant the sessions route already
        // holds: the browser sends a number, and nothing that comes back names a token.
        let path = local_artwork_path(ARTWORK_METADATA).expect("a local path");
        assert!(!path.contains("X-Plex-Token"), "{path}");
        assert!(!path.contains(PLANTED_TOKEN), "{path}");
        assert!(
            !path.contains('?'),
            "a query is where a token would ride: {path}"
        );

        // And the refusal a browser is given says nothing about the library, the filesystem
        // or Plex's answer -- there is one refusal for every failure, by construction.
        assert_eq!(
            local_artwork_path(
                r#"{"MediaContainer":{"Metadata":[{"thumb":"https://plex.tv/x?X-Plex-Token=SECRET"}]}}"#
            ),
            None
        );
    }

    #[test]
    fn artwork_is_refused_before_anything_is_asked_when_the_key_is_wrong() {
        // Fails closed, and fails without a request: a malformed key never reaches the point
        // where a connection would be opened, so a page cannot use this to make the
        // appliance talk to anything.
        let nowhere = std::path::Path::new("/nonexistent/plex/mount");
        assert!(poster_for(nowhere, "not-a-key").is_none());
        assert!(poster_for(nowhere, "").is_none());
        // And an unprovisioned machine answers the same way rather than explaining itself.
        assert!(poster_for(nowhere, "51231").is_none());
    }

    #[test]
    fn no_plex_credential_can_reach_the_browser() {
        // The absolute requirement, asserted against the thing that actually leaves this
        // process. The fixture carries a token in a `User.thumb` URL, which is where a real
        // capture has one, and the report is serialised exactly as the route serialises it.
        //
        // This is a property of the model rather than of the code that fills it: the model
        // has no field a token could land in. That is why the test greps the output instead
        // of checking a particular field, and why adding a field that copies Plex's answer
        // wholesale would fail here rather than on an appliance.
        assert!(
            HARDWARE_TRANSCODE.contains(PLANTED_TOKEN),
            "the fixture has to carry a token for this test to mean anything"
        );

        let sessions = parse(HARDWARE_TRANSCODE).expect("parses");
        let report = Report {
            available: true,
            state: State::Playing,
            detail: State::Playing.detail().to_owned(),
            active: sessions.len(),
            sessions,
        };
        let json = serde_json::to_string(&report).expect("serialises");

        assert!(
            !json.contains(PLANTED_TOKEN),
            "a Plex credential reached the browser: {json}"
        );
        for leaked in ["X-Plex-Token", "plex.tv", "203.0.113.7", "/var/media/"] {
            assert!(
                !json.contains(leaked),
                "{leaked} has no business in a browser response: {json}"
            );
        }
    }

    #[test]
    fn a_transcodes_source_comes_from_the_library_and_only_when_it_has_to() {
        // A transcoding session's Media node is the *output*, so the file's resolution and
        // HDR format are not in it. Direct play needs no lookup at all, and asking anyway
        // would be a request per poll for something already in hand.
        let mut asked = Vec::new();
        let mut sessions = parse(HARDWARE_TRANSCODE).expect("parses");
        enrich(&mut sessions, &mut |key| {
            asked.push(key.to_owned());
            source_from_metadata(LIBRARY_DOLBY_VISION)
        });

        assert_eq!(asked, ["118"], "one lookup, for the item being transcoded");
        let video = &sessions[0].video;
        assert_eq!(video.source_resolution.as_deref(), Some("4K"));
        assert_eq!(video.source_hdr.as_deref(), Some("Dolby Vision / HDR10"));
        assert_eq!(
            video.source_codec.as_deref(),
            Some("hevc"),
            "the session already said this, and it wins over the library"
        );
        assert_eq!(sessions[0].source_bitrate_kbps, Some(24399));

        let mut direct = parse(DIRECT_PLAY).expect("parses");
        let mut count = 0;
        enrich(&mut direct, &mut |_| {
            count += 1;
            None
        });
        assert_eq!(count, 0, "a direct play already carries its own source");
    }

    #[test]
    fn a_source_that_cannot_be_looked_up_leaves_the_session_intact() {
        // An enrichment that fails is not a session that fails.
        let mut sessions = parse(HARDWARE_TRANSCODE).expect("parses");
        enrich(&mut sessions, &mut |_| None);
        assert_eq!(sessions[0].video.source_codec.as_deref(), Some("hevc"));
        assert_eq!(sessions[0].video.source_resolution, None);
        assert_eq!(sessions[0].title.as_deref(), Some("Test Feature"));
    }

    #[test]
    fn hdr_is_read_from_fields_and_never_from_plexs_display_string() {
        // Pinned against values captured from the appliance rather than against this
        // module's own output. The Dolby Vision case is the file Plex itself labels
        // `4K DoVi/HDR10`.
        let dv = source_from_metadata(LIBRARY_DOLBY_VISION).expect("a source");
        assert_eq!(dv.hdr.as_deref(), Some("Dolby Vision / HDR10"));
        assert_eq!(dv.resolution.as_deref(), Some("4K"));
        assert_eq!(dv.audio_channels, Some(6));

        let cases = [
            (json!({"colorTrc":"smpte2084"}), Some("HDR10")),
            (json!({"colorTrc":"arib-std-b67"}), Some("HLG")),
            (json!({"colorTrc":"bt709"}), None),
            (
                json!({"DOVIPresent":true,"colorTrc":"bt709"}),
                Some("Dolby Vision"),
            ),
            (json!({}), None),
        ];
        for (stream, expected) in cases {
            assert_eq!(hdr_format(&stream).as_deref(), expected, "{stream}");
        }
    }

    #[test]
    fn plexs_two_spellings_of_a_resolution_become_one() {
        // `4k` in the library, `720p` in a session — the same field, two spellings, and a
        // page that passed both through would show "4k" beside "1080p".
        assert_eq!(resolution("4k").as_deref(), Some("4K"));
        assert_eq!(resolution("1080").as_deref(), Some("1080p"));
        assert_eq!(resolution("720p").as_deref(), Some("720p"));
        assert_eq!(resolution("sd").as_deref(), Some("SD"));
        assert_eq!(resolution(""), None);
    }

    #[test]
    fn a_number_is_read_whichever_way_plex_wrote_it() {
        // `"id":"289"` in a session and `"id":289` in the library, in the same server.
        let value = json!({"a":289,"b":"289","c":-1,"d":1.9,"e":"","f":true});
        assert_eq!(number(&value, "a"), Some(289));
        assert_eq!(number(&value, "b"), Some(289));
        assert_eq!(number(&value, "c"), None, "size: -1 means 'not yet'");
        assert_eq!(number(&value, "d"), Some(1));
        assert_eq!(number(&value, "e"), None);
        assert_eq!(number(&value, "f"), None);
        assert_eq!(text(&value, "a").as_deref(), Some("289"));
    }

    #[test]
    fn an_episode_is_named_only_when_both_halves_are_there() {
        // Half of it is worse than none: `E05` without a season is ambiguous on any series
        // with more than one.
        assert_eq!(
            episode(&json!({"parentIndex":2,"index":5})).as_deref(),
            Some("S02E05")
        );
        assert_eq!(episode(&json!({"index":5})), None);
        assert_eq!(episode(&json!({"parentIndex":2})), None);
    }

    #[test]
    fn every_state_names_a_remedy() {
        // The house rule: a report that says what is wrong and stops has reproduced the
        // problem this project exists to fix. It applies to the two states that are not
        // faults as well, where the remedy is "none needed" and saying so is the point.
        for state in [
            State::NotProvisioned,
            State::NotRunning,
            State::NotClaimed,
            State::NoToken,
            State::Unreachable,
            State::Refused,
            State::Unreadable,
            State::Idle,
            State::Playing,
        ] {
            let detail = state.detail();
            assert!(
                detail.contains("Remedy:"),
                "{state:?} does not name a remedy: {detail}"
            );
            assert!(
                detail.len() > 40,
                "{state:?} is too terse to be useful: {detail}"
            );
        }
    }

    #[test]
    fn every_state_is_a_different_answer_to_a_different_question() {
        // Two states with the same words would be one state with a bug. This is the same
        // check the update path needed when "already up to date" and "found nothing" shared
        // a return value and said the wrong thing on a machine.
        let mut seen = Vec::new();
        for state in [
            State::NotProvisioned,
            State::NotRunning,
            State::NotClaimed,
            State::NoToken,
            State::Unreachable,
            State::Refused,
            State::Unreadable,
            State::Idle,
            State::Playing,
        ] {
            assert!(!seen.contains(&state.detail()), "{state:?} repeats another");
            seen.push(state.detail());
        }
    }

    #[test]
    fn an_explanation_carries_no_sessions_and_says_so_twice() {
        // `available` exists so the page never decides by matching on a state name, and it
        // has to agree with the list beside it.
        let report = Report::of(State::NotClaimed);
        assert!(!report.available);
        assert_eq!(report.active, 0);
        assert!(report.sessions.is_empty());
        assert_eq!(report.detail, State::NotClaimed.detail());
    }

    #[test]
    fn a_rating_key_that_is_not_a_number_is_never_put_into_a_request() {
        // It is interpolated into a request line. A value carrying a newline would write
        // headers of its own, and the value comes from Plex's answer rather than from here.
        for hostile in ["", "1 2", "1\r\nX-Plex-Token: stolen", "../../etc", "1;"] {
            assert_eq!(
                library_source(hostile, "unused"),
                None,
                "{hostile:?} must never reach a request line"
            );
        }
    }

    use serde_json::json;
}

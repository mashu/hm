//! What a station knows, as a snapshot for people: every station it has
//! heard of and where it is, every path with what is believed of it (within
//! reach? open now? when in the day? how lossy? how likely a handoff?),
//! every custodian, the channel, how the chances it gives have come true,
//! the latest evidence and what it did, and why each message goes or waits.
//!
//! These are plain values, built by [`Node::insight`](crate::Node::insight)
//! from the node's state and serialised as they are by the daemon's API.
//! Nothing here is decided on: the node decides on its beliefs, and this
//! only shows them.

use serde::Serialize;

/// Everything shown, as of `at` (Unix seconds).
#[derive(Clone, Debug, Serialize)]
pub struct Insight {
    pub at: u64,
    pub me: String,
    pub stations: Vec<StationView>,
    pub links: Vec<LinkView>,
    pub custodians: Vec<CustodianView>,
    pub channel: ChannelView,
    pub calibration: Vec<CalibrationView>,
    /// The latest observations, the last taken in first.
    pub journal: Vec<UpdateView>,
    /// The latest decision about each message the station holds.
    pub decisions: Vec<DecisionView>,
}

/// A grid square and its centre.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Place {
    pub locator: String,
    pub lat: f64,
    pub lon: f64,
}

impl From<hm_wire::Locator> for Place {
    fn from(locator: hm_wire::Locator) -> Place {
        let (lat, lon) = locator.centre();
        Place {
            locator: locator.to_string(),
            lat,
            lon,
        }
    }
}

/// A station this one has heard of: heard on the air, trusted, named in a
/// beacon or an advert, or a destination.
#[derive(Clone, Debug, Serialize)]
pub struct StationView {
    pub call: String,
    /// This station.
    pub me: bool,
    /// Where its beacon (ours: our settings) says it is.
    pub place: Option<Place>,
    /// When any frame from it was last heard here.
    pub heard_at: Option<u64>,
    /// Its latest beacon heard here.
    pub beacon: Option<BeaconView>,
    /// Whether its key is among the trusted.
    pub trusted: bool,
    /// What it offers, as its beacon or adverts say.
    pub offers: Vec<&'static str>,
    /// Bearers it is in contact over now: heard on the radio within the
    /// live window, an internet session, the ARQ modem.
    pub linked: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BeaconView {
    pub at: u64,
    /// "trusted", "unknown" or "mismatch": its key against the trusted one.
    pub key: &'static str,
    /// Its clock minus ours, seconds.
    pub clock_offset: i64,
    /// The stations it says it hears, and how many minutes ago.
    pub hears: Vec<(String, u8)>,
}

/// A belief about a rate: its mean and a 90 % credible interval.
#[derive(Copy, Clone, Debug, PartialEq, Serialize)]
pub struct Interval {
    pub mean: f64,
    pub low: f64,
    pub high: f64,
}

/// What is believed of one path (both directions share one belief).
#[derive(Clone, Debug, Serialize)]
pub struct LinkView {
    pub a: String,
    pub b: String,
    pub bearer: &'static str,
    /// One end is this station.
    pub mine: bool,
    /// Seen open at least once (else inferred: from the population, from
    /// misses and failures).
    pub seen: bool,
    /// Chance the path is within reach at all.
    pub reach: f64,
    /// Chance it is open now.
    pub open_now: f64,
    /// Chance it is open at the start of each of the next 24 hours.
    pub forecast: Vec<f64>,
    /// The learned daily pattern: chance it is open at each UTC hour, 0 to
    /// 23, were it within reach.
    pub daily: Vec<f64>,
    /// When the next hour it is at least as likely open as not begins,
    /// within a day.
    pub next_opening: Option<u64>,
    /// Frame loss while open.
    pub frame_loss: Interval,
    /// Chance a handoff completes while it is open.
    pub handoff_if_open: Interval,
    /// Chance a handoff over it started now completes: as the path model
    /// gives it, and as the station's record of its chances corrects it.
    pub success_model: f64,
    pub success: f64,
    /// Correlation time of its openings, minutes.
    pub persistence_mins: f64,
    pub last_open: Option<u64>,
    /// How often the far end beacons, once learned.
    pub beacon_interval: Option<u64>,
    /// Evidence, faded with age: frames lost and arrived; handoffs
    /// completed and failed.
    pub frames: (f64, f64),
    pub handoffs: (f64, f64),
}

/// What is believed of a station holding custody.
#[derive(Clone, Debug, Serialize)]
pub struct CustodianView {
    pub call: String,
    /// Chance it takes custody when a handoff reaches it.
    pub accepts: Interval,
    /// Chance it does its part once it has.
    pub delivers: Interval,
    /// How late its end-to-end receipts come back past the time they were
    /// due: median and 90th percentile, seconds.
    pub lateness: (f64, f64),
    /// It said it is busy until then.
    pub busy_until: Option<u64>,
    /// Evidence, faded: custody taken and refused; delivered and silent.
    pub accept_counts: (f64, f64),
    pub deliver_counts: (f64, f64),
}

/// The shared radio channel, as the station sees it.
#[derive(Clone, Debug, Serialize)]
pub struct ChannelView {
    pub radio_up: bool,
    /// Share of the time others keep it busy.
    pub busy: f64,
    /// Stations sharing it.
    pub contenders: f64,
    /// What a minute of radio airtime costs now, in delivered messages.
    pub airtime_price_per_min: f64,
    /// Our beacon interval now (longer as more stations share the channel).
    pub beacon_interval_secs: u64,
}

/// How the handoff chances given for one kind of path came true.
#[derive(Clone, Debug, Serialize)]
pub struct CalibrationView {
    pub bearer: &'static str,
    /// Paths seen open (else only inferred).
    pub seen: bool,
    /// Handoffs in the record, faded.
    pub outcomes: f64,
    /// Each band of the chance given that holds a record, lowest first.
    pub bands: Vec<BandView>,
    /// The correction: (chance given, chance that comes true), lowest first.
    pub curve: Vec<(f64, f64)>,
}

/// One band of a calibration record: the handoffs given about the same
/// chance, and how many of them were carried.
#[derive(Copy, Clone, Debug, PartialEq, Serialize)]
pub struct BandView {
    /// Mean chance given.
    pub given: f64,
    /// Share carried.
    pub carried: f64,
    /// Handoffs, faded.
    pub outcomes: f64,
}

/// One observation and what it did.
#[derive(Clone, Debug, Serialize)]
pub struct UpdateView {
    /// When what was observed happened: a beacon due and not heard is taken
    /// in after it was due.
    pub at: u64,
    /// "link", "custodian" or "calibration".
    pub subject: &'static str,
    /// The stations it is about: a path's two ends, a custodian, or none.
    pub stations: Vec<String>,
    pub bearer: Option<&'static str>,
    /// What was observed, in words.
    pub observed: String,
    pub before: f64,
    pub after: f64,
}

/// The latest decision about a message.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DecisionView {
    /// The message's id, hex.
    pub id: String,
    pub to: String,
    pub at: u64,
    #[serde(flatten)]
    pub verdict: Verdict,
}

/// What was decided.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Handed to `route`'s first hop now.
    Send { route: RouteView },
    /// Waits until `route` leaves at its first departure.
    Wait { route: RouteView },
    /// Waits to hear the first hop of `route`.
    Hear { route: RouteView },
    /// Held: no way there, or none worth its airtime, now.
    Hold { reason: String },
}

/// A route as planned.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RouteView {
    pub hops: Vec<HopView>,
    /// Chance it delivers.
    pub chance: f64,
    pub arrival: u64,
    /// Expected value less expected cost, in delivered messages.
    pub utility: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HopView {
    pub from: String,
    pub to: String,
    pub bearer: &'static str,
    pub depart: u64,
    pub chance: f64,
}

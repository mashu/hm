//! `hm`: station keys, trusted stations, sending and receiving messages, and
//! the station daemon.
//!
//! Settings and trusted stations live in `station.toml` (see
//! `hm_cli::config`); every command reads it, and a flag given on the command
//! line overrides the file for that run.
//!
//! ```text
//! hm setup                           # interactive first-run setup
//! hm whoami                          # the line to give other stations
//! hm trust add "SO5KM-1 8a1e…"       # trust a station (the line from its hm whoami)
//! hm send --to SO5KM-1 --text "73 de SA0KAM"
//! hm node                            # the station: radio, internet, web page
//! ```

mod args;
mod client;
mod commands;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hm_cli::config::{self, Config};

use args::{NodeArgs, StationArgs};
use client::{messages, status};
use commands::{audio_devices, keygen, listen, load_config, node, send, trust_cmd, whoami, Prec};

#[derive(Parser)]
#[command(name = "hm", version, about = "hm-net: mail and chat over packet radio")]
struct Cli {
    /// Station settings and trusted stations.
    #[arg(long, global = true, default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Configure a new station interactively without overwriting existing files.
    Setup,
    /// Create a station key file, and a starter station.toml if there is none.
    Keygen {
        /// Your callsign. With an SSID (SA0KAM-2) the key is for that station only;
        /// without one it can run as any SSID set with `ssid` in station.toml.
        #[arg(long)]
        call: String,
        /// Where to write the key [default: `station.key` in station.toml].
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print the line other stations use to trust you (`hm trust add`).
    Whoami {
        /// Key file [default: `station.key` in station.toml].
        #[arg(long)]
        key: Option<PathBuf>,
    },
    /// Trusted stations: whose messages and receipts are verified.
    Trust {
        #[command(subcommand)]
        cmd: TrustCmd,
    },
    /// Send a message and wait until the receiving station confirms it.
    Send {
        #[command(flatten)]
        station: StationArgs,
        /// Destination station, e.g. SO5KM-1.
        #[arg(long)]
        to: String,
        /// Message text.
        #[arg(long)]
        text: String,
        /// Makes it mail instead of chat.
        #[arg(long)]
        subject: Option<String>,
        /// Handling precedence; higher goes first.
        #[arg(long, value_enum, default_value_t = Prec::Routine)]
        precedence: Prec,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Run the station: message store, radio and/or internet links, web page.
    Node {
        #[command(flatten)]
        station: StationArgs,
        #[command(flatten)]
        node: Box<NodeArgs>,
    },
    /// List the sound cards the built-in modem can use.
    AudioDevices,
    /// Print messages addressed to this station until interrupted.
    Listen {
        #[command(flatten)]
        station: StationArgs,
    },
    /// Show live node status (links, reachability) via the local web API.
    Status,
    /// List stored messages (chat, mail, bulletin) from the message store.
    Messages {
        /// in, out, relay, or all [default: in].
        #[arg(long, default_value = "in")]
        direction: String,
        /// chat, mail, bulletin, receipt, or all [default: human = chat+mail+bulletin].
        #[arg(long, default_value = "human")]
        kind: String,
        /// Max rows [default: 20].
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Subcommand)]
pub(crate) enum TrustCmd {
    /// Trust a station: give the line its `hm whoami` prints.
    Add {
        /// `CALL KEY`, e.g. "SO5KM-1 8a1e…".
        line: String,
        /// A note kept beside it, e.g. who it is.
        #[arg(long)]
        note: Option<String>,
    },
    /// Stop trusting a station.
    Remove { station: String },
    /// List trusted stations.
    List,
}

/// station.toml with this run's overrides applied.
fn with_overrides(
    path: &Path,
    station: &StationArgs,
    node: Option<&NodeArgs>,
) -> Result<(Config, Vec<String>), String> {
    let mut c = load_config(path)?;
    let mut o = Vec::new();
    station.apply(&mut c, &mut o);
    if let Some(n) = node {
        n.apply(&mut c, &mut o)?;
    }
    Ok((c, o))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let path = cli.config.as_path();
    let result = match &cli.cmd {
        Cmd::Setup => hm_cli::setup::run(path),
        Cmd::Keygen { call, out } => keygen(path, call, out.as_deref()),
        Cmd::Whoami { key } => whoami(path, key.as_deref()),
        Cmd::Trust { cmd } => trust_cmd(path, cmd),
        Cmd::Send {
            station,
            to,
            text,
            subject,
            precedence,
            timeout,
        } => with_overrides(path, station, None)
            .and_then(|(c, _)| send(&c, to, text, subject.as_deref(), *precedence, *timeout)),
        Cmd::Listen { station } => with_overrides(path, station, None).and_then(|(c, _)| listen(&c)),
        Cmd::Status => load_config(path).and_then(|c| status(&c)),
        Cmd::Messages {
            direction,
            kind,
            limit,
        } => load_config(path).and_then(|c| messages(&c, direction, kind, *limit)),
        Cmd::AudioDevices => audio_devices(),
        Cmd::Node { station, node: n } => {
            let (s2, n2) = (station.clone(), (**n).clone());
            let overrides: hm_cli::node::live::Overrides = std::sync::Arc::new(move |c: &mut Config| {
                let mut o = Vec::new();
                s2.apply(c, &mut o);
                let _ = n2.apply(c, &mut o);
            });
            with_overrides(path, station, Some(n.as_ref())).and_then(|(c, o)| node(path, &c, &o, overrides))
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hm: {e}");
            ExitCode::FAILURE
        }
    }
}

//! Runtime pieces for a real radio behind a KISS TNC (Direwolf, NinoTNC, ...).
//!
//! - [`kiss_link`]: a KISS TNC over TCP or a serial port, carrying hm frames inside AX.25 UI frames.
//! - [`sound_link`]: the built-in modem on a sound card, with PTT and CSMA.
//! - [`driver`]: runs any sans-IO [`hm_core::Machine`] in real time over a link.
//! - [`files`]: station key files and the trusted stations.
//! - [`station`]: build, send and receive bundles; used by the `hm` binary and tests.
//! - [`node`]: the station daemon: persistent store, radio, HTTP API and web page.
//!
//! This is the synchronous stand-in for `hm-node` so stations can go on air
//! early; the daemon replaces it later in Phase 1.

pub mod config;
pub mod driver;
pub mod files;
pub mod hex;
pub mod kiss_link;
pub mod node;
pub mod sound_link;
pub mod station;

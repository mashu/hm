//! Runtime pieces for a real radio behind a KISS TNC (Direwolf, NinoTNC, ...).
//!
//! - [`kiss_tcp`]: a KISS-over-TCP link carrying hm frames inside AX.25 UI frames.
//! - [`sound_link`]: the built-in modem on a sound card, with PTT and CSMA.
//! - [`driver`]: runs any sans-IO [`hm_core::Machine`] in real time over a link.
//! - [`files`]: station key files and trust files.
//! - [`station`]: build, send and receive bundles; used by the `hm` binary and tests.
//! - [`node`]: the station daemon: persistent store, radio, HTTP API and web page.
//!
//! This is the synchronous stand-in for `hm-node` so stations can go on air
//! early; the daemon replaces it later in Phase 1.

pub mod driver;
pub mod files;
pub mod hex;
pub mod kiss_tcp;
pub mod node;
pub mod sound_link;
pub mod station;

//! Transfers over fading HF paths: sizing the link, exploring parameters.

use super::*;

/// On a 300 bd HF path fading with a 0.5 Hz Doppler spread (CCIR 520
/// "moderate") at 20 dB, the link sizing of [`Config::hf_300`] (64-byte
/// symbols, overs up to 60 s) delivers everything, sooner and with less
/// airtime than the VHF 1200 sizes the daemon used at 300 bd before (200-byte
/// symbols).
#[test]
fn hf_link_sizing_on_a_fading_path() {
    let hf = |me: &str| Config::hf_300(call(me));
    let vhf_sized = |me: &str| {
        let mut c = Config::vhf_1200(call(me));
        c.bitrate_bps = 300;
        c.max_over = Millis::from_secs(120);
        c
    };
    let measure = |cfg: &dyn Fn(&str) -> Config| {
        let runs: Vec<_> = (0..20).map(|s| hf_run(s, cfg, 20.0, 0.5)).collect();
        let delivered = runs.iter().filter(|r| r.0).count();
        let mut latency: Vec<u64> = runs.iter().filter(|r| r.0).map(|r| r.1 .0).collect();
        latency.sort_unstable();
        let airtime = runs.iter().map(|r| r.2 .0).sum::<u64>() / runs.len() as u64;
        (delivered, latency[latency.len() / 2], airtime)
    };
    let (hf_ok, hf_p50, hf_air) = measure(&hf);
    let (vhf_ok, vhf_p50, vhf_air) = measure(&vhf_sized);
    eprintln!(
        "2 kB at 300 bd, 0.5 Hz fading, 20 dB: hf_300 {hf_ok}/20, p50 {:.0} s, airtime {:.0} s; \
         VHF-sized {vhf_ok}/20, p50 {:.0} s, airtime {:.0} s",
        hf_p50 as f64 / 1e3,
        hf_air as f64 / 1e3,
        vhf_p50 as f64 / 1e3,
        vhf_air as f64 / 1e3
    );
    assert_eq!(hf_ok, 20);
    assert!(hf_p50 < vhf_p50 && hf_air * 10 < vhf_air * 9);
}

#[test]
#[ignore = "exploration: HF link parameters on fading channels"]
fn explore_hf_parameters_on_fading() {
    for spread in [0.1, 0.5, 1.0] {
        for snr in [17.0, 20.0, 24.0] {
            for symbol in [64u16, 128, 200] {
                for over in [20u64, 60, 120] {
                    let cfg = |me: &str| {
                        let mut c = Config::hf_300(call(me));
                        c.symbol_size = symbol;
                        c.max_over = Millis::from_secs(over);
                        c
                    };
                    let runs: Vec<_> = (0..30).map(|s| hf_run(s, &cfg, snr, spread)).collect();
                    let ok: Vec<_> = runs.iter().filter(|r| r.0).collect();
                    let mut lat: Vec<u64> = ok.iter().map(|r| r.1 .0).collect();
                    lat.sort();
                    let air: u64 = runs.iter().map(|r| r.2 .0).sum::<u64>() / runs.len() as u64;
                    eprintln!(
                        "spread {spread:>3} Hz snr {snr} dB symbol {symbol:>3} over {over:>3} s: {:>2}/30, p50 {:>5.0} s, airtime {:>4.0} s",
                        ok.len(),
                        lat.get(lat.len() / 2).copied().unwrap_or(0) as f64 / 1e3,
                        air as f64 / 1e3
                    );
                }
            }
        }
    }
}

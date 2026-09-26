//! Parameter search against Direwolf's increasing-noise benchmark files.
//! `cargo test -p hm-modem-afsk --release --test tune -- --ignored --nocapture`

use std::process::Command;

use hm_modem_afsk::{Demodulator, DemodulatorConfig};

fn read_wav(path: &std::path::Path) -> Vec<f32> {
    let b = std::fs::read(path).unwrap();
    let data = &b[44..];
    data.as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c) as f32 / 32768.0)
        .collect()
}

#[test]
#[ignore]
fn search() {
    let dir = std::env::temp_dir();
    let mut files = Vec::new();
    for fs in [44_100u32, 48_000, 22_050, 11_025] {
        let path = dir.join(format!("tune-{fs}.wav"));
        if !path.exists() {
            Command::new("gen_packets")
                .args(["-n", "100", "-r", &fs.to_string(), "-o"])
                .arg(&path)
                .output()
                .unwrap();
        }
        let dw = String::from_utf8_lossy(&Command::new("atest").arg(&path).output().unwrap().stdout)
            .lines()
            .find_map(|l| {
                l.split_once(" packets decoded")
                    .and_then(|(n, _)| n.trim().parse::<usize>().ok())
            })
            .unwrap();
        files.push((fs, read_wav(&path), dw));
    }
    let dw_total: usize = files.iter().map(|f| f.2).sum();
    println!(
        "Direwolf total {dw_total}: {:?}",
        files.iter().map(|f| (f.0, f.2)).collect::<Vec<_>>()
    );
    let stage = std::env::var("HM_TUNE_STAGE").unwrap_or_else(|_| "prefilter".into());
    let mut grid: Vec<(f32, f32, f32, f32, usize)> = Vec::new();
    match stage.as_str() {
        "prefilter" => {
            for pre in [1.25f32, 2.0, 3.0] {
                for (lo, hi) in [(900.0f32, 2500.0f32), (700.0, 2700.0), (1000.0, 2400.0)] {
                    grid.push((pre, lo, hi, 1.0, 7));
                }
            }
        }
        spec => {
            // "window:PRE:LO:HI" then vary the correlator window and slicer count.
            let v: Vec<f32> = spec.split(':').skip(1).map(|x| x.parse().unwrap()).collect();
            for win in [1.1f32, 1.2, 1.3, 1.45, 1.6] {
                for sl in [7usize, 9] {
                    grid.push((v[0], v[1], v[2], win, sl));
                }
            }
        }
    }
    let mut results = Vec::new();
    for (pre_bits, lo, hi, win, slicers) in grid {
        let mut total = 0;
        let mut per = Vec::new();
        for (fs, audio, _) in &files {
            let mut cfg = DemodulatorConfig::new(*fs);
            cfg.prefilter_bits = pre_bits;
            cfg.prefilter_lo = lo;
            cfg.prefilter_hi = hi;
            cfg.window_bits = win;
            if slicers == 9 {
                cfg.slicer_gains = vec![0.4, 0.5, 0.63, 0.79, 1.0, 1.26, 1.58, 2.0, 2.5];
            }
            let mut d = Demodulator::new(cfg);
            let mut out = Vec::new();
            d.process(audio, &mut out);
            total += out.len();
            per.push(out.len());
        }
        results.push((total, pre_bits, lo, hi, win, slicers, per));
    }
    results.sort_by_key(|a| std::cmp::Reverse(a.0));
    for r in results.iter().take(12) {
        println!(
            "{:4} ({:.1}%) pre {} {}-{} win {} slicers {} per-rate {:?}",
            r.0,
            100.0 * r.0 as f64 / dw_total as f64,
            r.1,
            r.2,
            r.3,
            r.4,
            r.5,
            r.6
        );
    }
}

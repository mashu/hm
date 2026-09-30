//! Station clocks that drift and start late.

use super::*;

#[test]
fn clock_inverse_is_exact() {
    let mut rng = DetRng::from_seed(5);
    for _ in 0..20_000 {
        let c = Clock {
            offset: Millis(rng.below(100_000)),
            ppm: rng.below(20_001) as i32 - 10_000,
        };
        let local = Millis(rng.below(10_000_000_000));
        let g = c.global_for(local);
        assert!(c.local(g) >= local, "{c:?} {local:?} -> {g:?}");
        if g.0 > 0 {
            assert!(
                c.local(Millis(g.0 - 1)) < local,
                "{c:?} {local:?} -> {g:?} not minimal"
            );
        }
    }
}

#[test]
fn drifting_clocks_shift_timers() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let fast = sim.add_node(Beacon::periodic(1, 10, Millis(1000), 0, DetRng::from_seed(1)));
    let slow = sim.add_node(Beacon::periodic(2, 10, Millis(1000), 0, DetRng::from_seed(1)));
    sim.set_clock(
        fast,
        Clock {
            offset: Millis::ZERO,
            ppm: 1000,
        },
    );
    sim.set_clock(
        slow,
        Clock {
            offset: Millis::ZERO,
            ppm: -1000,
        },
    );
    sim.run_until(Millis::from_secs(1000));
    // 1000 global seconds are 1001 s on the fast clock and 999 s on the slow one.
    assert_eq!(sim.node(fast).sent(), 1001);
    assert_eq!(sim.node(slow).sent(), 999);
}

#[test]
fn clock_offset_delays_first_deadline() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::periodic(1, 10, Millis(5000), 0, DetRng::from_seed(1)));
    let b = sim.add_node(Beacon::silent(2, 10));
    sim.link(a, b, Loss::None);
    // Clock already reads 3 s at global 0, so the first beacon (local 5 s) goes at global 2 s.
    sim.set_clock(
        a,
        Clock {
            offset: Millis(3000),
            ppm: 0,
        },
    );
    sim.run_until(Millis(4000));
    let t = PLAIN.airtime(10);
    assert_eq!(heard_by(&sim, b), vec![(Millis(2000) + t, 0, 1, 0)]);
}

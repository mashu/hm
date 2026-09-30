//! When the station at the far end of a path will next be heard.
//!
//! On the radio, knowing whether a path is open costs nothing: a station
//! that hears its peer knows the path is open now. A station that waits to
//! hear its next hop before sending spends airtime only on a path it knows
//! is open, at the price of the wait. Deciding between sending now on the
//! forecast and sending on hearing needs the chance the peer is heard within
//! a while, and how long that takes.
//!
//! The peer transmits about every `interval` (its beacons), at a moment
//! unknown to us, so its transmissions fall at `(k − ½)·interval` on
//! average. Each is heard when the path is open then and the frame gets
//! through:
//!
//! ```text
//! P(heard at the k-th) = P(open at t_k) · (1 − ε)
//! P(first heard at the k-th) = P(heard at the k-th) · Π_{j<k} (1 − P(heard at the j-th))
//! ```
//!
//! (Successive transmissions are taken as independent chances; the open state
//! is correlated over the path's correlation time, so this is a little
//! optimistic when the path is closed now.)

/// The chance the peer is heard within a window, and the expected wait for
/// it when it is.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Hearing {
    pub chance: f64,
    pub wait_secs: u64,
}

/// The first hearing of a peer transmitting every `interval` seconds, within
/// `window` of `now`, when the path is open at `t` with chance `p_open(t)`
/// and a frame gets through an open path with chance `through`.
pub fn first_hearing(
    p_open: impl Fn(u64) -> f64,
    through: f64,
    now: u64,
    window: u64,
    interval: u64,
) -> Hearing {
    let interval = interval.max(1);
    let (mut unheard, mut chance, mut wait) = (1.0, 0.0, 0.0);
    let mut k = 1;
    loop {
        let offset = (2 * k - 1) * interval / 2;
        if offset > window {
            break;
        }
        let heard = (p_open(now + offset) * through).clamp(0.0, 1.0);
        let first = unheard * heard;
        chance += first;
        wait += first * offset as f64;
        unheard *= 1.0 - heard;
        k += 1;
    }
    Hearing {
        chance,
        wait_secs: if chance > 0.0 {
            libm::round(wait / chance) as u64
        } else {
            window
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_path_is_heard_soon_and_a_doubtful_one_later_or_not() {
        let open = first_hearing(|_| 1.0, 1.0, 0, 3_600, 600);
        assert_eq!(
            open,
            Hearing {
                chance: 1.0,
                wait_secs: 300
            }
        );
        let doubtful = first_hearing(|_| 0.3, 0.8, 0, 3_600, 600);
        assert!(doubtful.chance > 0.7 && doubtful.chance < 0.85, "{doubtful:?}");
        assert!(doubtful.wait_secs > 900, "{doubtful:?}");
        // Opening only later in the window: heard then, if at all.
        let later = first_hearing(|t| if t > 1_800 { 0.9 } else { 0.0 }, 1.0, 0, 3_600, 600);
        assert!(later.wait_secs > 1_800 && later.chance > 0.9, "{later:?}");
        assert_eq!(first_hearing(|_| 0.0, 1.0, 0, 3_600, 600).chance, 0.0);
    }
}

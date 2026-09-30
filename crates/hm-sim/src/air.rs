//! Frames on the air: channel access, carrier sense, and what each
//! listener makes of a transmission (delivered, lost to a fade, a collision,
//! its own transmitter).

use std::hash::Hash;

use hm_core::{Input, Machine, Millis, Output, Port};

use crate::fading::{lose, Fade};
use crate::{
    classify_hm_frame, Airtime, ChannelId, Ev, Fnv, FrameClass, LogEntry, Loss, NodeId, Outcome, Sim, Tx,
};

impl<M, C, E> Sim<M, C, E>
where
    M: Machine<Input = Input<C>, Output = Output<E>>,
    E: Hash,
{
    pub(crate) fn start_tx(&mut self, node: NodeId, port: Port, frame: Vec<u8>) {
        if !self.nodes[node].up {
            return;
        }
        let now = self.now;
        let r = match self.nodes[node].radios.get_mut(&port) {
            Some(r) => r,
            None => panic!("station {node} transmitted on port {port}, which has no radio"),
        };
        if r.csma.is_some() && r.free_at <= now {
            // Idle radio with channel access: wait for a clear channel. The
            // first attempt runs after the machine's other outputs, so a whole
            // over gathers into one key-up.
            r.waiting.push((frame, now));
            if !r.access_pending {
                r.access_pending = true;
                let gen = r.access_gen;
                self.push(now, Ev::Access { node, port, gen });
            }
            return;
        }
        self.put_on_air(node, port, frame, now);
    }

    /// One channel-access attempt for the frames waiting on a radio.
    pub(crate) fn access(&mut self, node: NodeId, port: Port, gen: u64) {
        let now = self.now;
        let n = &self.nodes[node];
        let r = &n.radios[&port];
        if !n.up || r.access_gen != gen || !r.access_pending {
            return;
        }
        // Channel access switched off meanwhile: send what is waiting now.
        if let Some(csma) = r.csma {
            let busy = self.carrier(node, r.channel, csma.dcd_delay);
            if busy || self.rng.below(256) > csma.persist as u64 {
                self.push(now + csma.slot, Ev::Access { node, port, gen });
                return;
            }
        }
        let r = self.nodes[node].radios.get_mut(&port).expect("checked");
        r.access_pending = false;
        for (frame, asked_at) in std::mem::take(&mut r.waiting) {
            self.put_on_air(node, port, frame, asked_at);
        }
    }

    /// Whether `node` detects a carrier on `ch`: another station it can hear
    /// has been keyed up for at least `dcd_delay`.
    fn carrier(&self, node: NodeId, ch: ChannelId, dcd_delay: Millis) -> bool {
        let now = self.now;
        self.txs.iter().any(|t| {
            t.channel == ch
                && t.from != node
                && t.start <= now
                && now < t.end
                && t.keyed_at + dcd_delay <= now
                && self.links.get(&(ch, t.from, node)).is_some_and(|l| l.enabled)
        })
    }

    /// Key up (or continue the key-up) and send `frame`.
    fn put_on_air(&mut self, node: NodeId, port: Port, frame: Vec<u8>, asked_at: Millis) {
        let ch = self.nodes[node].radios[&port].channel;
        let radio = self.channels[ch.0];
        let keyup = self.nodes[node].radios[&port].free_at <= self.now;
        let airtime = radio.airtime_of(&frame, keyup);
        let (header, class) = classify_hm_frame(&frame);
        let header = header.min(frame.len());
        let body_bits = (frame.len() - header) as u64 * 8;
        let split = Airtime {
            txdelay_us: if keyup {
                (radio.txdelay.0 + radio.txtail.0) * 1000
            } else {
                0
            },
            link_us: radio.bits_us(radio.frame_bits(&frame) - frame.len() as u64 * 8),
            overhead_us: radio.bits_us(header as u64 * 8),
            payload_us: if class == FrameClass::Payload {
                radio.bits_us(body_bits)
            } else {
                0
            },
            control_us: if class == FrameClass::Control {
                radio.bits_us(body_bits)
            } else {
                0
            },
            unknown_us: if class == FrameClass::Unknown {
                radio.bits_us(body_bits)
            } else {
                0
            },
        };

        let r = self.nodes[node].radios.get_mut(&port).expect("checked above");
        let start = if keyup { self.now } else { r.free_at };
        let end = start + airtime;
        r.free_at = end;
        if keyup {
            r.keyed_at = start;
        }
        let keyed_at = r.keyed_at;
        let n = &mut self.nodes[node];
        n.stats.frames_sent += 1;
        n.stats.bytes_sent += frame.len() as u64;
        n.stats.airtime_ms += airtime.0;
        let s = &mut self.channel_stats[ch.0];
        s.frames_sent += 1;
        s.bytes_sent += frame.len() as u64;
        s.airtime_ms += airtime.0;
        s.airtime.add(&split);
        self.max_airtime = self.max_airtime.max(airtime);
        self.seq += 1;
        let id = self.seq;
        self.txs.push(Tx {
            id,
            channel: ch,
            from: node,
            start,
            end,
            keyed_at,
        });
        let digest = Fnv::digest(&frame);
        let queued_at = self.now;
        self.record(LogEntry::Tx {
            id,
            channel: ch,
            from: node,
            port,
            asked_at,
            queued_at,
            start,
            end,
            keyup,
            len: frame.len(),
            digest,
            data: if self.log.is_some() {
                frame.clone()
            } else {
                Vec::new()
            },
        });
        self.push(
            end,
            Ev::TxEnd {
                id,
                channel: ch,
                from: node,
                start,
                frame,
            },
        );
    }

    pub(crate) fn finish_tx(&mut self, id: u64, ch: ChannelId, from: NodeId, start: Millis, frame: Vec<u8>) {
        let end = self.now;
        let overlaps = |t: &Tx| t.channel == ch && t.start < end && start < t.end;
        let receivers: Vec<NodeId> = self
            .links
            .range((ch, from, 0)..=(ch, from, usize::MAX))
            .filter(|(_, l)| l.enabled)
            .map(|(&(_, _, to), _)| to)
            .collect();
        let original = Fnv::digest(&frame);
        self.record(LogEntry::Eval { tx: id, at: end });
        for r in receivers {
            let port = self.nodes[r]
                .port_on(ch)
                .expect("linked stations have a radio on the channel");
            let outcome = if !self.nodes[r].up {
                Outcome::LostDown
            } else if self.txs.iter().any(|t| t.from == r && overlaps(t)) {
                Outcome::LostHalfDuplex
            } else if self.txs.iter().any(|t| {
                t.id != id
                    && t.from != from
                    && overlaps(t)
                    && self.links.get(&(ch, t.from, r)).is_some_and(|l| l.enabled)
            }) {
                Outcome::LostCollision
            } else {
                let now = self.now;
                let on_air = frame.len() + self.channels[ch.0].phy_overhead_bytes as usize;
                let link = self
                    .links
                    .get_mut(&(ch, from, r))
                    .expect("receiver comes from link table");
                let lost = match link.loss {
                    Loss::Fading {
                        curve,
                        mean_snr_db,
                        doppler_spread_hz,
                        rician_k,
                    } => {
                        let path = (ch, from.min(r), from.max(r));
                        if !self.fades.contains_key(&path) {
                            let fade = Fade::new(&mut self.rng, doppler_spread_hz);
                            self.fades.insert(path, fade);
                        }
                        let snr = self.fades[&path].weakest_snr_db(
                            (start, now),
                            doppler_spread_hz,
                            rician_k,
                            mean_snr_db,
                        );
                        self.rng.chance(curve.loss(snr, on_air))
                    }
                    _ => lose(&mut self.rng, link, now, on_air),
                };
                if lost {
                    Outcome::LostChannel
                } else if !frame.is_empty() && self.rng.chance(link.corrupt) {
                    Outcome::Corrupted
                } else {
                    Outcome::Delivered
                }
            };
            let mut data = frame.clone();
            if outcome == Outcome::Corrupted {
                // 1-3 distinct bits, so the frame always differs from what was sent.
                let bits = data.len() as u64 * 8;
                let flips = (1 + self.rng.below(3)).min(bits);
                let mut chosen: Vec<u64> = Vec::with_capacity(flips as usize);
                while (chosen.len() as u64) < flips {
                    let bit = self.rng.below(bits);
                    if !chosen.contains(&bit) {
                        chosen.push(bit);
                    }
                }
                for bit in chosen {
                    data[(bit / 8) as usize] ^= 1 << (bit % 8);
                }
            }
            let digest = if matches!(outcome, Outcome::Delivered | Outcome::Corrupted) {
                Fnv::digest(&data)
            } else {
                original
            };
            self.record(LogEntry::Rx {
                tx: id,
                channel: ch,
                to: r,
                port,
                at: self.now,
                outcome,
                digest,
            });
            let s = &mut self.channel_stats[ch.0];
            match outcome {
                Outcome::Delivered => s.delivered += 1,
                Outcome::Corrupted => s.corrupted += 1,
                Outcome::LostChannel => s.lost_channel += 1,
                Outcome::LostCollision => s.lost_collision += 1,
                Outcome::LostHalfDuplex => s.lost_half_duplex += 1,
                Outcome::LostDown => s.lost_down += 1,
            }
            if matches!(outcome, Outcome::Delivered | Outcome::Corrupted) {
                self.nodes[r].stats.frames_received += 1;
                (self.now, ch.0, from, r, port, 0xD1u8).hash(&mut self.trace);
                data.hash(&mut self.trace);
                let local = self.nodes[r].clock.local(self.now);
                let mut out = Vec::new();
                self.nodes[r]
                    .machine
                    .handle(local, Input::Frame { port, data }, &mut out);
                self.apply(r, out);
            }
        }
        self.drained(from, ch, end);
        let horizon = self.max_airtime;
        let now = self.now;
        self.txs.retain(|t| t.end + horizon > now);
    }
}

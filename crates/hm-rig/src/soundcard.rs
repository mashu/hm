//! Real sound cards through cpal (ALSA, CoreAudio, WASAPI).

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

use crate::AudioPort;

fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Names of the audio devices that can capture and play.
pub fn devices() -> io::Result<(Vec<String>, Vec<String>)> {
    let host = cpal::default_host();
    let name = |d: cpal::Device| {
        d.description()
            .map(|x| x.name().to_string())
            .unwrap_or_else(|_| "?".into())
    };
    let inputs = host.input_devices().map_err(err)?.map(name).collect();
    let outputs = host.output_devices().map_err(err)?.map(name).collect();
    Ok((inputs, outputs))
}

fn pick(input: bool, wanted: &str) -> io::Result<cpal::Device> {
    let host = cpal::default_host();
    if wanted == "default" {
        let d = if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        };
        return d.ok_or_else(|| {
            err(format!(
                "no default {} device",
                if input { "input" } else { "output" }
            ))
        });
    }
    let mut all = if input {
        host.input_devices().map_err(err)?
    } else {
        host.output_devices().map_err(err)?
    };
    all.find(|d| {
        d.description()
            .map(|x| x.name().contains(wanted))
            .unwrap_or(false)
    })
    .ok_or_else(|| {
        err(format!(
            "no audio device matching {wanted:?}; see `hm audio-devices`"
        ))
    })
}

/// Mono capture and playback on one device, at the input's native rate.
pub struct SoundCard {
    fs: u32,
    _input: cpal::Stream,
    _output: cpal::Stream,
    captured: Receiver<Vec<f32>>,
    queue: Arc<Mutex<VecDeque<f32>>>,
    pending: Arc<AtomicUsize>,
}

impl SoundCard {
    /// `device` is `default` or part of a device name.
    pub fn open(device: &str) -> io::Result<SoundCard> {
        let input = pick(true, device)?;
        let output = pick(false, device)?;
        let in_cfg = input.default_input_config().map_err(err)?;
        let fs = in_cfg.sample_rate();
        let in_channels = in_cfg.channels() as usize;
        let out_default = output.default_output_config().map_err(err)?;
        let out_channels = out_default.channels() as usize;
        let out_cfg = StreamConfig {
            channels: out_channels as u16,
            sample_rate: fs,
            buffer_size: cpal::BufferSize::Default,
        };

        let (tx, captured) = mpsc::channel();
        let on_err = |e| eprintln!("audio: {e}");
        let input_stream = match in_cfg.sample_format() {
            SampleFormat::F32 => input.build_input_stream(
                in_cfg.config(),
                move |data: &[f32], _| {
                    let _ = tx.send(data.iter().step_by(in_channels).copied().collect());
                },
                on_err,
                None,
            ),
            SampleFormat::I16 => input.build_input_stream(
                in_cfg.config(),
                move |data: &[i16], _| {
                    let _ = tx.send(
                        data.iter()
                            .step_by(in_channels)
                            .map(|&s| s as f32 / 32768.0)
                            .collect(),
                    );
                },
                on_err,
                None,
            ),
            f => return Err(err(format!("unsupported input sample format {f:?}"))),
        }
        .map_err(err)?;

        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        let pending = Arc::new(AtomicUsize::new(0));
        let (q, p) = (queue.clone(), pending.clone());
        let output_stream = match out_default.sample_format() {
            SampleFormat::F32 => output.build_output_stream(
                out_cfg,
                move |data: &mut [f32], _| {
                    let mut q = q.lock().expect("lock");
                    for frame in data.chunks_mut(out_channels) {
                        let v = q.pop_front().unwrap_or(0.0);
                        frame.fill(v);
                    }
                    p.store(q.len(), Ordering::Release);
                },
                on_err,
                None,
            ),
            SampleFormat::I16 => output.build_output_stream(
                out_cfg,
                move |data: &mut [i16], _| {
                    let mut q = q.lock().expect("lock");
                    for frame in data.chunks_mut(out_channels) {
                        let v = q.pop_front().unwrap_or(0.0);
                        frame.fill((v.clamp(-1.0, 1.0) * 32767.0) as i16);
                    }
                    p.store(q.len(), Ordering::Release);
                },
                on_err,
                None,
            ),
            f => return Err(err(format!("unsupported output sample format {f:?}"))),
        }
        .map_err(err)?;
        input_stream.play().map_err(err)?;
        output_stream.play().map_err(err)?;
        Ok(SoundCard {
            fs,
            _input: input_stream,
            _output: output_stream,
            captured,
            queue,
            pending,
        })
    }
}

impl AudioPort for SoundCard {
    fn sample_rate(&self) -> u32 {
        self.fs
    }

    fn capture(&mut self, out: &mut Vec<f32>, wait: Duration) -> io::Result<()> {
        match self.captured.recv_timeout(wait) {
            Ok(chunk) => out.extend(chunk),
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(err("audio input stopped")),
        }
        while let Ok(chunk) = self.captured.try_recv() {
            out.extend(chunk);
        }
        Ok(())
    }

    fn play(&mut self, samples: &[f32]) -> io::Result<()> {
        {
            let mut q = self.queue.lock().expect("lock");
            q.extend(samples.iter().copied());
            self.pending.store(q.len(), Ordering::Release);
        }
        let limit =
            std::time::Instant::now() + Duration::from_secs_f64(samples.len() as f64 / self.fs as f64 + 2.0);
        while self.pending.load(Ordering::Acquire) > 0 {
            if std::time::Instant::now() > limit {
                self.queue.lock().expect("lock").clear();
                return Err(err("audio output stalled"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Let the device's own buffer drain before PTT is released.
        std::thread::sleep(Duration::from_millis(60));
        Ok(())
    }
}

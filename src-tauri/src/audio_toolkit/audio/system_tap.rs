//! macOS system-audio capture via Core Audio process taps.
//!
//! Captures everything routed to the system output device (e.g. the other
//! participants of a Zoom/Meet call) without joining the call — the same
//! technique Granola-style apps use. Ported from anarlog (MIT,
//! crates/audio-actual/src/speaker/macos.rs) and simplified to Lark's
//! accumulate-then-stop recording model: the real-time callback ships raw
//! chunks over a channel, a consumer thread resamples to 16 kHz mono and
//! accumulates until `stop()`.
//!
//! Requires macOS 14.2+ and the NSAudioCaptureUsageDescription Info.plist
//! key (the first capture triggers a one-time "record system audio"
//! permission prompt).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::audio_toolkit::audio::FrameResampler;
use crate::audio_toolkit::constants;

use cidre::core_audio::aggregate_device_keys as agg_keys;
use cidre::{av, cat, cf, core_audio as ca, ns, os};

const TAP_DEVICE_NAME: &str = "lark-audio-tap";

/// Trim the rolling buffer back to the cap only once per this much overshoot
/// — see the matching constant in `recorder.rs`.
const CAP_SLACK_SAMPLES: usize = constants::WHISPER_SAMPLE_RATE as usize * 30;

struct Ctx {
    common_format: av::audio::CommonFormat,
    tx: mpsc::Sender<Vec<f32>>,
}

pub struct SystemAudioTap {
    tap: Option<ca::TapGuard>,
    device: Option<ca::hardware::StartedDevice<ca::AggregateDevice>>,
    // Box gives the IO proc a stable pointer; must outlive `device`.
    ctx: Option<Box<Ctx>>,
    consumer: Option<std::thread::JoinHandle<()>>,
    /// Shared with the consumer thread so the buffer can be capped, read or
    /// taken while capture continues.
    accumulated: Arc<Mutex<Vec<f32>>>,
    /// `usize::MAX` = keep everything (a real recording); smaller = keep only
    /// the most recent window (standby capture waiting to be promoted).
    sample_cap: Arc<AtomicUsize>,
}

// The tap/device guards are only touched from start()/stop(); Core Audio
// delivers callbacks on its own realtime thread via the raw Ctx pointer.
unsafe impl Send for SystemAudioTap {}

impl SystemAudioTap {
    /// Creates the process tap + private aggregate device and starts capture.
    pub fn start() -> Result<Self> {
        let tap_desc = ca::TapDesc::with_mono_global_tap_excluding_processes(&ns::Array::new());
        let tap = tap_desc
            .create_process_tap()
            .map_err(|e| anyhow!("failed to create system audio tap: {e:?}"))?;

        let asbd = tap
            .asbd()
            .map_err(|e| anyhow!("failed to read tap format: {e:?}"))?;
        let sample_rate = asbd.sample_rate as u32;
        let format = av::AudioFormat::with_asbd(&asbd)
            .ok_or_else(|| anyhow!("unsupported tap audio format"))?;
        let common_format = format.common_format();

        log::info!(
            "System tap created: {} Hz, format {:?}",
            sample_rate,
            common_format
        );

        let sub_tap = cf::DictionaryOf::with_keys_values(
            &[ca::sub_device_keys::uid()],
            &[tap.uid().unwrap().as_type_ref()],
        );

        let agg_desc = cf::DictionaryOf::with_keys_values(
            &[
                agg_keys::is_private(),
                agg_keys::tap_auto_start(),
                agg_keys::name(),
                agg_keys::uid(),
                agg_keys::tap_list(),
            ],
            &[
                cf::Boolean::value_true().as_type_ref(),
                cf::Boolean::value_false(),
                cf::String::from_str(TAP_DEVICE_NAME).as_ref(),
                &cf::Uuid::new().to_cf_string(),
                &cf::ArrayOf::from_slice(&[sub_tap.as_ref()]),
            ],
        );

        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let mut ctx = Box::new(Ctx { common_format, tx });

        extern "C" fn proc(
            _device: ca::Device,
            _now: &cat::AudioTimeStamp,
            input_data: &cat::AudioBufList<1>,
            _input_time: &cat::AudioTimeStamp,
            _output_data: &mut cat::AudioBufList<1>,
            _output_time: &cat::AudioTimeStamp,
            ctx: Option<&mut Ctx>,
        ) -> os::Status {
            let Some(ctx) = ctx else {
                return os::Status::NO_ERR;
            };

            let buffer = &input_data.buffers[0];
            if buffer.data_bytes_size == 0 || buffer.data.is_null() {
                return os::Status::NO_ERR;
            }

            let samples: Option<Vec<f32>> = match ctx.common_format {
                av::audio::CommonFormat::PcmF32 => read_samples::<f32>(buffer).map(|s| s.to_vec()),
                av::audio::CommonFormat::PcmF64 => {
                    read_samples::<f64>(buffer).map(|s| s.iter().map(|&v| v as f32).collect())
                }
                av::audio::CommonFormat::PcmI32 => read_samples::<i32>(buffer)
                    .map(|s| s.iter().map(|&v| v as f32 / 2_147_483_648.0).collect()),
                av::audio::CommonFormat::PcmI16 => read_samples::<i16>(buffer)
                    .map(|s| s.iter().map(|&v| v as f32 / 32_768.0).collect()),
                _ => None,
            };

            if let Some(samples) = samples {
                let _ = ctx.tx.send(samples);
            }

            os::Status::NO_ERR
        }

        let agg_device = ca::AggregateDevice::with_desc(&agg_desc)
            .map_err(|e| anyhow!("failed to create aggregate device: {e:?}"))?;
        let proc_id = agg_device
            .create_io_proc_id(proc, Some(&mut ctx))
            .map_err(|e| anyhow!("failed to register tap IO proc: {e:?}"))?;
        let device = ca::device_start(agg_device, Some(proc_id))
            .map_err(|e| anyhow!("failed to start tap device: {e:?}"))?;

        // Consumer: resample native-rate chunks to 16 kHz mono and accumulate.
        // Runs until every sender (the Ctx) is dropped in stop().
        let accumulated = Arc::new(Mutex::new(Vec::<f32>::new()));
        let sample_cap = Arc::new(AtomicUsize::new(usize::MAX));
        let sink = accumulated.clone();
        let cap = sample_cap.clone();
        let consumer = std::thread::spawn(move || {
            let mut resampler = FrameResampler::new(
                sample_rate as usize,
                constants::WHISPER_SAMPLE_RATE as usize,
                Duration::from_millis(30),
            );
            while let Ok(chunk) = rx.recv() {
                let mut buf = sink.lock().unwrap();
                resampler.push(&chunk, &mut |frame: &[f32]| buf.extend_from_slice(frame));
                let cap = cap.load(Ordering::Relaxed);
                if cap != usize::MAX {
                    // Reserve the whole window up front rather than letting
                    // the Vec double into it — see the matching comment in
                    // `recorder.rs`; `drain` never gives capacity back.
                    let ceiling = cap.saturating_add(CAP_SLACK_SAMPLES);
                    // Length read out first: through the mutex guard, calling
                    // `buf.len()` inside `buf.reserve_exact(..)` is a borrow
                    // of `buf` while it is already mutably borrowed.
                    let len = buf.len();
                    if buf.capacity() < ceiling && len < ceiling {
                        buf.reserve_exact(ceiling - len);
                    }
                    if buf.len() > ceiling {
                        let excess = buf.len() - cap;
                        buf.drain(..excess);
                    }
                }
            }
            let mut buf = sink.lock().unwrap();
            resampler.finish(&mut |frame: &[f32]| buf.extend_from_slice(frame));
        });

        Ok(Self {
            tap: Some(tap),
            device: Some(device),
            ctx: Some(ctx),
            consumer: Some(consumer),
            accumulated,
            sample_cap,
        })
    }

    /// Caps the buffer to the most recent `samples`, or removes the cap with
    /// `None`. Safe to change mid-capture.
    pub fn set_sample_cap(&self, samples: Option<usize>) {
        self.sample_cap
            .store(samples.unwrap_or(usize::MAX), Ordering::Relaxed);
    }

    /// Samples captured so far, without interrupting capture.
    pub fn buffered_len(&self) -> usize {
        self.accumulated.lock().unwrap().len()
    }

    /// Takes the captured audio and keeps capturing into a fresh buffer —
    /// the tap half of promoting a standby buffer into a real recording.
    pub fn take_buffer(&self) -> Vec<f32> {
        std::mem::take(&mut *self.accumulated.lock().unwrap())
    }

    /// Stops capture and returns the accumulated 16 kHz mono samples.
    pub fn stop(mut self) -> Result<Vec<f32>> {
        // Drop order matters: stop the device (no more callbacks), destroy the
        // tap, then drop Ctx so the channel closes and the consumer finishes.
        drop(self.device.take());
        drop(self.tap.take());
        drop(self.ctx.take());

        let consumer = self
            .consumer
            .take()
            .ok_or_else(|| anyhow!("system tap already stopped"))?;
        consumer
            .join()
            .map_err(|_| anyhow!("system tap consumer thread panicked"))?;
        Ok(std::mem::take(&mut *self.accumulated.lock().unwrap()))
    }
}

fn read_samples<T: Copy>(buffer: &cat::AudioBuf) -> Option<&[T]> {
    let byte_count = buffer.data_bytes_size as usize;
    if byte_count == 0 || buffer.data.is_null() {
        return None;
    }

    let data = buffer.data as *const T;
    if (data as usize) % std::mem::align_of::<T>() != 0 {
        return None;
    }

    let sample_count = byte_count / std::mem::size_of::<T>();
    if sample_count == 0 {
        return None;
    }

    Some(unsafe { std::slice::from_raw_parts(data, sample_count) })
}

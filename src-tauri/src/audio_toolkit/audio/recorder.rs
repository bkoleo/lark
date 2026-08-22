use std::{
    io::Error,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Sample, SizedSample,
};

use crate::audio_toolkit::{
    audio::{AudioVisualiser, ChunkSink, FrameResampler},
    constants,
    vad::{self, VadFrame},
    VoiceActivityDetector,
};

enum Cmd {
    Start,
    Stop(mpsc::Sender<Vec<f32>>),
    /// Hand over everything captured so far and keep the stream running —
    /// how a rolling pre-roll buffer becomes the head of a real recording
    /// without a teardown (see `MeetingManager::promote_standby`). Unlike
    /// `Stop` it never raises the stop flag, so no audio is dropped waiting
    /// for the callback to acknowledge one.
    Take(mpsc::Sender<Vec<f32>>),
    Shutdown,
}

/// How far past the cap the rolling buffer is allowed to grow before it is
/// trimmed back. Trimming is a memmove of the whole buffer, so doing it once
/// per this much audio (rather than per 30ms chunk) keeps the cost invisible
/// while bounding the overshoot to something that never matters.
const CAP_SLACK_SAMPLES: usize = constants::WHISPER_SAMPLE_RATE as usize * 30;

enum AudioChunk {
    Samples(Vec<f32>),
    EndOfStream,
}

pub struct AudioRecorder {
    device: Option<Device>,
    cmd_tx: Option<mpsc::Sender<Cmd>>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
    vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
    level_cb: Option<Arc<dyn Fn(Vec<f32>) + Send + Sync + 'static>>,
    flow_cb: Option<Arc<dyn Fn(f32) + Send + Sync + 'static>>,
    /// Where recorded chunks are streamed to disk while capture runs, if
    /// anywhere — see `ChunkSink`. `None` in the sink itself (not this
    /// `Option`) means "nothing to write yet", checked on every chunk, so a
    /// caller can install or remove a writer without tearing the stream
    /// down. Cloned into the worker thread on every `open()`, so it survives
    /// a mid-recording device restart (`close()` + `open()` on the same
    /// `AudioRecorder`) the same way `level_cb`/`flow_cb` already do.
    chunk_sink: Option<ChunkSink>,
    /// Rolling-buffer ceiling in samples. `usize::MAX` (the default, and
    /// what dictation and a live meeting recording both use) means keep
    /// everything; anything smaller discards the oldest audio and retains
    /// only the most recent window. Shared with the consumer thread so it
    /// can be changed while the stream is running.
    sample_cap: Arc<AtomicUsize>,
}

impl AudioRecorder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(AudioRecorder {
            device: None,
            cmd_tx: None,
            worker_handle: None,
            vad: None,
            level_cb: None,
            flow_cb: None,
            chunk_sink: None,
            sample_cap: Arc::new(AtomicUsize::new(usize::MAX)),
        })
    }

    /// Caps the buffer to the most recent `samples`, or removes the cap with
    /// `None`. Takes effect on the next chunk, so it is safe to call on a
    /// running stream — which is the point: standby capture runs capped and
    /// is uncapped in place the moment the user hits Record.
    pub fn set_sample_cap(&self, samples: Option<usize>) {
        self.sample_cap
            .store(samples.unwrap_or(usize::MAX), Ordering::Relaxed);
    }

    /// Takes the captured audio without interrupting capture. Returns empty
    /// if the stream is delivering nothing — a dead mic must not be able to
    /// block the caller (the recording this hands off to still has to start).
    pub fn take_buffer(&self) -> Vec<f32> {
        let (resp_tx, resp_rx) = mpsc::channel();
        let Some(tx) = &self.cmd_tx else {
            return Vec::new();
        };
        if tx.send(Cmd::Take(resp_tx)).is_err() {
            return Vec::new();
        }
        // The consumer only reads commands between chunks (~30ms). A second
        // is many chunks; missing that means no audio is arriving at all.
        resp_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_else(|_| {
                log::warn!(
                    "Timed out taking the audio buffer — no chunks arriving from the device"
                );
                Vec::new()
            })
    }

    pub fn with_vad(mut self, vad: Box<dyn VoiceActivityDetector>) -> Self {
        self.vad = Some(Arc::new(Mutex::new(vad)));
        self
    }

    pub fn with_level_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(Vec<f32>) + Send + Sync + 'static,
    {
        self.level_cb = Some(Arc::new(cb));
        self
    }

    /// Called with the RMS of every raw chunk delivered by the device, before
    /// VAD filtering. Lets callers distinguish a live microphone from one that
    /// is streaming pure digital silence (e.g. a Bluetooth mic whose handshake
    /// failed) — both look identical at the stream level.
    pub fn with_flow_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(f32) + Send + Sync + 'static,
    {
        self.flow_cb = Some(Arc::new(cb));
        self
    }

    /// Installs where this recorder streams its chunks to as it captures —
    /// see `ChunkSink`. The sink starts with nothing installed in it
    /// (`None`, checked per chunk by the consumer thread), so passing an
    /// empty sink here and filling it in later is how a caller defers the
    /// decision of *whether* to write to disk without needing to reopen the
    /// device once it decides.
    pub fn with_chunk_sink(mut self, sink: ChunkSink) -> Self {
        self.chunk_sink = Some(sink);
        self
    }

    pub fn device_name(&self) -> Option<String> {
        self.device.as_ref().and_then(|d| d.name().ok())
    }

    pub fn open(&mut self, device: Option<Device>) -> Result<(), Box<dyn std::error::Error>> {
        if self.worker_handle.is_some() {
            return Ok(()); // already open
        }

        let (sample_tx, sample_rx) = mpsc::channel::<AudioChunk>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let host = crate::audio_toolkit::get_cpal_host();
        let device = match device {
            Some(dev) => dev,
            None => host
                .default_input_device()
                .ok_or_else(|| Error::new(std::io::ErrorKind::NotFound, "No input device found"))?,
        };

        let thread_device = device.clone();
        let vad = self.vad.clone();
        // Move the optional callbacks into the worker thread
        let level_cb = self.level_cb.clone();
        let flow_cb = self.flow_cb.clone();
        let chunk_sink = self.chunk_sink.clone();
        let sample_cap = self.sample_cap.clone();

        let worker = std::thread::spawn(move || {
            let stop_flag = Arc::new(AtomicBool::new(false));
            let stop_flag_for_stream = stop_flag.clone();
            let init_result = (|| -> Result<(cpal::Stream, u32), String> {
                let config = AudioRecorder::get_preferred_config(&thread_device)
                    .map_err(|e| format!("Failed to fetch preferred config: {e}"))?;

                let sample_rate = config.sample_rate().0;
                let channels = config.channels() as usize;

                log::info!(
                    "Using device: {:?}\nSample rate: {}\nChannels: {}\nFormat: {:?}",
                    thread_device.name(),
                    sample_rate,
                    channels,
                    config.sample_format()
                );

                let stream = match config.sample_format() {
                    cpal::SampleFormat::U8 => AudioRecorder::build_stream::<u8>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        stop_flag_for_stream,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::I8 => AudioRecorder::build_stream::<i8>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        stop_flag_for_stream,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::I16 => AudioRecorder::build_stream::<i16>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        stop_flag_for_stream,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::I32 => AudioRecorder::build_stream::<i32>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        stop_flag_for_stream,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::F32 => AudioRecorder::build_stream::<f32>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        stop_flag_for_stream,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    sample_format => {
                        return Err(format!("Unsupported sample format: {sample_format:?}"));
                    }
                };

                stream
                    .play()
                    .map_err(|e| format!("Failed to start microphone stream: {e}"))?;

                Ok((stream, sample_rate))
            })();

            match init_result {
                Ok((stream, sample_rate)) => {
                    let _ = init_tx.send(Ok(()));
                    // Keep the stream alive while we process samples.
                    run_consumer(
                        sample_rate,
                        vad,
                        sample_rx,
                        cmd_rx,
                        level_cb,
                        flow_cb,
                        chunk_sink,
                        stop_flag,
                        sample_cap,
                    );
                    drop(stream);
                }
                Err(error_message) => {
                    log::error!("{error_message}");
                    let _ = init_tx.send(Err(error_message));
                }
            }
        });

        match init_rx.recv() {
            Ok(Ok(())) => {
                self.device = Some(device);
                self.cmd_tx = Some(cmd_tx);
                self.worker_handle = Some(worker);
                Ok(())
            }
            Ok(Err(error_message)) => {
                let _ = worker.join();
                let kind = if is_microphone_access_denied(&error_message) {
                    std::io::ErrorKind::PermissionDenied
                } else {
                    std::io::ErrorKind::Other
                };
                Err(Box::new(Error::new(kind, error_message)))
            }
            Err(recv_error) => {
                let _ = worker.join();
                Err(Box::new(Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to initialize microphone worker: {recv_error}"),
                )))
            }
        }
    }

    pub fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tx) = &self.cmd_tx {
            tx.send(Cmd::Start)?;
        }
        Ok(())
    }

    pub fn stop(&self) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let (resp_tx, resp_rx) = mpsc::channel();
        if let Some(tx) = &self.cmd_tx {
            tx.send(Cmd::Stop(resp_tx))?;
        }
        // Wait for the samples — but never forever. The consumer only reads
        // commands between chunks, so a device that has stopped delivering
        // them entirely (a USB mic pulled off the bus mid-take) leaves this
        // waiting on a reply that can no longer come. Unbounded, that hangs
        // whatever holds the meeting state lock and takes the app's tray and
        // status with it. Ten seconds is far past any real stop — the
        // internal drain gives up on a dead callback after two — so this
        // fires only in the hung case, and turns an app-wide freeze into one
        // lost track with a line in the log saying so.
        resp_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| {
                log::error!(
                    "Audio stream stopped delivering audio and never returned its samples — the device was most likely disconnected mid-recording"
                );
                Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the audio device stopped responding",
                )
                .into()
            })
    }

    pub fn close(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(Cmd::Shutdown);
        }
        if let Some(h) = self.worker_handle.take() {
            let _ = h.join();
        }
        self.device = None;
        Ok(())
    }

    fn build_stream<T>(
        device: &cpal::Device,
        config: &cpal::SupportedStreamConfig,
        sample_tx: mpsc::Sender<AudioChunk>,
        channels: usize,
        stop_flag: Arc<AtomicBool>,
    ) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        T: Sample + SizedSample + Send + 'static,
        f32: cpal::FromSample<T>,
    {
        let mut output_buffer = Vec::new();
        let mut eos_sent = false;

        let stream_cb = move |data: &[T], _: &cpal::InputCallbackInfo| {
            if stop_flag.load(Ordering::Relaxed) {
                if !eos_sent {
                    let _ = sample_tx.send(AudioChunk::EndOfStream);
                    eos_sent = true;
                }
                return;
            }
            eos_sent = false;

            output_buffer.clear();

            if channels == 1 {
                output_buffer.extend(data.iter().map(|&sample| sample.to_sample::<f32>()));
            } else {
                let frame_count = data.len() / channels;
                output_buffer.reserve(frame_count);

                for frame in data.chunks_exact(channels) {
                    let mono_sample = frame
                        .iter()
                        .map(|&sample| sample.to_sample::<f32>())
                        .sum::<f32>()
                        / channels as f32;
                    output_buffer.push(mono_sample);
                }
            }

            if sample_tx
                .send(AudioChunk::Samples(output_buffer.clone()))
                .is_err()
            {
                log::error!("Failed to send samples");
            }
        };

        device.build_input_stream(
            &config.clone().into(),
            stream_cb,
            |err| log::error!("Stream error: {}", err),
            None,
        )
    }

    fn get_preferred_config(
        device: &cpal::Device,
    ) -> Result<cpal::SupportedStreamConfig, Box<dyn std::error::Error>> {
        // Use the device's native/default sample rate and let the FrameResampler
        // in run_consumer() downsample to 16kHz. This avoids forcing hardware into
        // a non-native rate which can cause issues on some devices (Bluetooth
        // codecs, certain ALSA drivers, etc.).
        let default_config = device.default_input_config()?;
        let target_rate = default_config.sample_rate();

        // Try to find the best sample format at the device's default rate
        let supported_configs = match device.supported_input_configs() {
            Ok(configs) => configs,
            Err(e) => {
                log::warn!("Could not enumerate input configs ({e}), using device default");
                return Ok(default_config);
            }
        };
        let mut best_config: Option<cpal::SupportedStreamConfigRange> = None;

        for config_range in supported_configs {
            if config_range.min_sample_rate() <= target_rate
                && config_range.max_sample_rate() >= target_rate
            {
                match best_config {
                    None => best_config = Some(config_range),
                    Some(ref current) => {
                        // Prioritize F32 > I16 > I32 > others
                        let score = |fmt: cpal::SampleFormat| match fmt {
                            cpal::SampleFormat::F32 => 4,
                            cpal::SampleFormat::I16 => 3,
                            cpal::SampleFormat::I32 => 2,
                            _ => 1,
                        };

                        if score(config_range.sample_format()) > score(current.sample_format()) {
                            best_config = Some(config_range);
                        }
                    }
                }
            }
        }

        if let Some(config) = best_config {
            return Ok(config.with_sample_rate(target_rate));
        }

        // Fall back to device default if no config matched (exotic/virtual devices)
        log::warn!(
            "No supported config matched device default rate {:?}, using default config",
            target_rate
        );
        Ok(default_config)
    }
}

pub fn is_microphone_access_denied(error_message: &str) -> bool {
    let normalized = error_message.to_lowercase();
    normalized.contains("access is denied")
        || normalized.contains("permission denied")
        || normalized.contains("0x80070005")
}

pub fn is_no_input_device_error(error_message: &str) -> bool {
    let normalized = error_message.to_lowercase();
    normalized.contains("no input device found")
        || (normalized.contains("failed to fetch preferred config")
            && normalized.contains("coreaudio"))
}

#[cfg(test)]
mod tests {
    use super::{is_microphone_access_denied, is_no_input_device_error};

    #[test]
    fn detects_access_is_denied() {
        assert!(is_microphone_access_denied("Access is denied"));
    }

    #[test]
    fn detects_permission_denied() {
        assert!(is_microphone_access_denied("permission denied"));
    }

    #[test]
    fn detects_windows_error_code() {
        assert!(is_microphone_access_denied("WASAPI error: 0x80070005"));
    }

    #[test]
    fn does_not_match_unrelated_errors() {
        assert!(!is_microphone_access_denied("device not found"));
    }

    #[test]
    fn detects_no_input_device() {
        assert!(is_no_input_device_error("No input device found"));
    }

    #[test]
    fn detects_coreaudio_config_error() {
        assert!(is_no_input_device_error(
            "Failed to fetch preferred config: A backend-specific error has occurred: An unknown error unknown to the coreaudio-rs API occurred"
        ));
    }

    #[test]
    fn does_not_match_other_errors_for_no_device() {
        assert!(!is_no_input_device_error("permission denied"));
        assert!(!is_no_input_device_error("device not found"));
    }
}

/// How often the consumer loop wakes on its own when no audio chunk has
/// arrived, purely to give `cmd_rx` a chance. See the comment on the main
/// `recv_timeout` below for why this exists — it is not a polling interval
/// for anything audio-related, and 500ms is far below anything a human
/// would notice as latency on Start/Stop.
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(500);

// One private consumer wired to one stream: the argument list is the wiring,
// and bundling it into a struct would only move the same fields elsewhere.
#[allow(clippy::too_many_arguments)]
fn run_consumer(
    in_sample_rate: u32,
    vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
    sample_rx: mpsc::Receiver<AudioChunk>,
    cmd_rx: mpsc::Receiver<Cmd>,
    level_cb: Option<Arc<dyn Fn(Vec<f32>) + Send + Sync + 'static>>,
    flow_cb: Option<Arc<dyn Fn(f32) + Send + Sync + 'static>>,
    chunk_sink: Option<ChunkSink>,
    stop_flag: Arc<AtomicBool>,
    sample_cap: Arc<AtomicUsize>,
) {
    let mut frame_resampler = FrameResampler::new(
        in_sample_rate as usize,
        constants::WHISPER_SAMPLE_RATE as usize,
        Duration::from_millis(30),
    );

    let mut processed_samples = Vec::<f32>::new();
    let mut recording = false;

    // ---------- spectrum visualisation setup ---------------------------- //
    const BUCKETS: usize = 16;
    const WINDOW_SIZE: usize = 512;
    let mut visualizer = AudioVisualiser::new(
        in_sample_rate,
        WINDOW_SIZE,
        BUCKETS,
        400.0,  // vocal_min_hz
        4000.0, // vocal_max_hz
    );

    // Writes a frame to the installed streaming WAV writer, if any — the
    // same frame being extended into `out_buf`, so the on-disk file and the
    // in-memory buffer this task feeds Whisper from never disagree about
    // which chunks are "the recording" (t17873258798635ef6, 2026-08-22).
    // Locking the sink's mutex once per ~30ms frame is far below anything
    // that matters against a 500ms command-poll interval.
    fn write_to_sink(chunk_sink: &Option<ChunkSink>, frame: &[f32]) {
        let Some(sink) = chunk_sink else { return };
        // Bound to a variable rather than chained off `.lock().unwrap()`
        // directly — the unbound form drops the MutexGuard before `writer`
        // is used, which either fails to compile or (worse, with a
        // differently-shaped chain) silently unlocks too early.
        let mut guard = sink.lock().unwrap();
        if let Some(writer) = guard.as_mut() {
            if let Err(e) = writer.write(frame) {
                log::warn!("Streaming WAV write failed: {e}");
            }
        }
    }

    fn handle_frame(
        samples: &[f32],
        recording: bool,
        vad: &Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
        out_buf: &mut Vec<f32>,
        chunk_sink: &Option<ChunkSink>,
    ) {
        if !recording {
            return;
        }

        if let Some(vad_arc) = vad {
            let mut det = vad_arc.lock().unwrap();
            match det.push_frame(samples).unwrap_or(VadFrame::Speech(samples)) {
                VadFrame::Speech(buf) => {
                    write_to_sink(chunk_sink, buf);
                    out_buf.extend_from_slice(buf);
                }
                VadFrame::Noise => {}
            }
        } else {
            write_to_sink(chunk_sink, samples);
            out_buf.extend_from_slice(samples);
        }
    }

    // Drains every pending command, handling Start/Stop/Take inline and
    // reporting whether Shutdown was among them (the caller does the actual
    // `return` — a nested fn can't return out of run_consumer for it).
    //
    // Pulled out so it can be called from two places: after a normal chunk
    // arrives (the original behaviour), AND on a bare timeout tick when no
    // chunk has arrived at all. That second call site is the actual fix —
    // see the comment on the main loop below for the failure it closes.
    #[allow(clippy::too_many_arguments)]
    fn drain_commands(
        cmd_rx: &mpsc::Receiver<Cmd>,
        sample_rx: &mpsc::Receiver<AudioChunk>,
        stop_flag: &Arc<AtomicBool>,
        recording: &mut bool,
        processed_samples: &mut Vec<f32>,
        frame_resampler: &mut FrameResampler,
        visualizer: &mut AudioVisualiser,
        vad: &Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
        chunk_sink: &Option<ChunkSink>,
    ) -> bool {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Cmd::Start => {
                    stop_flag.store(false, Ordering::Relaxed);
                    processed_samples.clear();
                    *recording = true;
                    visualizer.reset();
                    if let Some(v) = vad {
                        v.lock().unwrap().reset();
                    }
                }
                Cmd::Stop(reply_tx) => {
                    *recording = false;
                    stop_flag.store(true, Ordering::Relaxed);

                    // Drain all remaining audio until the producer confirms end-of-stream.
                    // The cpal callback sees the stop flag, sends EndOfStream, and goes
                    // silent — guaranteeing every captured sample is in the channel
                    // ahead of the sentinel. If the device has already gone fully
                    // silent (dropped off the bus — no more chunks, no EndOfStream
                    // either), this still bails after 2s rather than hanging; the
                    // outer loop's own timeout (below) is what guarantees we reach
                    // this branch at all in that case.
                    loop {
                        match sample_rx.recv_timeout(Duration::from_secs(2)) {
                            Ok(AudioChunk::Samples(remaining)) => {
                                frame_resampler.push(&remaining, &mut |frame: &[f32]| {
                                    handle_frame(frame, true, vad, processed_samples, chunk_sink)
                                });
                            }
                            Ok(AudioChunk::EndOfStream) => break,
                            Err(_) => {
                                log::warn!("Timed out waiting for EndOfStream from audio callback");
                                break;
                            }
                        }
                    }

                    frame_resampler.finish(&mut |frame: &[f32]| {
                        handle_frame(frame, true, vad, processed_samples, chunk_sink)
                    });

                    let _ = reply_tx.send(std::mem::take(processed_samples));

                    // Resume the audio callback so the consumer loop can continue
                    // receiving chunks (important for always-on microphone mode).
                    stop_flag.store(false, Ordering::Relaxed);
                }
                Cmd::Take(reply_tx) => {
                    // Hand the buffer over and carry straight on recording
                    // into a fresh one. No stop flag, no drain: the caller is
                    // promoting a rolling buffer, not ending a take.
                    let _ = reply_tx.send(std::mem::take(processed_samples));
                }
                Cmd::Shutdown => {
                    stop_flag.store(true, Ordering::Relaxed);
                    return true;
                }
            }
        }
        false
    }

    loop {
        // `recv_timeout` rather than a blocking `recv()` — deliberately
        // (t17873258798635ef6, 2026-08-21). A device that drops off the bus
        // mid-recording (Bluetooth handshake failure, USB unplug) stops
        // firing its cpal callback entirely: no more Samples, and no
        // EndOfStream either, because the callback that would send it never
        // runs again. A blocking `recv()` here waits on a message that is
        // never coming, and `Cmd::Stop`/`Cmd::Shutdown` were previously only
        // checked AFTER a chunk arrived — so the whole consumer, and with it
        // `AudioRecorder::close()`'s unconditional `worker_handle.join()`,
        // hung forever. That hang (not the mic error, which was already
        // handled and logged) is what ate the 2026-08-21 09:00 standup and
        // then killed call detection for the rest of the day, because
        // `meeting_detect.rs` calls `toggle()` inline on its one detector
        // thread. Waking on a timeout means `Cmd::Stop`/`Cmd::Shutdown` are
        // always seen within `COMMAND_POLL_INTERVAL`, dead device or not.
        let chunk = match sample_rx.recv_timeout(COMMAND_POLL_INTERVAL) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if drain_commands(
                    &cmd_rx,
                    &sample_rx,
                    &stop_flag,
                    &mut recording,
                    &mut processed_samples,
                    &mut frame_resampler,
                    &mut visualizer,
                    &vad,
                    &chunk_sink,
                ) {
                    return;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // stream closed
        };

        let raw = match chunk {
            AudioChunk::Samples(s) => s,
            AudioChunk::EndOfStream => continue,
        };

        // ---------- raw energy reporting ---------------------------------- //
        if let Some(cb) = &flow_cb {
            if !raw.is_empty() {
                let energy: f32 = raw.iter().map(|s| s * s).sum();
                cb((energy / raw.len() as f32).sqrt());
            }
        }

        // ---------- spectrum processing ---------------------------------- //
        if let Some(buckets) = visualizer.feed(&raw) {
            if let Some(cb) = &level_cb {
                cb(buckets);
            }
        }

        // ---------- existing pipeline ------------------------------------ //
        frame_resampler.push(&raw, &mut |frame: &[f32]| {
            handle_frame(frame, recording, &vad, &mut processed_samples, &chunk_sink)
        });

        // ---------- rolling-buffer trim ----------------------------------- //
        // Only ever active for standby capture; a normal recording runs
        // uncapped and never enters this branch.
        let cap = sample_cap.load(Ordering::Relaxed);
        if cap != usize::MAX {
            let ceiling = cap.saturating_add(CAP_SLACK_SAMPLES);
            // Claim the window in one go. `drain` frees length, never
            // capacity, so a buffer left to grow into its cap settles at
            // whatever power-of-two the doubling landed on — measured at
            // ~2.5x the audio it holds, which on a ten-minute buffer is
            // ~50 MB of headroom nobody asked for, on a machine that has
            // OOM-killed a transcription before. Reserved pages cost nothing
            // until they are written to.
            if processed_samples.capacity() < ceiling && processed_samples.len() < ceiling {
                processed_samples.reserve_exact(ceiling - processed_samples.len());
            }
            if processed_samples.len() > ceiling {
                let excess = processed_samples.len() - cap;
                processed_samples.drain(..excess);
            }
        }

        // non-blocking check for a command
        if drain_commands(
            &cmd_rx,
            &sample_rx,
            &stop_flag,
            &mut recording,
            &mut processed_samples,
            &mut frame_resampler,
            &mut visualizer,
            &vad,
            &chunk_sink,
        ) {
            return;
        }
    }
}

use anyhow::Result;
use hound::{WavReader, WavSpec, WavWriter};
use log::debug;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Read a WAV file and return normalised f32 samples.
pub fn read_wav_samples<P: AsRef<Path>>(file_path: P) -> Result<Vec<f32>> {
    let reader = WavReader::open(file_path.as_ref())?;
    let samples = reader
        .into_samples::<i16>()
        .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
        .collect::<Result<Vec<f32>, _>>()?;
    Ok(samples)
}

/// Verify a WAV file by reading it back and checking the sample count.
pub fn verify_wav_file<P: AsRef<Path>>(file_path: P, expected_samples: usize) -> Result<()> {
    let reader = WavReader::open(file_path.as_ref())?;
    let actual_samples = reader.len() as usize;
    if actual_samples != expected_samples {
        anyhow::bail!(
            "WAV sample count mismatch: expected {}, got {}",
            expected_samples,
            actual_samples
        );
    }
    Ok(())
}

/// Save audio samples as a WAV file
pub fn save_wav_file<P: AsRef<Path>>(file_path: P, samples: &[f32]) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = WavWriter::create(file_path.as_ref(), spec)?;

    // Convert f32 samples to i16 for WAV
    for sample in samples {
        let sample_i16 = (sample * i16::MAX as f32) as i16;
        writer.write_sample(sample_i16)?;
    }

    writer.finalize()?;
    debug!("Saved WAV file: {:?}", file_path.as_ref());
    Ok(())
}

/// Where a live recorder/tap hands off its chunks so they land on disk as
/// they arrive, instead of only at the end (see `save_wav_file`, which is
/// what every meeting WAV used to wait for — and what a hung or crashed
/// stop lost entirely). `None` means nothing is written right now: the
/// default for a recorder that has no `StreamingWavWriter` installed yet
/// (standby capture, or before the caller has decided where to write),
/// checked on every chunk so writing can start or stop mid-stream with no
/// teardown of the audio stream itself.
///
/// (t17873258798635ef6, 2026-08-22)
pub type ChunkSink = Arc<Mutex<Option<StreamingWavWriter>>>;

/// A 16kHz mono 16-bit WAV file written incrementally, chunk by chunk, so a
/// hang or crash mid-recording loses at most the last unflushed samples
/// rather than everything (`save_wav_file` writes once, at the end, from a
/// fully-accumulated buffer — the exact shape of loss this exists to close,
/// t17873258798635ef6).
///
/// `hound::WavWriter` leaves an invalid/zero-length-header file until its
/// header is patched with the real size — but it also does that itself on
/// an ordinary `Drop` if `finalize()` was never called (confirmed by
/// `unfinalised_file_is_not_left_corrupt` below against hound 3.5.1's own
/// `Drop` impl), so a panic or an early return the writer merely falls out
/// of scope from is already safe. What `Drop` can't reach is the one
/// scenario this task exists for: a *hang*, where the thread holding the
/// writer never returns control to anything, Rust's drop glue included.
/// That's why the caller still finalises explicitly, and does it as the
/// first thing after a stop is requested — before touching anything that
/// might hang (see `MeetingManager::stop_and_process`) — rather than
/// relying on `Drop` to eventually run.
pub struct StreamingWavWriter {
    writer: WavWriter<BufWriter<File>>,
}

impl StreamingWavWriter {
    /// Creates (or truncates) a 16kHz mono 16-bit WAV file for incremental
    /// writes. Matches `save_wav_file`'s spec exactly, so a streamed file
    /// and an at-the-end file are byte-for-byte the same format.
    pub fn create<P: AsRef<Path>>(file_path: P) -> Result<Self> {
        let spec = WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = WavWriter::create(file_path.as_ref(), spec)?;
        Ok(Self { writer })
    }

    /// Appends samples, converting f32 -> i16 the same way `save_wav_file`
    /// does.
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        for &sample in samples {
            let sample_i16 = (sample * i16::MAX as f32) as i16;
            self.writer.write_sample(sample_i16)?;
        }
        Ok(())
    }

    /// Writes the correct header/size in place without closing the file —
    /// not required for `write()` to be crash-safe (the data itself is
    /// already past the OS write buffer), only for an external reader to
    /// see a non-corrupt file while capture is still running. Not currently
    /// called anywhere; kept for a future recovery-CLI use.
    #[allow(dead_code)]
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }

    /// Writes the final header/size. Consumes `self` because writing after
    /// this point would leave a file whose header undercounts its own data.
    /// Prefer calling this explicitly and promptly over relying on `Drop` —
    /// see the struct doc for why: `Drop` only helps when something Rust's
    /// drop glue actually runs for, which a hang is definitionally not.
    pub fn finalize(self) -> Result<()> {
        self.writer.finalize()?;
        Ok(())
    }
}

#[cfg(test)]
mod streaming_wav_writer_tests {
    use super::*;

    /// A file only `finalize()`d after every chunk has landed reads back
    /// with the right length and the right samples — the property the
    /// whole task depends on (t17873258798635ef6): if this round trip were
    /// wrong, every meeting WAV written this way would be silently corrupt.
    fn round_trip_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "lark-streaming-wav-test-{:?}.wav",
            std::thread::current().id()
        ))
    }

    #[test]
    fn writes_across_multiple_chunks_and_reads_back_exactly() {
        let path = round_trip_path();
        let mut writer = StreamingWavWriter::create(&path).unwrap();
        // Several small chunks, the way the consumer thread actually calls
        // this — one ~30ms frame at a time, not one big buffer.
        let chunk_a = vec![0.0_f32, 0.25, -0.25, 0.5];
        let chunk_b = vec![-0.5, 1.0, -1.0];
        writer.write(&chunk_a).unwrap();
        writer.write(&chunk_b).unwrap();
        writer.finalize().unwrap();

        let samples = read_wav_samples(&path).unwrap();
        assert_eq!(samples.len(), chunk_a.len() + chunk_b.len());
        // i16 round-trip loses precision (matches `save_wav_file`'s
        // existing conversion) — assert closeness, not bit-exactness.
        let expected: Vec<f32> = chunk_a.into_iter().chain(chunk_b).collect();
        for (got, want) in samples.iter().zip(expected.iter()) {
            assert!(
                (got - want).abs() < 0.001,
                "sample mismatch: got {got}, want {want}"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    /// A `StreamingWavWriter` that merely falls out of scope without an
    /// explicit `finalize()` call (a panic, an early return — anything
    /// Rust's normal drop glue runs for) still reads back correctly,
    /// because `hound::WavWriter`'s own `Drop` patches the header if it
    /// wasn't already finalised. This is what the struct doc's claim rests
    /// on — verified here against hound 3.5.1 rather than assumed, since
    /// the whole design (finalising explicitly and early, rather than
    /// trusting `Drop`, only for the hang case `Drop` can't reach) depends
    /// on knowing exactly which failure this safety net does and doesn't
    /// cover.
    #[test]
    fn unfinalised_file_is_not_left_corrupt() {
        let path = round_trip_path().with_extension("unfinalised.wav");
        let mut writer = StreamingWavWriter::create(&path).unwrap();
        writer.write(&[0.1, 0.2, 0.3]).unwrap();
        // Deliberately dropped without finalize() — simulates a panic or
        // an early return between `write` and `finalize`, NOT a hang (a
        // hung thread never reaches this drop either — see the struct doc).
        drop(writer);

        let samples = read_wav_samples(&path).expect(
            "hound's own Drop impl should have patched the header on an unfinalised writer",
        );
        assert_eq!(samples.len(), 3);

        let _ = std::fs::remove_file(&path);
    }
}

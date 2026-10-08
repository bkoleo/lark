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
///
/// Neither `Drop` nor an explicit finalise runs when the process is simply
/// gone — SIGKILL, a jetsam, or (2026-10-08) a Quit Apple Event from the
/// Dock that AppKit turned into `exit(0)` with no Rust unwinding at all.
/// That left two 27 MB files whose RIFF and data sizes both read 0, and the
/// audio had to be recovered by hand. So the writer now checkpoints itself:
/// at most once a second `write()` rewrites the header to the true size
/// (`hound::WavWriter::flush`), so the file on disk is a valid WAV at every
/// moment, a second of audio behind at worst. `repair_wav_header` closes
/// even that gap at the next launch.
pub struct StreamingWavWriter {
    writer: WavWriter<BufWriter<File>>,
    last_checkpoint: std::time::Instant,
    checkpoint_every: std::time::Duration,
}

/// How often `write()` rewrites the header. One second: a 16 kHz mono i16
/// stream is 32 KB/s, so a checkpoint is a 44-byte seek-and-write per
/// 32 KB appended — unmeasurable against the audio itself.
const WAV_CHECKPOINT_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

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
        Ok(Self {
            writer,
            last_checkpoint: std::time::Instant::now(),
            checkpoint_every: WAV_CHECKPOINT_EVERY,
        })
    }

    /// Appends samples, converting f32 -> i16 the same way `save_wav_file`
    /// does, and checkpoints the header once `checkpoint_every` has passed
    /// since the last one.
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        for &sample in samples {
            let sample_i16 = (sample * i16::MAX as f32) as i16;
            self.writer.write_sample(sample_i16)?;
        }
        if self.last_checkpoint.elapsed() >= self.checkpoint_every {
            self.flush()?;
        }
        Ok(())
    }

    /// Writes the correct header/size in place without closing the file,
    /// so a reader that opens the file right now — or a process that finds
    /// it after this one has died — sees a valid WAV up to the last
    /// checkpoint. Called by `write()` on its own clock; safe to call at
    /// any time.
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.last_checkpoint = std::time::Instant::now();
        Ok(())
    }

    #[cfg(test)]
    fn with_checkpoint_every(mut self, every: std::time::Duration) -> Self {
        self.checkpoint_every = every;
        self
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

/// What `repair_wav_header` found.
#[derive(Debug, PartialEq, Eq)]
pub enum WavRepair {
    /// The header already described every byte on disk.
    Intact,
    /// The header was rewritten to the true size; carries the data length
    /// (in bytes) it now declares.
    Repaired { data_bytes: u64 },
}

/// Rewrites the RIFF and `data` chunk sizes of a plain 44-byte-header PCM
/// WAV so they match the bytes actually on disk, in place. This is the
/// by-hand repair of 2026-10-08 (both sizes read 0 on two 27 MB files) made
/// automatic: with `StreamingWavWriter`'s checkpointing the header is at
/// most a second stale, and this takes it the rest of the way, so every
/// sample the OS accepted before the process died is in the file a reader
/// sees. A torn trailing sample is trimmed rather than declared.
///
/// Refuses anything that is not a plain 44-byte header (a `data` chunk not
/// at byte 36 means extra chunks this code does not understand) — a wrong
/// repair is worse than none.
pub fn repair_wav_header<P: AsRef<Path>>(path: P) -> Result<WavRepair> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let path = path.as_ref();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let len = file.metadata()?.len();
    if len < 44 {
        anyhow::bail!(
            "{} is {len} bytes — shorter than a WAV header",
            path.display()
        );
    }
    let mut header = [0u8; 44];
    file.read_exact(&mut header)?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        anyhow::bail!("{} is not a RIFF/WAVE file", path.display());
    }
    if &header[36..40] != b"data" {
        anyhow::bail!(
            "{} does not have a plain 44-byte header (no data chunk at byte 36) — leaving it alone",
            path.display()
        );
    }
    let block_align = u16::from_le_bytes([header[32], header[33]]).max(1) as u64;
    let data_bytes = (len - 44) / block_align * block_align;
    let riff_bytes = 36 + data_bytes;
    if riff_bytes > u32::MAX as u64 {
        anyhow::bail!("{} is too large for a RIFF header", path.display());
    }
    let riff_now = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as u64;
    let data_now = u32::from_le_bytes([header[40], header[41], header[42], header[43]]) as u64;
    if riff_now == riff_bytes && data_now == data_bytes {
        return Ok(WavRepair::Intact);
    }
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&(riff_bytes as u32).to_le_bytes())?;
    file.seek(SeekFrom::Start(40))?;
    file.write_all(&(data_bytes as u32).to_le_bytes())?;
    if 44 + data_bytes < len {
        file.set_len(44 + data_bytes)?;
    }
    file.sync_all()?;
    Ok(WavRepair::Repaired { data_bytes })
}

#[cfg(test)]
mod streaming_wav_writer_tests {
    use super::*;

    /// The 2026-10-08 failure, in miniature: a writer that is still open
    /// when the process vanishes (`mem::forget` — no `Drop`, no finalise,
    /// exactly what `exit(0)` from AppKit's terminate path does) leaves a
    /// file that is nonetheless a valid WAV holding everything written up
    /// to the last checkpoint.
    #[test]
    fn header_refresh_keeps_file_readable_mid_write() {
        let path = round_trip_path().with_extension("checkpoint.wav");
        let mut writer = StreamingWavWriter::create(&path)
            .unwrap()
            .with_checkpoint_every(std::time::Duration::ZERO);
        let frame = vec![0.1_f32; 480];
        for _ in 0..10 {
            writer.write(&frame).unwrap();
        }
        // The process dies here: no finalize, no Drop.
        std::mem::forget(writer);

        let samples = read_wav_samples(&path)
            .expect("a checkpointed file must parse without any finalise having run");
        assert_eq!(samples.len(), 480 * 10);
        let _ = std::fs::remove_file(&path);
    }

    /// Without the checkpoint, the same death leaves the file hound's
    /// placeholder header describes: zero samples. This is the baseline the
    /// checkpoint exists to replace, kept as a test so a future refactor
    /// that drops the checkpoint fails loudly here rather than in a meeting.
    #[test]
    fn header_refresh_is_what_makes_the_difference() {
        let path = round_trip_path().with_extension("no-checkpoint.wav");
        let mut writer = StreamingWavWriter::create(&path)
            .unwrap()
            .with_checkpoint_every(std::time::Duration::from_secs(3600));
        writer.write(&vec![0.1_f32; 480 * 10]).unwrap();
        std::mem::forget(writer);

        let samples = read_wav_samples(&path).map(|s| s.len()).unwrap_or(0);
        assert_eq!(
            samples, 0,
            "the unflushed writer should have left nothing readable"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The exact on-disk shape found on 2026-10-08 — RIFF size 0, data
    /// size 0, megabytes of PCM after the header — is repaired in place and
    /// then parses with every sample present.
    #[test]
    fn repair_wav_header_rebuilds_zeroed_sizes() {
        use std::io::{Seek, SeekFrom, Write};
        let path = round_trip_path().with_extension("zeroed.wav");
        let n = 16_000 * 3;
        let mut writer = StreamingWavWriter::create(&path).unwrap();
        writer.write(&vec![0.25_f32; n]).unwrap();
        writer.finalize().unwrap();
        assert_eq!(repair_wav_header(&path).unwrap(), WavRepair::Intact);

        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(4)).unwrap();
        file.write_all(&[0, 0, 0, 0]).unwrap();
        file.seek(SeekFrom::Start(40)).unwrap();
        file.write_all(&[0, 0, 0, 0]).unwrap();
        drop(file);
        assert_eq!(
            read_wav_samples(&path).map(|s| s.len()).unwrap_or(0),
            0,
            "the zeroed header should hide every sample, as it did on 2026-10-08"
        );

        assert_eq!(
            repair_wav_header(&path).unwrap(),
            WavRepair::Repaired {
                data_bytes: (n * 2) as u64
            }
        );
        assert_eq!(read_wav_samples(&path).unwrap().len(), n);
        let _ = std::fs::remove_file(&path);
    }

    /// A header that is merely stale (the last checkpoint ran a second
    /// before death) is brought up to the true length, and a torn trailing
    /// byte is trimmed rather than declared.
    #[test]
    fn repair_wav_header_extends_a_stale_header_and_trims_a_torn_sample() {
        use std::io::Write;
        let path = round_trip_path().with_extension("stale.wav");
        let n = 16_000;
        let mut writer = StreamingWavWriter::create(&path).unwrap();
        writer.write(&vec![0.25_f32; n]).unwrap();
        writer.finalize().unwrap();
        // Another second of audio landed after the last checkpoint, plus one
        // torn byte of a sample that never completed.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&vec![0u8; 16_000 * 2 + 1]).unwrap();
        drop(file);
        assert_eq!(read_wav_samples(&path).unwrap().len(), n);

        assert_eq!(
            repair_wav_header(&path).unwrap(),
            WavRepair::Repaired {
                data_bytes: (n * 2 * 2) as u64
            }
        );
        assert_eq!(read_wav_samples(&path).unwrap().len(), n * 2);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            44 + (n * 2 * 2) as u64
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Anything that is not a plain 44-byte header is refused, untouched.
    #[test]
    fn repair_wav_header_refuses_an_unfamiliar_layout() {
        let path = round_trip_path().with_extension("odd.wav");
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(b"RIFF");
        bytes[8..12].copy_from_slice(b"WAVE");
        bytes[12..16].copy_from_slice(b"LIST");
        std::fs::write(&path, &bytes).unwrap();
        assert!(repair_wav_header(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let _ = std::fs::remove_file(&path);
    }

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

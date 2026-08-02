/*
 Copyright (c) 2023 clone206

 This file is part of rdsd2pcm

 rdsd2pcm is free software: you can redistribute it and/or modify it
 under the terms of the GNU General Public License as published by the
 Free Software Foundation, either version 3 of the License, or
 (at your option) any later version.

 rdsd2pcm is distributed in the hope that it will be useful, but
 WITHOUT ANY WARRANTY; without even the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.
 You should have received a copy of the GNU General Public License
 along with rdsd2pcm. If not, see <https://www.gnu.org/licenses/>.
*/

//! Trait abstraction for "true streaming" sample writers: writers that
//! encode/write samples to disk one batch at a time as they arrive during
//! conversion, instead of buffering an entire track in memory (see
//! `AudioFile`) and writing it out in one shot at the end. This keeps peak
//! memory bounded to a single batch regardless of track length.
//!
//! Every output format now has a true streaming implementation (FLAC,
//! AIFF, AIFC, WAV). Driving all of them through one
//! [`StreamingWriter`] trait object means `PcmWriter` doesn't need a
//! parallel set of `open_*_stream`/`flush_*_block`/`finalize_*_stream`
//! methods and scratch buffers per format - adding a new streaming format
//! is just a new struct implementing this trait plus one match arm in
//! `PcmWriter::open_stream`.

use std::error::Error;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::tag_util::{id3_to_vorbis, write_wav_id3_tag};
use flac_codec::byteorder::LittleEndian;
use flac_codec::encode::{FlacByteWriter, Options};

/// A per-format streaming sample writer. Implementations own their file
/// handle, per-channel scratch buffer, and any format-specific state (bit
/// depth, byte layout, frame counters, etc).
///
/// Usage pattern (mirrors the DSD block processing loop in
/// `ConversionContext::process_blocks`):
/// 1. `push_sample` once per sample as it is produced.
/// 2. `flush_ready` after each DSD block, to opportunistically write out
///    any fully-accumulated batch. No-op until enough samples have piled
///    up (each implementation picks its own threshold).
/// 3. `finalize` exactly once, when conversion completes successfully, to
///    flush any remaining partial batch and close out the file (patch
///    header sizes, write trailing metadata, etc).
///
/// If conversion is cancelled or fails, the writer is simply dropped
/// without calling `finalize` (see `PcmWriter::abort_stream`); the partial
/// output file is discarded by the caller, so no format-specific abort
/// logic is required here.
pub trait StreamingWriter: Send {
    /// Push one already-quantized sample for `chan` into this writer's
    /// scratch buffer. Cheap; does not necessarily touch disk (see
    /// `flush_ready`).
    fn push_sample(&mut self, chan: usize, sample: i32);

    /// Push one 32-bit float sample for `chan`, for formats that support
    /// float PCM (currently only WAV). The default implementation panics;
    /// only writers opened in float mode override it, and callers only
    /// invoke this when the writer was opened for 32-bit float output.
    fn push_sample_f32(&mut self, _chan: usize, _sample: f32) {
        unreachable!(
            "push_sample_f32 called on a writer that does not support float PCM"
        );
    }

    /// Interleave and write out any fully-accumulated batch of samples
    /// across all channels, leaving a trailing partial batch (smaller than
    /// this writer's flush threshold) buffered for the next call. Called
    /// once per DSD block; cheap no-op when the threshold hasn't been
    /// reached yet.
    fn flush_ready(&mut self) -> Result<(), Box<dyn Error>>;

    /// Flush all remaining buffered samples regardless of size and close
    /// out the file (patch header fields, write trailing metadata, etc).
    /// Consumes the writer since it cannot be used afterward.
    fn finalize(self: Box<Self>) -> Result<(), Box<dyn Error>>;
}

/// Batch this many frames (across all channels) before handing them to the
/// FLAC encoder. Larger values mean fewer, bigger writes at the cost of a
/// bit more memory (frames * channels * 4 bytes, since scratch is stored
/// as `i32`) - trivial compared to the full-track buffering this streaming
/// path replaces. At 2 channels this is ~16 MiB per in-flight file, which
/// is negligible even with several files converting in parallel.
const FLAC_FLUSH_FRAMES: usize = 2 * 1024 * 1024;

/// Capacity of the `BufWriter` wrapping the output file for streaming FLAC
/// writes. The FLAC encoder emits one write per encoded frame (~4096
/// samples by default), so a small default buffer can otherwise result in
/// a very large number of small OS-level write syscalls ("disk slamming").
const FLAC_WRITE_BUF_CAPACITY: usize = 8 * 1024 * 1024;

/// AIFF streaming uses a large flush threshold so each write is one big,
/// contiguous write to a single file rather than many small ones.
const AIFF_FLUSH_FRAMES: usize = 8 * 1024 * 1024;

/// AIFF streaming uses the same underlying file buffer capacity as FLAC so
/// the OS sees fewer, larger physical writes.
const AIFF_WRITE_BUF_CAPACITY: usize = 8 * 1024 * 1024;

/// AIFC streaming uses the same large flush threshold as AIFF.
const AIFC_FLUSH_FRAMES: usize = 8 * 1024 * 1024;

/// AIFC streaming uses the same underlying file buffer capacity as AIFF.
const AIFC_WRITE_BUF_CAPACITY: usize = 8 * 1024 * 1024;

/// WAV streaming uses the same large flush threshold as AIFF.
const WAV_FLUSH_FRAMES: usize = 8 * 1024 * 1024;

/// WAV streaming uses the same underlying file buffer capacity as AIFF/FLAC.
const WAV_WRITE_BUF_CAPACITY: usize = 8 * 1024 * 1024;

// NOTE: writes across concurrently-converting files are intentionally NOT
// serialized with a shared lock. That was tried and made things worse on
// NVMe/SSD storage: forcing every file's write through a single mutex caps
// the effective queue depth at 1 (only one file's write in flight at a
// time), which starves the drive's ability to service multiple concurrent
// I/O requests in parallel - the opposite of what fast SSDs are good at.
// It also blocks the other conversion threads on the lock instead of
// letting them keep working, which shows up as high disk activity but low
// CPU usage. Each file's own buffered writer plus a large per-flush batch
// (see `AIFF_FLUSH_FRAMES` / `FLAC_FLUSH_FRAMES`) is enough to avoid
// small-write thrashing without serializing across files.
// (If this ever needs to run well on spinning HDDs again, serialization
// akin to the previous approach may be worth reintroducing behind a
// storage-type check rather than unconditionally.)

/// True streaming FLAC writer: encodes and writes samples directly to disk
/// one batch at a time (see `FLAC_FLUSH_FRAMES`) instead of accumulating
/// an entire track in memory.
pub struct FlacStreamWriter {
    inner: FlacByteWriter<io::BufWriter<std::fs::File>, LittleEndian>,
    scratch: Vec<Vec<i32>>,
    channels_num: usize,
    bytes_per_sample: usize,
}

impl FlacStreamWriter {
    /// Open a true streaming FLAC writer at `out_path`. Must be called
    /// after any tag/vorbis metadata has already been prepared, since FLAC
    /// metadata blocks must precede all audio frames.
    pub fn open(
        out_path: &Path,
        rate: u32,
        bits: usize,
        channels_num: usize,
        tag: Option<id3::Tag>,
    ) -> Result<Self, Box<dyn Error>> {
        let mut bits_per_sample = bits;
        if bits_per_sample > 24 {
            bits_per_sample = 24;
        }
        if bits_per_sample != 16
            && bits_per_sample != 20
            && bits_per_sample != 24
        {
            return Err("FLAC: only 16, 20, or 24-bit supported".into());
        }
        let bytes_per_sample = if bits_per_sample == 20 {
            3
        } else {
            bits_per_sample / 8
        };

        let mut opts = Options::default();

        if let Some(tag) = tag {
            let (vorbis, pictures) = id3_to_vorbis(&tag);
            opts = opts.comment(vorbis);
            for pic in pictures {
                opts = opts.picture(pic);
            }
        }

        if out_path.exists() {
            std::fs::remove_file(out_path).map_err(|e| {
                format!(
                    "Failed to remove existing file '{}': {}",
                    out_path.to_string_lossy(),
                    e
                )
            })?;
        }

        // Open the file ourselves with a large `BufWriter` capacity
        // instead of using `FlacByteWriter::create` (which wraps the file
        // in a default ~8 KiB `BufWriter`). The FLAC encoder internally
        // emits one write per encoded FLAC frame (~4096 samples), which at
        // high sample rates/thread counts can otherwise translate into a
        // very large number of small OS-level write syscalls. A multi-MiB
        // buffer lets the OS batch many encoded frames into far fewer,
        // larger physical writes.
        let file = std::fs::File::create(out_path)
            .map_err(|e| format!("FLAC create: {e}"))?;
        let buffered =
            io::BufWriter::with_capacity(FLAC_WRITE_BUF_CAPACITY, file);

        let inner: FlacByteWriter<_, LittleEndian> = FlacByteWriter::new(
            buffered,
            opts,
            rate,
            bits_per_sample as u32,
            channels_num.try_into().unwrap(),
            None,
        )
        .map_err(|e| format!("FLAC create: {e}"))?;

        Ok(Self {
            inner,
            scratch: (0..channels_num).map(|_| Vec::new()).collect(),
            channels_num,
            bytes_per_sample,
        })
    }

    fn min_frames(&self) -> usize {
        self.scratch.iter().map(|c| c.len()).min().unwrap_or(0)
    }

    /// Interleave `frames` worth of samples from `scratch` into
    /// little-endian bytes and write them to the underlying FLAC encoder,
    /// then clear the scratch buffers (retaining their capacity) for
    /// reuse.
    fn encode_and_write(
        &mut self,
        frames: usize,
    ) -> Result<(), Box<dyn Error>> {
        let bps = self.bytes_per_sample;
        let mut buf: Vec<u8> =
            Vec::with_capacity(frames * self.channels_num * bps);
        if bps == 2 {
            for i in 0..frames {
                for ch in 0..self.channels_num {
                    let v =
                        self.scratch[ch][i].max(-32768).min(32767) as i16;
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
        } else {
            for i in 0..frames {
                for ch in 0..self.channels_num {
                    let v = self.scratch[ch][i];
                    buf.extend_from_slice(&[
                        (v & 0xFF) as u8,
                        ((v >> 8) & 0xFF) as u8,
                        ((v >> 16) & 0xFF) as u8,
                    ]);
                }
            }
        }
        self.inner
            .write_all(&buf)
            .map_err(|e| format!("FLAC write: {e}"))?;

        for ch in &mut self.scratch {
            ch.drain(0..frames);
        }
        Ok(())
    }
}

impl StreamingWriter for FlacStreamWriter {
    fn push_sample(&mut self, chan: usize, sample: i32) {
        self.scratch[chan].push(sample);
    }

    fn flush_ready(&mut self) -> Result<(), Box<dyn Error>> {
        let frames = self.min_frames();
        if frames < FLAC_FLUSH_FRAMES {
            return Ok(());
        }
        self.encode_and_write(frames)
    }

    fn finalize(self: Box<Self>) -> Result<(), Box<dyn Error>> {
        let mut this = *self;
        let frames = this.min_frames();
        if frames > 0 {
            this.encode_and_write(frames)?;
        }
        this.inner
            .finalize()
            .map_err(|e| format!("FLAC finalize: {e}"))?;
        Ok(())
    }
}

/// True streaming AIFF writer: pre-writes a header with placeholder sizes,
/// streams sample batches to disk as they arrive (see `AIFF_FLUSH_FRAMES`),
/// and patches the final sizes/frame count on `finalize`.
pub struct AiffStreamWriter {
    inner: io::BufWriter<std::fs::File>,
    scratch: Vec<Vec<i32>>,
    channels_num: usize,
    bits: usize,
    bytes_per_sample: usize,
    frames_written: u64,
    out_path: PathBuf,
    tag: Option<id3::Tag>,
}

impl AiffStreamWriter {
    /// Open a true streaming AIFF writer at `out_path`, writing a
    /// placeholder header up front (patched in `finalize` once the total
    /// frame count is known).
    pub fn open(
        out_path: &Path,
        rate: u32,
        bits: usize,
        channels_num: usize,
        tag: Option<id3::Tag>,
    ) -> Result<Self, Box<dyn Error>> {
        if bits != 16 && bits != 20 && bits != 24 {
            return Err("AIFF: only 16, 20, or 24-bit supported".into());
        }
        let bytes_per_sample = if bits == 20 { 3 } else { bits / 8 };

        if out_path.exists() {
            std::fs::remove_file(out_path).map_err(|e| {
                format!(
                    "Failed to remove existing file '{}': {}",
                    out_path.to_string_lossy(),
                    e
                )
            })?;
        }

        let file = std::fs::File::create(out_path)
            .map_err(|e| format!("AIFF create: {e}"))?;
        let mut w =
            io::BufWriter::with_capacity(AIFF_WRITE_BUF_CAPACITY, file);

        // Header mirrors the buffered `save_aiff_file` path, but uses
        // placeholders that are patched in `finalize` once the total frame
        // count is known.
        w.write_all(b"FORM")?;
        w.write_all(&0u32.to_be_bytes())?; // form size placeholder
        w.write_all(b"AIFF")?;

        w.write_all(b"COMM")?;
        w.write_all(&18u32.to_be_bytes())?;
        w.write_all(&(channels_num as u16).to_be_bytes())?;
        w.write_all(&0u32.to_be_bytes())?; // numSampleFrames placeholder
        w.write_all(&(bits as u16).to_be_bytes())?;
        let mut extended = [0u8; 10];
        encode_extended(rate as f64, &mut extended);
        w.write_all(&extended)?;

        w.write_all(b"SSND")?;
        w.write_all(&0u32.to_be_bytes())?; // ssnd size placeholder
        w.write_all(&0u32.to_be_bytes())?; // offset
        w.write_all(&0u32.to_be_bytes())?; // block size

        Ok(Self {
            inner: w,
            scratch: (0..channels_num).map(|_| Vec::new()).collect(),
            channels_num,
            bits,
            bytes_per_sample,
            frames_written: 0,
            out_path: out_path.to_path_buf(),
            tag,
        })
    }

    fn min_frames(&self) -> usize {
        self.scratch.iter().map(|c| c.len()).min().unwrap_or(0)
    }

    /// Interleave `frames` worth of samples from `scratch` into big-endian
    /// bytes and write them out in one large sequential write (matching
    /// the FLAC streaming path), then clear the scratch buffers.
    fn encode_and_write(
        &mut self,
        frames: usize,
    ) -> Result<(), Box<dyn Error>> {
        let bps = self.bytes_per_sample;
        let mut buf: Vec<u8> =
            Vec::with_capacity(frames * self.channels_num * bps);

        if self.bits == 16 {
            for i in 0..frames {
                for ch in 0..self.channels_num {
                    let v =
                        self.scratch[ch][i].max(-32768).min(32767) as i16;
                    buf.extend_from_slice(&v.to_be_bytes());
                }
            }
        } else {
            for i in 0..frames {
                for ch in 0..self.channels_num {
                    let mut v = self.scratch[ch][i];
                    if self.bits == 20 {
                        v <<= 4;
                    }
                    buf.extend_from_slice(&[
                        ((v >> 16) & 0xFF) as u8,
                        ((v >> 8) & 0xFF) as u8,
                        (v & 0xFF) as u8,
                    ]);
                }
            }
        }

        self.inner
            .write_all(&buf)
            .map_err(|e| format!("AIFF write: {e}"))?;
        self.frames_written += frames as u64;

        for ch in &mut self.scratch {
            ch.drain(0..frames);
        }
        Ok(())
    }
}

impl StreamingWriter for AiffStreamWriter {
    fn push_sample(&mut self, chan: usize, sample: i32) {
        self.scratch[chan].push(sample);
    }

    fn flush_ready(&mut self) -> Result<(), Box<dyn Error>> {
        let frames = self.min_frames();
        if frames < AIFF_FLUSH_FRAMES {
            return Ok(());
        }
        self.encode_and_write(frames)
    }

    fn finalize(self: Box<Self>) -> Result<(), Box<dyn Error>> {
        let mut this = *self;
        let frames = this.min_frames();
        if frames > 0 {
            this.encode_and_write(frames)?;
        }

        let data_size = this
            .frames_written
            .saturating_mul(this.channels_num as u64)
            .saturating_mul(this.bytes_per_sample as u64);
        if data_size > u32::MAX as u64 {
            return Err(
                "AIFF stream too large for 32-bit AIFF chunk sizes".into(),
            );
        }
        let form_size = data_size + 46;
        if form_size > u32::MAX as u64 {
            return Err(
                "AIFF stream too large for 32-bit AIFF FORM size".into()
            );
        }

        // FORM size at byte offset 4
        this.inner.seek(SeekFrom::Start(4))?;
        this.inner.write_all(&(form_size as u32).to_be_bytes())?;

        // COMM.numSampleFrames at byte offset 22
        this.inner.seek(SeekFrom::Start(22))?;
        this.inner
            .write_all(&(this.frames_written as u32).to_be_bytes())?;

        // SSND chunk size at byte offset 42 (kept aligned with legacy
        // buffered AIFF behavior)
        this.inner.seek(SeekFrom::Start(42))?;
        this.inner.write_all(&(data_size as u32).to_be_bytes())?;

        let out_path = this.out_path.clone();
        let tag = this.tag.take();
        this.inner.flush()?;
        this.inner.get_mut().sync_all()?;
        drop(this.inner);

        if let Some(tag) = tag {
            tag.write_to_path(&out_path, tag.version())?;
        }
        Ok(())
    }
}

/// True streaming AIFC writer. Unlike `AiffStreamWriter`, AIFC also
/// supports 32-bit float PCM (see `is_float`) and its header carries a
/// compression type/name pair (`COMM`) plus a mandatory `FVER` chunk and a
/// `FLLR` padding chunk that aligns the `SSND` audio data to a 4096-byte
/// boundary. All of that header content (aside from the `FORM` size,
/// `COMM.numSampleFrames`, and `SSND` size fields, which depend on the
/// final frame count) is fixed size and is written up front in `open`;
/// only those three fields are patched in `finalize`.
pub struct AifcStreamWriter {
    inner: io::BufWriter<std::fs::File>,
    channels_num: usize,
    bits: usize,
    bytes_per_sample: usize,
    is_float: bool,
    frames_written: u64,
    comm_frames_offset: u64,
    ssnd_size_offset: u64,
    out_path: PathBuf,
    tag: Option<id3::Tag>,
    int_scratch: Vec<Vec<i32>>,
    float_scratch: Vec<Vec<f32>>,
}

impl AifcStreamWriter {
    /// Open a true streaming AIFC writer at `out_path`, writing the full
    /// header (with placeholder `FORM`/`COMM`/`SSND` size fields, patched
    /// in `finalize` once the total frame count is known) up front.
    /// `bits == 32` selects 32-bit float PCM; all other supported bit
    /// depths are integer PCM.
    pub fn open(
        out_path: &Path,
        rate: u32,
        bits: usize,
        channels_num: usize,
        tag: Option<id3::Tag>,
    ) -> Result<Self, Box<dyn Error>> {
        let is_float = bits == 32;
        if bits != 16 && bits != 20 && bits != 24 && bits != 32 {
            return Err(
                "AIFC: only 16, 20, 24, or 32-bit supported".into()
            );
        }
        let bytes_per_sample = if is_float {
            4
        } else if bits == 20 {
            3
        } else {
            bits / 8
        };

        // COMM compression type and Pascal string name. All integer
        // formats use big-endian byte order (matches the formerly
        // buffered `AudioFile::save_aifc_file` path).
        let (comp_type, comp_name_bytes): ([u8; 4], Vec<u8>) = if is_float
        {
            let s =
                b"Linear PCM, 32 bit big-endian floating point" as &[u8];
            let mut v = vec![s.len() as u8];
            v.extend_from_slice(s);
            (*b"fl32", v)
        } else if bits == 16 {
            let s =
                b"Linear PCM, 16 bit big-endian signed integer" as &[u8];
            let mut v = vec![s.len() as u8];
            v.extend_from_slice(s);
            (*b"twos", v)
        } else {
            // 24-bit (and 20-bit packed into 24)
            let s =
                b"Linear PCM, 24 bit big-endian signed integer" as &[u8];
            let mut v = vec![s.len() as u8];
            v.extend_from_slice(s);
            // TODO: Apple afconvert and other software use "in24", but
            // sox does not understand; using "NONE" for sox compatibility
            (*b"NONE", v)
        };
        // channels(2) + frames(4) + bitDepth(2) + rate(10) + comp_type(4) + comp_name
        let comm_size: u32 = 22 + comp_name_bytes.len() as u32;
        let comm_size_padded = (comm_size + 1) & !1;

        // FLLR: pad so SSND audio data starts on a 4096-byte boundary.
        // before_fllr = FORM(8) + AIFC(4) + FVER(12) + COMM(8+comm_size_padded)
        //             = 32 + comm_size_padded
        let fllr_data_size: u32 = {
            const PAGE: u32 = 4096;
            let before_fllr = 32 + comm_size_padded;
            // +24: FLLR header(8) + SSND header(8) + SSND offset+block(8)
            let r = (before_fllr + 24) % PAGE;
            if r == 0 { 0 } else { PAGE - r }
        };

        if out_path.exists() {
            std::fs::remove_file(out_path).map_err(|e| {
                format!(
                    "Failed to remove existing file '{}': {}",
                    out_path.to_string_lossy(),
                    e
                )
            })?;
        }

        let file = std::fs::File::create(out_path)
            .map_err(|e| format!("AIFC create: {e}"))?;
        let mut w =
            io::BufWriter::with_capacity(AIFC_WRITE_BUF_CAPACITY, file);

        w.write_all(b"FORM")?;
        w.write_all(&0u32.to_be_bytes())?; // form size placeholder
        w.write_all(b"AIFC")?;

        // FVER (mandatory for AIFC, version timestamp 0xA2805140)
        w.write_all(b"FVER")?;
        w.write_all(&4u32.to_be_bytes())?;
        w.write_all(&0xA2805140u32.to_be_bytes())?;

        // COMM
        w.write_all(b"COMM")?;
        w.write_all(&comm_size_padded.to_be_bytes())?;
        w.write_all(&(channels_num as u16).to_be_bytes())?;
        // Absolute byte offset of numSampleFrames, patched in `finalize`.
        let comm_frames_offset = 12 + 12 + 8 + 2;
        w.write_all(&0u32.to_be_bytes())?; // numSampleFrames placeholder
        w.write_all(&(bits as u16).to_be_bytes())?;
        let mut extended = [0u8; 10];
        encode_extended(rate as f64, &mut extended);
        w.write_all(&extended)?;
        w.write_all(&comp_type)?;
        w.write_all(&comp_name_bytes)?;
        if comm_size % 2 != 0 {
            w.write_all(&[0u8])?;
        }

        // FLLR
        if fllr_data_size > 0 {
            w.write_all(b"FLLR")?;
            w.write_all(&fllr_data_size.to_be_bytes())?;
            w.write_all(&vec![0u8; fllr_data_size as usize])?;
        }

        // SSND
        w.write_all(b"SSND")?;
        // Absolute byte offset of the SSND chunk size, patched in
        // `finalize`. Mirrors `comm_frames_offset`: computed arithmetically
        // from the (fixed-size) header layout above rather than queried
        // via `stream_position`, since none of it depends on frame count.
        let fllr_chunk_size = if fllr_data_size > 0 {
            8 + fllr_data_size
        } else {
            0
        };
        let ssnd_size_offset: u64 =
            (12 + 12 + 8 + comm_size_padded + fllr_chunk_size + 4) as u64;
        w.write_all(&0u32.to_be_bytes())?; // ssnd size placeholder
        w.write_all(&0u32.to_be_bytes())?; // offset
        w.write_all(&0u32.to_be_bytes())?; // block size

        Ok(Self {
            inner: w,
            channels_num,
            bits,
            bytes_per_sample,
            is_float,
            frames_written: 0,
            comm_frames_offset,
            ssnd_size_offset,
            out_path: out_path.to_path_buf(),
            tag,
            int_scratch: if is_float {
                Vec::new()
            } else {
                (0..channels_num).map(|_| Vec::new()).collect()
            },
            float_scratch: if is_float {
                (0..channels_num).map(|_| Vec::new()).collect()
            } else {
                Vec::new()
            },
        })
    }

    fn min_frames(&self) -> usize {
        if self.is_float {
            self.float_scratch
                .iter()
                .map(|c| c.len())
                .min()
                .unwrap_or(0)
        } else {
            self.int_scratch.iter().map(|c| c.len()).min().unwrap_or(0)
        }
    }

    /// Interleave `frames` worth of samples from the active scratch buffer
    /// into big-endian bytes and write them out in one large sequential
    /// write (matching the FLAC/AIFF streaming paths), then clear the
    /// scratch buffers.
    fn encode_and_write(
        &mut self,
        frames: usize,
    ) -> Result<(), Box<dyn Error>> {
        let bps = self.bytes_per_sample;
        let mut buf: Vec<u8> =
            Vec::with_capacity(frames * self.channels_num * bps);

        if self.is_float {
            for i in 0..frames {
                for ch in 0..self.channels_num {
                    let v = self.float_scratch[ch][i];
                    buf.extend_from_slice(&v.to_be_bytes());
                }
            }
        } else if self.bits == 16 {
            for i in 0..frames {
                for ch in 0..self.channels_num {
                    let v = self.int_scratch[ch][i].max(-32768).min(32767)
                        as i16;
                    buf.extend_from_slice(&v.to_be_bytes());
                }
            }
        } else {
            for i in 0..frames {
                for ch in 0..self.channels_num {
                    let mut v = self.int_scratch[ch][i];
                    if self.bits == 20 {
                        v <<= 4;
                    }
                    buf.extend_from_slice(&[
                        ((v >> 16) & 0xFF) as u8,
                        ((v >> 8) & 0xFF) as u8,
                        (v & 0xFF) as u8,
                    ]);
                }
            }
        }

        self.inner
            .write_all(&buf)
            .map_err(|e| format!("AIFC write: {e}"))?;
        self.frames_written += frames as u64;

        if self.is_float {
            for ch in &mut self.float_scratch {
                ch.drain(0..frames);
            }
        } else {
            for ch in &mut self.int_scratch {
                ch.drain(0..frames);
            }
        }
        Ok(())
    }
}

impl StreamingWriter for AifcStreamWriter {
    fn push_sample(&mut self, chan: usize, sample: i32) {
        self.int_scratch[chan].push(sample);
    }

    fn push_sample_f32(&mut self, chan: usize, sample: f32) {
        self.float_scratch[chan].push(sample);
    }

    fn flush_ready(&mut self) -> Result<(), Box<dyn Error>> {
        let frames = self.min_frames();
        if frames < AIFC_FLUSH_FRAMES {
            return Ok(());
        }
        self.encode_and_write(frames)
    }

    fn finalize(self: Box<Self>) -> Result<(), Box<dyn Error>> {
        let mut this = *self;
        let frames = this.min_frames();
        if frames > 0 {
            this.encode_and_write(frames)?;
        }

        let data_size = this
            .frames_written
            .saturating_mul(this.channels_num as u64)
            .saturating_mul(this.bytes_per_sample as u64);
        if data_size > u32::MAX as u64 {
            return Err(
                "AIFC stream too large for 32-bit AIFF/AIFC chunk sizes"
                    .into(),
            );
        }
        // FORM size = everything from "AIFC" formType through the end of
        // audio data (i.e. total header bytes before audio data - the
        // initial "FORM"+size(8 bytes), which isn't counted in FORM's own
        // size field - plus the audio data itself). Header bytes before
        // audio data = `ssnd_size_offset` + its own 4-byte size field +
        // the 8-byte offset/blocksize fields that follow it.
        let form_size = this.ssnd_size_offset + 4 + data_size;
        if form_size > u32::MAX as u64 {
            return Err(
                "AIFC stream too large for 32-bit AIFF/AIFC FORM size"
                    .into(),
            );
        }

        // FORM size at byte offset 4
        this.inner.seek(SeekFrom::Start(4))?;
        this.inner.write_all(&(form_size as u32).to_be_bytes())?;

        // COMM.numSampleFrames
        this.inner.seek(SeekFrom::Start(this.comm_frames_offset))?;
        this.inner
            .write_all(&(this.frames_written as u32).to_be_bytes())?;

        // SSND chunk size (offset+blocksize header + audio data)
        this.inner.seek(SeekFrom::Start(this.ssnd_size_offset))?;
        this.inner
            .write_all(&((data_size + 8) as u32).to_be_bytes())?;

        let out_path = this.out_path.clone();
        let tag = this.tag.take();
        this.inner.flush()?;
        this.inner.get_mut().sync_all()?;
        drop(this.inner);

        if let Some(tag) = tag {
            tag.write_to_path(&out_path, tag.version())?;
        }
        Ok(())
    }
}

/// True streaming WAV writer. Supports both integer PCM (16/20/24-bit) and
/// 32-bit float PCM.
pub struct WavStreamWriter {
    inner: io::BufWriter<std::fs::File>,
    channels_num: usize,
    bits: usize,
    bytes_per_sample: usize,
    is_float: bool,
    frames_written: u64,
    out_path: PathBuf,
    tag: Option<id3::Tag>,
    int_scratch: Vec<Vec<i32>>,
    float_scratch: Vec<Vec<f32>>,
}

impl WavStreamWriter {
    /// Open a true streaming WAV writer at `out_path`, writing a
    /// placeholder header (with a reserved `JUNK` chunk, see struct docs)
    /// up front. `bits == 32` selects 32-bit float PCM; all other
    /// supported bit depths are integer PCM.
    pub fn open(
        out_path: &Path,
        rate: u32,
        bits: usize,
        channels_num: usize,
        tag: Option<id3::Tag>,
    ) -> Result<Self, Box<dyn Error>> {
        let is_float = bits == 32;
        if bits != 16 && bits != 20 && bits != 24 && bits != 32 {
            return Err("WAV: only 16, 20, 24, or 32-bit supported".into());
        }
        let bytes_per_sample = if bits == 20 { 3 } else { bits / 8 };
        let block_align = channels_num * bytes_per_sample;

        if out_path.exists() {
            std::fs::remove_file(out_path).map_err(|e| {
                format!(
                    "Failed to remove existing file '{}': {}",
                    out_path.to_string_lossy(),
                    e
                )
            })?;
        }

        let file = std::fs::File::create(out_path)
            .map_err(|e| format!("WAV create: {e}"))?;
        let mut w =
            io::BufWriter::with_capacity(WAV_WRITE_BUF_CAPACITY, file);

        // RIFF header. The size field is patched in `finalize`.
        w.write_all(b"RIFF")?;
        w.write_all(&0u32.to_le_bytes())?; // RIFF size placeholder
        w.write_all(b"WAVE")?;

        // fmt chunk
        w.write_all(b"fmt ")?;
        w.write_all(&16u32.to_le_bytes())?;
        let format_tag: u16 = if is_float { 3 } else { 1 };
        w.write_all(&format_tag.to_le_bytes())?;
        w.write_all(&(channels_num as u16).to_le_bytes())?;
        w.write_all(&rate.to_le_bytes())?;
        let byte_rate = rate * block_align as u32;
        w.write_all(&byte_rate.to_le_bytes())?;
        w.write_all(&(block_align as u16).to_le_bytes())?;
        w.write_all(&(bits as u16).to_le_bytes())?;

        // data chunk header; size is a placeholder patched in `finalize`.
        w.write_all(b"data")?;
        w.write_all(&0u32.to_le_bytes())?;

        Ok(Self {
            inner: w,
            channels_num,
            bits,
            bytes_per_sample,
            is_float,
            frames_written: 0,
            out_path: out_path.to_path_buf(),
            tag,
            int_scratch: if is_float {
                Vec::new()
            } else {
                (0..channels_num).map(|_| Vec::new()).collect()
            },
            float_scratch: if is_float {
                (0..channels_num).map(|_| Vec::new()).collect()
            } else {
                Vec::new()
            },
        })
    }

    fn min_frames(&self) -> usize {
        if self.is_float {
            self.float_scratch
                .iter()
                .map(|c| c.len())
                .min()
                .unwrap_or(0)
        } else {
            self.int_scratch.iter().map(|c| c.len()).min().unwrap_or(0)
        }
    }

    /// Interleave `frames` worth of samples from the active scratch buffer
    /// into little-endian bytes and write them out in one large sequential
    /// write, then clear the scratch buffers. Errors out if doing so would
    /// grow the file beyond the standard 32-bit WAV size limit
    fn encode_and_write(
        &mut self,
        frames: usize,
    ) -> Result<(), Box<dyn Error>> {
        let bps = self.bytes_per_sample;
        let mut buf: Vec<u8> =
            Vec::with_capacity(frames * self.channels_num * bps);

        if self.is_float {
            for i in 0..frames {
                for ch in 0..self.channels_num {
                    let v = self.float_scratch[ch][i];
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
        } else if self.bits == 16 {
            for i in 0..frames {
                for ch in 0..self.channels_num {
                    let v = self.int_scratch[ch][i].max(-32768).min(32767)
                        as i16;
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
        } else {
            for i in 0..frames {
                for ch in 0..self.channels_num {
                    let mut v = self.int_scratch[ch][i];
                    if self.bits == 20 {
                        v <<= 4;
                    }
                    buf.extend_from_slice(&[
                        (v & 0xFF) as u8,
                        ((v >> 8) & 0xFF) as u8,
                        ((v >> 16) & 0xFF) as u8,
                    ]);
                }
            }
        }

        let prospective_frames = self.frames_written + frames as u64;
        self.check_size_limit(prospective_frames)?;

        self.inner
            .write_all(&buf)
            .map_err(|e| format!("WAV write: {e}"))?;
        self.frames_written = prospective_frames;

        if self.is_float {
            for ch in &mut self.float_scratch {
                ch.drain(0..frames);
            }
        } else {
            for ch in &mut self.int_scratch {
                ch.drain(0..frames);
            }
        }
        Ok(())
    }

    /// Real (not yet size-limited) RIFF chunk size for `frames` worth of
    /// sample frames: `WAVE`(4) + `fmt `(8+16) + `data` header(8) + data bytes.
    fn riff_size_for(&self, frames: u64) -> u64 {
        let data_size = frames
            * self.channels_num as u64
            * self.bytes_per_sample as u64;
        4 + (8 + 16) + 8 + data_size
    }

    /// Fail fast if writing `frames` worth of sample frames would exceed
    /// the standard 32-bit WAV size limit
    fn check_size_limit(&self, frames: u64) -> Result<(), Box<dyn Error>> {
        if self.riff_size_for(frames) >= u32::MAX as u64 {
            return Err(
                "File too large for standard WAV (4GB limit)".into()
            );
        }
        Ok(())
    }
}

impl StreamingWriter for WavStreamWriter {
    fn push_sample(&mut self, chan: usize, sample: i32) {
        self.int_scratch[chan].push(sample);
    }

    fn push_sample_f32(&mut self, chan: usize, sample: f32) {
        self.float_scratch[chan].push(sample);
    }

    fn flush_ready(&mut self) -> Result<(), Box<dyn Error>> {
        let frames = self.min_frames();
        if frames < WAV_FLUSH_FRAMES {
            return Ok(());
        }
        self.encode_and_write(frames)
    }

    fn finalize(self: Box<Self>) -> Result<(), Box<dyn Error>> {
        let mut this = *self;
        let frames = this.min_frames();
        if frames > 0 {
            this.encode_and_write(frames)?;
        }

        let data_size = this
            .frames_written
            .saturating_mul(this.channels_num as u64)
            .saturating_mul(this.bytes_per_sample as u64);
        let riff_size = this.riff_size_for(this.frames_written);

        if riff_size >= u32::MAX as u64 {
            return Err(
                "File too large for standard WAV (4GB limit)".into()
            );
        } else {
            // Standard RIFF/WAVE: patch real sizes.
            this.inner.seek(SeekFrom::Start(4))?;
            this.inner.write_all(&(riff_size as u32).to_le_bytes())?;

            this.inner.seek(SeekFrom::Start(40))?;
            this.inner.write_all(&(data_size as u32).to_le_bytes())?;
        }

        let out_path = this.out_path.clone();
        let tag = this.tag.take();
        this.inner.flush()?;
        this.inner.get_mut().sync_all()?;
        drop(this.inner);

        if let Some(tag) = tag {
            write_wav_id3_tag(&out_path, &tag, tag.version())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn wav_stream_writer_uses_standard_riff_header() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "dsd2dxd-wav-stream-writer-{unique}.wav"
        ));

        let mut writer = WavStreamWriter::open(&path, 44_100, 16, 2, None)
            .unwrap();
        writer.push_sample(0, 0);
        writer.push_sample(1, 32767);
        <WavStreamWriter as StreamingWriter>::finalize(Box::new(writer))
            .unwrap();

        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert!(!bytes.windows(4).any(|chunk| chunk == b"JUNK"));

        let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let data_size = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(riff_size, (bytes.len() as u32).saturating_sub(8));
        assert_eq!(data_size, 4);

        let _ = fs::remove_file(&path);
    }
}

/// Encode `value` as an 80-bit IEEE 754 extended-precision float, as used
/// by the AIFF `COMM` chunk's sample rate field.
fn encode_extended(value: f64, buffer: &mut [u8; 10]) {
    if value == 0.0 {
        *buffer = [0; 10];
        return;
    }

    let sign = if value < 0.0 { 1 } else { 0 };
    let mut abs_value = value.abs();
    let mut exponent = 16383;

    while abs_value >= 1.0 {
        abs_value /= 2.0;
        exponent += 1;
    }
    while abs_value < 0.5 {
        abs_value *= 2.0;
        exponent -= 1;
    }

    let mantissa = (abs_value * (1u64 << 63) as f64) as u64;

    buffer[0] = ((sign << 7) | ((exponent >> 8) & 0x7F)) as u8;
    buffer[1] = (exponent & 0xFF) as u8;
    for i in 0..8 {
        buffer[i + 2] = ((mantissa >> (56 - i * 8)) & 0xFF) as u8;
    }
}

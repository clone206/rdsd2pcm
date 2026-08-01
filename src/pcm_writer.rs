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

use crate::audio_file::{
    AifcStreamWriter, AiffStreamWriter, FlacStreamWriter,
    StreamingWriter, WavStreamWriter,
};
use crate::{Dither, DitherType, OutputType};
use log::debug;
use std::error::Error;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::{io, vec};

pub struct PcmWriter {
    float_data: Vec<f64>,
    scale_factor: f64,
    bits: usize,
    channels_num: usize,
    rate: u32,
    bytes_per_sample: usize,
    output: OutputType,
    path: Option<PathBuf>,
    peak_level: i32,
    clips: i32,
    dither: Dither,
    last_samps_clipped_low: i32,
    last_samps_clipped_high: i32,
    stdout_buf: Vec<u8>,
    // True streaming support: when set, samples are encoded and written
    // directly to disk one batch at a time (see `write_to_buffer`,
    // `flush_stream`) instead of being accumulated in memory. This keeps
    // peak memory bounded to a single batch regardless of track length.
    // Every file output format (Flac, Aiff, Aifc, Wav) has a
    // streaming implementation (see `audio_file`); this is `None`
    // only for `Stdout`, which uses `stdout_buf` instead.
    stream_writer: Option<Box<dyn StreamingWriter>>,
}

impl PcmWriter {
    pub fn rate(&self) -> u32 {
        self.rate
    }
    pub fn channels_num(&self) -> usize {
        self.channels_num
    }
    pub fn clips(&self) -> i32 {
        self.clips
    }
    pub fn bytes_per_sample(&self) -> usize {
        self.bytes_per_sample
    }
    pub fn output(&self) -> OutputType {
        self.output
    }
    pub fn path(&self) -> &Option<PathBuf> {
        &self.path
    }
    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }
    pub fn float_data_mut(&mut self) -> &mut Vec<f64> {
        &mut self.float_data
    }

    /// Create a PCM sink. Useful for level checks or whenever no file output is needed.
    /// 32 bit float PCM is assumed internally.
    /// * `out_rate` - output sample rate in Hz
    /// * `out_frames_capacity` - number of frames to allocate buffer for
    /// * `channels_num` - number of channels
    /// * `upsample_ratio` - upsample ratio (1 when not fractional)
    pub fn new_sink(
        out_rate: u32,
        out_frames_capacity: usize,
        channels_num: usize,
        upsample_ratio: u32,
    ) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bits: 32,
            output: OutputType::Stdout,
            bytes_per_sample: 4,
            channels_num: channels_num,
            rate: out_rate,
            peak_level: 0,
            scale_factor: upsample_ratio as f64,
            stdout_buf: vec![
                0u8;
                out_frames_capacity
                    * channels_num
                    * 4
            ],
            path: None,
            last_samps_clipped_low: 0,
            last_samps_clipped_high: 0,
            clips: 0,
            dither: Dither::new(DitherType::None)?,
            float_data: vec![0.0; out_frames_capacity],
            stream_writer: None,
        })
    }

    pub fn new(
        out_bits: usize,
        out_type: OutputType,
        out_vol: f64,
        out_rate: u32,
        out_path: Option<PathBuf>,
        dither: Dither,
        out_frames_capacity: usize,
        channels_num: usize,
        upsample_ratio: u32,
    ) -> Result<Self, Box<dyn Error>> {
        if ![16, 20, 24, 32].contains(&out_bits) {
            return Err("Unsupported bit depth".into());
        }

        if out_type == OutputType::Stdout && out_path.is_some() {
            return Err(
                "Cannot specify output path when outputting to stdout"
                    .into(),
            );
        }

        if out_bits == 32
            && out_type != OutputType::Stdout
            && out_type != OutputType::Wav
            && out_type != OutputType::Aifc
        {
            return Err(
                "32 bit float only allowed with wav, aifc, or stdout".into()
            );
        }

        let bytes_per_sample =
            if out_bits == 20 { 3 } else { out_bits / 8 };

        let canon_path = if let Some(p) = &out_path {
            if !p.exists() {
                return Err(format!(
                    "Specified output path does not exist: {}",
                    p.display()
                )
                .into());
            }
            Some(p.canonicalize()?)
        } else {
            None
        };

        let mut ctx = Self {
            bits: out_bits,
            output: out_type,
            bytes_per_sample,
            channels_num: channels_num,
            rate: out_rate,
            peak_level: 0,
            scale_factor: 1.0,
            stdout_buf: vec![
                0u8;
                out_frames_capacity
                    * channels_num
                    * bytes_per_sample
            ],
            path: canon_path,
            last_samps_clipped_low: 0,
            last_samps_clipped_high: 0,
            clips: 0,
            dither,
            float_data: vec![0.0; out_frames_capacity],
            stream_writer: None,
        };
        debug!("Dither type: {:#?}", ctx.dither.dither_type());

        ctx.set_scaling(out_vol, upsample_ratio);

        Ok(ctx)
    }

    /// Open this writer's streaming implementation at `out_path`, bypassing
    /// the in-memory sample buffer entirely so peak memory stays bounded
    /// to a single batch regardless of track length. For FLAC, must be
    /// called after any tag/vorbis metadata has already been set via
    /// `id3_to_flac_meta` (FLAC metadata blocks must precede all audio
    /// frames). Must be called before any samples are written via
    /// `write_to_buffer`. No-op for `Stdout`, the only output type without
    /// a streaming implementation (see `audio_file`).
    pub fn open_stream(
        &mut self,
        out_path: &Path,
        tag: Option<id3::Tag>,
    ) -> Result<(), Box<dyn Error>> {
        let writer: Box<dyn StreamingWriter> = match self.output {
            OutputType::Flac => Box::new(FlacStreamWriter::open(
                out_path,
                self.rate,
                self.bits,
                self.channels_num,
                tag,
            )?),
            OutputType::Aiff => Box::new(AiffStreamWriter::open(
                out_path,
                self.rate,
                self.bits,
                self.channels_num,
                tag,
            )?),
            OutputType::Aifc => Box::new(AifcStreamWriter::open(
                out_path,
                self.rate,
                self.bits,
                self.channels_num,
                tag,
            )?),
            OutputType::Wav => Box::new(WavStreamWriter::open(
                out_path,
                self.rate,
                self.bits,
                self.channels_num,
                tag,
            )?),
            OutputType::Stdout => return Ok(()),
        };
        self.stream_writer = Some(writer);
        Ok(())
    }

    /// Interleave and write out the current batch's accumulated samples
    /// (across all channels) once enough frames have accumulated, then
    /// clear the writer's per-channel scratch buffers for reuse. Batching
    /// many DSD blocks' worth of samples before handing them to the
    /// encoder means far fewer, larger writes instead of one tiny write
    /// per DSD block. No-op unless a streaming writer is open (see
    /// `open_stream`) or not enough frames have accumulated yet.
    pub fn flush_stream(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(w) = self.stream_writer.as_mut() {
            w.flush_ready()?;
        }
        Ok(())
    }

    /// Finalize and close the streaming writer opened via `open_stream`,
    /// flushing any samples still sitting in the scratch buffer (which may
    /// be smaller than the writer's flush threshold, since this is the
    /// final, possibly-partial batch) and patching any header fields that
    /// depend on the final frame count. No-op if no streaming writer is
    /// open.
    pub fn finalize_stream(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(w) = self.stream_writer.take() {
            w.finalize()?;
        }
        Ok(())
    }

    /// Whether a streaming writer is currently open (see `open_stream`).
    /// Used by callers to decide whether a partial output file needs to be
    /// cleaned up after a failed/cancelled conversion.
    pub fn has_open_stream(&self) -> bool {
        self.stream_writer.is_some()
    }

    /// Abandon any open streaming writer without flushing pending scratch
    /// samples, for use when a conversion is aborted (error or user
    /// cancellation) and the partial output file is about to be deleted
    /// anyway. Dropping the writer still triggers its internal `Drop` impl,
    /// but since the file is discarded immediately afterward any such cost
    /// is harmless.
    pub fn abort_stream(&mut self) {
        self.stream_writer = None;
    }

    pub fn set_scaling(&mut self, volume: f64, upsample_ratio: u32) {
        let vol_scale = 10.0f64.powf(volume / 20.0);

        if self.bits != 32 {
            self.scale_factor = 2.0f64.powi(self.bits as i32 - 1);
        }

        self.peak_level = self.scale_factor.floor() as i32;
        self.scale_factor *= vol_scale;
        self.scale_factor *= upsample_ratio as f64
    }

    pub fn pack_float(&mut self, offset: &mut usize, sample: f64) {
        // Convert to f32 and write in little-endian
        let bytes = (sample as f32).to_le_bytes();
        self.stdout_buf[*offset..*offset + 4].copy_from_slice(&bytes);
        *offset += 4;
    }

    pub fn pack_int(&mut self, offset: &mut usize, value: i32) {
        if *offset + self.bytes_per_sample > self.stdout_buf.len() {
            return;
        }

        match self.bytes_per_sample {
            3 => {
                // 24-bit container (also used for 20-bit). For 20-bit we left-align by shifting 4.
                let mut v = value;
                if self.bits == 20 {
                    v <<= 4; // align 20 significant bits into the top of 24-bit word (LS 4 bits zero)
                }
                self.stdout_buf[*offset] = (v & 0xFF) as u8;
                self.stdout_buf[*offset + 1] = ((v >> 8) & 0xFF) as u8;
                self.stdout_buf[*offset + 2] = ((v >> 16) & 0xFF) as u8;
            }
            2 => {
                let v = value as i16;
                let b = v.to_le_bytes();
                self.stdout_buf[*offset..*offset + 2].copy_from_slice(&b);
            }
            _ => return,
        }
        *offset += self.bytes_per_sample;
    }

    pub fn write_stdout(
        &mut self,
        pcm_bytes: usize,
    ) -> Result<(), Box<dyn Error>> {
        if pcm_bytes == 0 || pcm_bytes > self.stdout_buf.len() {
            return Ok(());
        }

        io::stdout().write_all(&self.stdout_buf[..pcm_bytes])?;
        io::stdout().flush()?;
        Ok(())
    }

    #[inline(always)]
    pub fn write_to_buffer(
        &mut self,
        samples_used_per_chan: usize,
        chan: usize,
    ) {
        // Output / packing for channel
        if self.output == OutputType::Stdout {
            // Interleave into pcm_data (handle float vs integer separately)
            let bps = self.bytes_per_sample; // 4 for 32-bit float
            let mut pcm_pos = chan * bps;
            for s in 0..samples_used_per_chan {
                let mut out_idx = pcm_pos;
                if self.bits == 32 {
                    // 32-bit float path
                    let mut q = self.float_data[s];
                    self.scale_and_dither(&mut q);
                    self.pack_float(&mut out_idx, q);
                } else {
                    // Integer path: dither + clamp + write_int
                    let mut qin: f64 = self.float_data[s];
                    self.scale_and_dither(&mut qin);
                    let quantized = self.quantize(&mut qin);
                    self.pack_int(&mut out_idx, quantized);
                }
                pcm_pos += self.channels_num * bps;
            }
        } else if self.bits == 32 {
            for s in 0..samples_used_per_chan {
                let mut q = self.float_data[s];
                self.scale_and_dither(&mut q);
                if let Some(w) = self.stream_writer.as_mut() {
                    w.push_sample_f32(chan, q as f32);
                }
            }
        } else {
            for s in 0..samples_used_per_chan {
                let mut qin: f64 = self.float_data[s];
                self.scale_and_dither(&mut qin);
                let quantized = self.quantize(&mut qin);
                if let Some(w) = self.stream_writer.as_mut() {
                    w.push_sample(chan, quantized);
                }
            }
        }
    }

    #[inline(always)]
    fn scale_and_dither(&mut self, sample: &mut f64) {
        *sample *= self.scale_factor;
        self.dither.process_samp(sample);
    }

    // Helper function for clip stats
    #[inline(always)]
    fn update_clip_stats(&mut self, low: bool, high: bool) {
        if low {
            if self.last_samps_clipped_low == 1 {
                self.clips += 1;
            }
            self.last_samps_clipped_low += 1;
            return;
        }
        self.last_samps_clipped_low = 0;
        if high {
            if self.last_samps_clipped_high == 1 {
                self.clips += 1;
            }
            self.last_samps_clipped_high += 1;
            return;
        }
        self.last_samps_clipped_high = 0;
    }

    #[inline(always)]
    pub fn quantize(&mut self, qin: &mut f64) -> i32 {
        let value = Self::my_round(*qin) as i32;
        let peak = self.peak_level as i32;
        self.clamp_value(-peak, value, peak - 1)
    }

    #[inline(always)]
    fn clamp_value(&mut self, min: i32, value: i32, max: i32) -> i32 {
        return if value < min {
            self.update_clip_stats(true, false);
            min
        } else if value > max {
            self.update_clip_stats(false, true);
            max
        } else {
            self.update_clip_stats(false, false);
            value
        };
    }

    #[inline(always)]
    fn my_round(x: f64) -> i64 {
        if x < 0.0 {
            (x - 0.5).floor() as i64
        } else {
            (x + 0.5).floor() as i64
        }
    }
}

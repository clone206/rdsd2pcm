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

//! A library for converting DSD to PCM.
//! Logging implemented via log crate.
//! Reads DSD from stdin or file, writes PCM to stdout or file.

mod audio_file;
mod byte_precalc_decimator;
mod conversion_context;
mod dither;
mod dsd_file;
mod dsd_reader;
mod filters;
mod filters_lm;
mod lm_resampler;
mod pcm_writer;

use std::{error::Error, fs, io, path::PathBuf, sync::mpsc};

use crate::{
    conversion_context::ConversionContext, dither::Dither,
    dsd_reader::DsdReader, lm_resampler::compute_decim_and_upsample,
    pcm_writer::PcmWriter,
};

pub use crate::dsd_reader::DsdRate;
pub use crate::dsd_file::{DsdFileFormat, FormatExtensions};

/// `100.0`
pub const ONE_HUNDRED_PERCENT: f32 = 100.0;
/// `["dsf", "dff", "dsd"]`
pub const DSD_EXTENSIONS: [&str; 3] = ["dsf", "dff", "dsd"];

/// Main Rdsd2Pcm conversion struct
pub struct Rdsd2Pcm {
    conv_ctx: ConversionContext,
}

impl Rdsd2Pcm {
    /// Create a new Rdsd2Pcm conversion context.
    /// Certain input parameters will be overriden when
    /// loading a container file (a .dsf or .dff file). Consider using
    /// `Rdsd2Pcm::from_container` when converting from container files.
    /// * `bit_depth` - Output PCM bit depth
    /// * `out_type` - Output type (audio file or stdout)
    /// * `level_db` - Output level adjustment in dB
    /// * `out_rate` - Output PCM sample rate
    /// * `out_path` - Optional output path. Same as input file if None.
    /// * `dither_type` - Dither type to apply
    /// * `in_format` - Input DSD format (planar or interleaved). Will be overridden when loading container file.
    /// * `endianness` - Input DSD endianness (most significant bit first, or least significant bit first). Will be overridden when loading container file.
    /// * `dsd_rate` - Input DSD sample rate (DSD64, DSD128, or DSD256 allowed). Will be overridden when loading container file.
    /// * `in_block_size` - Input DSD block size in bytes. Will be overridden when loading container file.
    /// * `num_channels` - Number of input channels. Will be overridden when loading container file.
    /// * `filt_type` - Filter type to use for conversion
    /// * `append_rate_suffix` - Whether to append the sample rate to output file names and album tags
    /// * `base_dir` - Base directory for output files' relative paths
    /// * `in_path` - Optional path to input DSD file. `stdin` assumed if None. .dsd files are considered raw DSD.
    pub fn new(
        bit_depth: i32,
        out_type: OutputType,
        level_db: f64,
        out_rate: u32,
        out_path: Option<PathBuf>,
        dither_type: DitherType,
        in_format: FmtType,
        endianness: Endianness,
        dsd_rate: DsdRate,
        in_block_size: u32,
        num_channels: u32,
        filt_type: FilterType,
        append_rate_suffix: bool,
        base_dir: PathBuf,
        in_path: Option<PathBuf>,
    ) -> Result<Self, Box<dyn Error>> {
        let dsd_reader = DsdReader::new(
            in_path.clone(),
            in_format,
            endianness,
            dsd_rate,
            in_block_size,
            num_channels,
        )?;

        Self::delegate_new(
            dsd_reader,
            bit_depth,
            out_type,
            level_db,
            out_rate,
            out_path,
            dither_type,
            filt_type,
            append_rate_suffix,
            base_dir,
        )
    }

    /// Create a new Rdsd2Pcm conversion context using a container file as input.
    /// * `bit_depth` - Output PCM bit depth
    /// * `out_type` - Output type (audio file or stdout)
    /// * `level_db` - Output level adjustment in dB
    /// * `out_rate` - Output PCM sample rate
    /// * `out_path` - Optional output path. Same as input file if None.
    /// * `dither_type` - Dither type to apply
    /// * `filt_type` - Filter type to use for conversion
    /// * `append_rate_suffix` - Whether to append the sample rate to output file names and album tags
    /// * `base_dir` - Base directory for output files' relative paths
    /// * `in_path` - Path to input DSD container file (e.g., .dsf or .dff)
    pub fn from_container(
        bit_depth: i32,
        out_type: OutputType,
        level_db: f64,
        out_rate: u32,
        out_path: Option<PathBuf>,
        dither_type: DitherType,
        filt_type: FilterType,
        append_rate_suffix: bool,
        base_dir: PathBuf,
        in_path: PathBuf,
    ) -> Result<Self, Box<dyn Error>> {
        let dsd_reader = DsdReader::from_container(in_path.clone())?;

        Self::delegate_new(
            dsd_reader,
            bit_depth,
            out_type,
            level_db,
            out_rate,
            out_path,
            dither_type,
            filt_type,
            append_rate_suffix,
            base_dir,
        )
    }

    fn delegate_new(
        dsd_reader: DsdReader,
        bit_depth: i32,
        out_type: OutputType,
        level_db: f64,
        out_rate: u32,
        out_path: Option<PathBuf>,
        dither_type: DitherType,
        filt_type: FilterType,
        append_rate_suffix: bool,
        base_dir: PathBuf,
    ) -> Result<Self, Box<dyn Error>> {
        let (decim_ratio, upsample_ratio) =
            compute_decim_and_upsample(dsd_reader.dsd_rate(), out_rate);
        let out_frames_capacity = Self::calc_frames_cap(
            dsd_reader.block_size() as usize,
            decim_ratio,
            upsample_ratio,
        );

        let pcm_writer = PcmWriter::new(
            bit_depth,
            out_type,
            level_db,
            out_rate,
            out_path.clone(),
            Dither::new(dither_type)?,
            out_frames_capacity,
            dsd_reader.channels_num(),
            upsample_ratio,
        )?;

        let conv_ctx = ConversionContext::new(
            dsd_reader,
            pcm_writer,
            filt_type,
            append_rate_suffix,
            base_dir,
            decim_ratio,
            upsample_ratio,
        )?;

        let rdsd2pcm = Self { conv_ctx };

        Ok(rdsd2pcm)
    }

    /// Worst-case frames per channel per input block:
    /// ceil((bits_in * L) / M). Add small slack for LM paths to avoid edge truncation.
    fn calc_frames_cap(
        block_size: usize,
        decim: i32,
        upsample: u32,
    ) -> usize {
        let bits_per_chan = block_size * 8;
        let frames_max = ((bits_per_chan * (upsample as usize))
            + (decim.abs() as usize - 1))
            / (decim.abs() as usize);
        let lm_slack = if upsample > 1 { 16 } else { 0 };
        frames_max + lm_slack
    }

    /// Perform the conversion from DSD to PCM
    /// * `percent_sender` - Optional channel sender for percentage progress updates.
    pub fn do_conversion(
        &mut self,
        percent_sender: Option<mpsc::Sender<f32>>,
    ) -> Result<(), Box<dyn Error>> {
        self.conv_ctx.do_conversion(percent_sender)
    }

    /// Get the input file name (or empty string for stdin)
    pub fn file_name(&self) -> String {
        self.conv_ctx.file_name_lossy()
    }
}

/// DSD bit endianness
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum Endianness {
    LsbFirst,
    MsbFirst,
}

/// DSD channel format
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum FmtType {
    /// Block per channel
    Planar,
    /// Byte per channel
    Interleaved,
}

/// Output dither type
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum DitherType {
    /// Triangular probability density function dither
    TPDF,
    /// Airwindows floating-point dither.
    /// Randomizes when casting from the internal f64 sample values to f32
    /// for 32 bit float outputs.
    FPD,
    Rectangular,
    None,
}

/// Decimation filter type
#[derive(Copy, Clone, PartialEq, Debug, Default)]
pub enum FilterType {
    /// From the original dsd2pcm c library (only for DSD64 to 352.8kHz output)
    Dsd2Pcm,
    /// Standard windowed sinc lowpass filter with equiripple design.
    /// Available for all input and output rates supported by the library.
    #[default]
    Equiripple,
    /// Inverse chebyshev. Only available for DSD128 to 88.2kHz multiples
    Chebyshev,
    /// Copied over from XLD. Only for DSD64 to 88.2kHz multiples
    XLD,
}

/// Output type to write. Either standard output or file.
#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub enum OutputType {
    /// Raw PCM to stdout
    Stdout,
    Wav,
    Aiff,
    Flac,
}

/// Find all DSD files in the provided paths, optionally recursing into directories
pub fn find_dsd_files(
    paths: &[PathBuf],
    recurse: bool,
) -> io::Result<Vec<PathBuf>> {
    let mut file_paths = Vec::new();
    for path in paths {
        if path.is_dir() {
            if recurse {
                // Recurse into all directory entries
                let entries: Vec<PathBuf> = fs::read_dir(path)?
                    .filter_map(|e| e.ok().map(|d| d.path()))
                    .collect();
                file_paths.extend(find_dsd_files(&entries, recurse)?);
            } else {
                // Non-recursive: include only top-level files that are DSD
                for entry in fs::read_dir(path)? {
                    let entry_path = entry?.path();
                    if entry_path.is_file() && is_dsd_file(&entry_path) {
                        file_paths
                            .push(entry_path.canonicalize()?.clone());
                    }
                }
            }
        } else if path.is_file() && is_dsd_file(path) {
            // Single push site for matching files
            file_paths.push(path.canonicalize()?.clone());
        }
    }
    file_paths.sort();
    file_paths.dedup();
    Ok(file_paths)
}

/// Check if the provided path is a DSD file based on its extension.
/// True if extension in `rdsd2pcm::DSD_EXTENSIONS`.
pub fn is_dsd_file(path: &PathBuf) -> bool {
    if path.is_file()
        && let Some(ext) = path.extension()
        && let ext_lower = ext.to_ascii_lowercase().to_string_lossy()
        && DSD_EXTENSIONS.contains(&ext_lower.as_ref())
    {
        return true;
    }
    false
}

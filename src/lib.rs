//! A library for converting DSD to PCM.
//! Logging implemented via log crate.
//! Reads DSD from stdin or file, writes PCM to stdout or file.

mod audio_file;
mod byte_precalc_decimator;
mod conversion_context;
mod dither;
mod dsd;
mod filters;
mod filters_lm;
mod input;
mod lm_resampler;
mod output;

use std::{error::Error, fs, io, path::PathBuf, sync::mpsc};

use crate::{
    conversion_context::ConversionContext, dither::Dither,
    input::InputContext, output::OutputContext,
};

pub const ONE_HUNDRED_PERCENT: f32 = 100.0;
/// `["dsf", "dff", "dsd"]`
pub const DSD_EXTENSIONS: [&str; 3] = ["dsf", "dff", "dsd"];

pub struct Rdsd2Pcm {
    in_format: FmtType,
    endianness: Endianness,
    dsd_rate: i32,
    in_block_size: u32,
    num_channels: u32,
    std_in: bool,
    filt_type: FilterType,
    append_rate_suffix: bool,
    base_dir: PathBuf,
    out_ctx: OutputContext,
    in_ctx: Option<InputContext>,
    in_file_name: String,
}

impl Rdsd2Pcm {
    /// Create a new Rdsd2Pcm conversion context.
    /// Certain input parameters will be overriden when loading a container file (e.g. .dsf or .dff)
    /// with `load_input`.
    /// * `bit_depth` - Output PCM bit depth
    /// * `out_type` - Output type (file or stdout)
    /// * `level_db` - Output level adjustment in dB
    /// * `out_rate` - Output PCM sample rate
    /// * `out_path` - Optional output path
    /// * `dither_type` - Dither type to apply
    /// * `in_format` - Input DSD format (planar or interleaved)
    /// * `endianness` - Input DSD endianness
    /// * `dsd_rate` - Input DSD sample rate
    /// * `in_block_size` - Input DSD block size in bytes
    /// * `num_channels` - Number of input channels
    /// * `filt_type` - Filter type to use for conversion
    /// * `append_rate_suffix` - Whether to append the sample rate to output file names and album tags
    /// * `base_dir` - Base directory for output files' relative paths
    pub fn new(
        bit_depth: i32,
        out_type: OutputType,
        level_db: f64,
        out_rate: i32,
        out_path: Option<PathBuf>,
        dither_type: DitherType,
        in_format: FmtType,
        endianness: Endianness,
        dsd_rate: i32,
        in_block_size: u32,
        num_channels: u32,
        filt_type: FilterType,
        append_rate_suffix: bool,
        base_dir: PathBuf,
    ) -> Result<Self, Box<dyn Error>> {
        let out_ctx = OutputContext::new(
            bit_depth,
            out_type,
            level_db,
            out_rate,
            out_path.clone(),
            Dither::new(dither_type)?,
        )?;

        let rdsd2pcm = Self {
            in_format,
            endianness,
            dsd_rate,
            in_block_size,
            num_channels,
            std_in: true,
            filt_type,
            append_rate_suffix,
            base_dir,
            in_file_name: "".to_string(),
            out_ctx,
            in_ctx: None,
        };

        Ok(rdsd2pcm)
    }

    /// Load the input file from path or set up for stdin if None
    /// * `path` - Optional path to input DSD file. .dsd files are considered raw DSD.
    pub fn load_input(
        &mut self,
        path: Option<PathBuf>,
    ) -> Result<(), Box<dyn Error>> {
        self.std_in = path.is_none();
        let in_ctx = InputContext::new(
            path,
            self.in_format,
            self.endianness,
            self.dsd_rate,
            self.in_block_size,
            self.num_channels,
            self.std_in,
        )?;

        self.in_file_name =
            in_ctx.file_name().to_string_lossy().into_owned();
        self.in_ctx = Some(in_ctx);
        Ok(())
    }

    /// Perform the conversion from DSD to PCM
    /// * `percent_sender` - Optional channel sender for percentage progress updates.
    /// The receiver should be explicitly dropped when received value is `100.0` (`rdsd2pcm::ONE_HUNDRED_PERCENT`).
    pub fn do_conversion(
        &mut self,
        percent_sender: Option<mpsc::Sender<f32>>,
    ) -> Result<(), Box<dyn Error>> {
        let Some(mut in_ctx) = self.in_ctx.take() else {
            return Err("Input context not initialized".into());
        };
        in_ctx.init()?;

        let mut conv_ctx = ConversionContext::new(
            in_ctx,
            self.out_ctx.clone(),
            self.filt_type,
            self.append_rate_suffix,
            self.base_dir.clone(),
        )?;
        conv_ctx.do_conversion(percent_sender)
    }

    /// Get the input file name (or empty string for stdin)
    pub fn file_name(&self) -> String {
        self.in_file_name.clone()
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

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum FilterType {
    /// From the original dsd2pcm c library (only for 352.8kHz output)
    Dsd2Pcm,
    Equiripple,
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

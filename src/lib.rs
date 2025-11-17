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

use std::path::PathBuf;

pub use dither::Dither;

use crate::{conversion_context::FilterType, dither::DitherType, input::{Endianness, FmtType}, output::OutputType};

pub struct Rdsd2Pcm {
    bits: i32,
    output: OutputType,
    level: f64,
    rate: i32,
    out_path: Option<PathBuf>,
    dither_type: DitherType,
    in_path: Option<PathBuf>,
    format: FmtType,
    endian: Endianness,
    dsd_rate: i32,
    block_size: u32,
    channels: u32,
    std_in: bool,
    filt_type: FilterType,
    append_rate_suffix: bool,
    base_dir: PathBuf,
}

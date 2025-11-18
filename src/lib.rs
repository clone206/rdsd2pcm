mod audio_file;
mod byte_precalc_decimator;
mod conversion_context;
mod dither;
pub mod dsd;
mod filters;
mod filters_lm;
mod input;
mod lm_resampler;
mod output;

use std::{error::Error, path::PathBuf, sync::mpsc};

pub use dither::Dither;

use crate::{conversion_context::ConversionContext, input::InputContext, output::OutputContext};

pub const ONE_HUNDRED_PERCENT: f32 = 100.0;

pub struct Rdsd2Pcm {
    format: FmtType,
    endian: Endianness,
    dsd_rate: i32,
    block_size: u32,
    channels: u32,
    std_in: bool,
    filt_type: FilterType,
    append_rate_suffix: bool,
    base_dir: PathBuf,
    out_ctx: OutputContext,
    in_ctx: Option<InputContext>,
    file_name: String,
}

impl Rdsd2Pcm {
    pub fn file_name(&self) -> String {
        self.file_name.clone()
    }

    pub fn new(
        bits: i32,
        output: OutputType,
        level: f64,
        rate: i32,
        out_path: Option<PathBuf>,
        dither_type: DitherType,
        format: FmtType,
        endian: Endianness,
        dsd_rate: i32,
        block_size: u32,
        channels: u32,
        filt_type: FilterType,
        append_rate_suffix: bool,
        base_dir: PathBuf,
    ) -> Result<Self, Box<dyn Error>> {
        let out_ctx = OutputContext::new(
            bits,
            output,
            level,
            rate,
            out_path.clone(),
            Dither::new(dither_type)?,
        )?;

        let rdsd2pcm = Self {
            format,
            endian,
            dsd_rate,
            block_size,
            channels,
            std_in: true,
            filt_type,
            append_rate_suffix,
            base_dir,
            file_name: "".to_string(),
            out_ctx,
            in_ctx: None,
        };

        Ok(rdsd2pcm)
    }

    pub fn load_input(&mut self, path: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
        self.std_in = path.is_none();
        let in_ctx = InputContext::new(
            path,
            self.format,
            self.endian,
            self.dsd_rate,
            self.block_size,
            self.channels,
            self.std_in,
        )?;

        self.file_name = in_ctx.file_name().to_string_lossy().into_owned();
        self.in_ctx = Some(in_ctx);
        Ok(())
    }

    pub fn do_conversion(
        &mut self,
        sender: Option<mpsc::Sender<f32>>,
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
        conv_ctx.do_conversion(sender)
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum Endianness {
    LsbFirst,
    MsbFirst,
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum FmtType {
    Planar,
    Interleaved,
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum DitherType {
    TPDF,
    FPD,
    Rectangular,
    None,
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum FilterType {
    Dsd2Pcm,
    Equiripple,
    Chebyshev,
    XLD,
}

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub enum OutputType {
    Stdout,
    Wav,
    Aiff,
    Flac,
}

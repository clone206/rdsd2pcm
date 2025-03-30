use dsd2pcm::Dsd2Pcm;
use std::io::{Read, Write};
mod noise_shaper;
use noise_shaper::NoiseShaper;

// Constants for noise shaper
const NS_SOSCOUNT: usize = 2;
const NS_COEFFS: [f32; 8] = [
    -1.62666423,
    0.79410094,
    0.61367127,
    0.23311013, // section 1
    -1.44870017,
    0.54196219,
    0.03373857,
    0.70316556, // section 2
];

fn main() {
    const BLOCK: usize = 4096; // Block size for planar DSD format
    let args: Vec<String> = std::env::args().collect();

    if args.len() != 4 {
        eprintln!(
            "\nDSD2PCM filter (raw DSD64 --> 352 kHz raw PCM)\n\
            (c) 2009 Sebastian Gesemann\n\n\
            (filter as in \"reads data from stdin and writes to stdout\")\n\n\
            Syntax: dsd2pcm <channels> <bitorder> <bitdepth>\n\
            channels = 1,2,3,...,9 (number of channels in DSD stream)\n\
            bitorder = L (lsb first), M (msb first) (DSD stream option)\n\
            bitdepth = 16 or 24 (intel byte order, output option)\n\n\
            Note: At 16 bits/sample a noise shaper kicks in that can preserve\n\
            a dynamic range of 135 dB below 30 kHz.\n"
        );
        std::process::exit(1);
    }

    let channels: usize = args[1].parse().unwrap_or(0);
    let lsbitfirst: i32 = match args[2].to_lowercase().as_str() {
        "l" => 1,
        "m" => 0,
        _ => -1,
    };
    let bits: i32 = match args[3].as_str() {
        "16" => 16,
        "24" => 24,
        _ => -1,
    };

    if channels < 1 || lsbitfirst < 0 || bits < 0 {
        eprintln!("Invalid arguments.");
        std::process::exit(1);
    }

    let bytes_per_sample = (bits / 8) as usize;
    // Allocate buffer for all channels in planar format
    let mut dsd_data = vec![0u8; BLOCK * channels];
    let mut float_data = vec![0f32; BLOCK];
    let mut pcm_data = vec![0u8; BLOCK * channels * bytes_per_sample];

    // Create DSD2PCM contexts for each channel
    let mut dsd2pcm_contexts: Vec<Dsd2Pcm> = (0..channels)
        .map(|_| Dsd2Pcm::new().expect("Failed to create DSD2PCM context"))
        .collect();

    // Create noise shapers if using 16-bit output
    let mut noise_shapers = if bits == 16 {
        (0..channels)
            .map(|_| NoiseShaper::new(NS_SOSCOUNT, &NS_COEFFS))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    // Read planar data: one full block per channel
    while let Ok(bytes_read) = std::io::stdin().read_exact(&mut dsd_data) {
        for c in 0..channels {
            // Process each channel's block of data
            dsd2pcm_contexts[c].translate(
                BLOCK,
                &dsd_data[c * BLOCK..], // Start of this channel's block
                1,                      // Stride is 1 for planar format
                lsbitfirst == 1,
                &mut float_data,
                1,
            );

            // Write PCM data with correct interleaving
            let mut out_idx = c * bytes_per_sample;
            for sample in float_data.iter().take(BLOCK) {
                if bits == 16 {
                    let mut shaped_sample = *sample;
                    noise_shapers[c].update(shaped_sample);
                    shaped_sample += noise_shapers[c].get();

                    let pcm = (shaped_sample * 32768.0).max(-32768.0).min(32767.0) as i16;
                    pcm_data[out_idx] = pcm as u8;
                    pcm_data[out_idx + 1] = (pcm >> 8) as u8;
                } else {
                    let pcm = (*sample * 8388608.0).max(-8388608.0).min(8388607.0) as i32;
                    pcm_data[out_idx] = pcm as u8;
                    pcm_data[out_idx + 1] = (pcm >> 8) as u8;
                    pcm_data[out_idx + 2] = (pcm >> 16) as u8;
                }
                out_idx += channels * bytes_per_sample; // Maintain interleaved output
            }
        }

        std::io::stdout()
            .write_all(&pcm_data[..BLOCK * channels * bytes_per_sample])
            .unwrap();
    }
}

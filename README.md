# dsd2pcm/dsd2pcm/README.md

# rdsd2pcm

rdsd2pcm is a Rust application that converts raw DSD64 audio data into 352 kHz raw PCM format. This project implements a filter that reads data from standard input and writes the converted PCM data to standard output. It is a rust implementation of the dsd2pcm application, using the C library directly.

## Features

- Supports multiple channels (1 to 9).
- Configurable bit order (LSB first or MSB first).
- Configurable bit depth (16 or 24 bits).
- Implements a noise shaping algorithm for 16 bit output to preserve dynamic range.
- Assumes planar format DSD with a block size of 4096 bytes

## Build

`cargo build`

## Usage

To run the application, use the following syntax:

```
./target/debug/dsd2pcm <channels> <bitorder> <bitdepth>
```

### Parameters

- `channels`: Number of channels in the DSD stream (1, 2, 3, ..., 9).
- `bitorder`: Specify 'L' for LSB first or 'M' for MSB first.
- `bitdepth`: Specify '16' for 16 bits or '24' for 24 bits.

### Note

At 16 bits/sample, a noise shaper is activated to preserve a dynamic range of 135 dB below 30 kHz.

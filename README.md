# cri

CRI Middleware's containers and codecs in plain Rust.

| module | what                                                                     |
| ------ | ------------------------------------------------------------------------ |
| `utf`  | `@UTF` tables, the format inside ACB, ACF and USM headers                |
| `acb`  | cue banks: cue sheets, waveforms, and the cue programs (`acb::program`)  |
| `acf`  | project settings: categories, DSP buses and their effects, AISACs, REACT |
| `asr`  | the sound renderer's mix, offline: cues through an ACF's bus graph       |
| `afs2` | AWB archives, and the subkey they carry                                  |
| `adx`  | ADX decoding, including type 9 encryption                                |
| `hca`  | HCA decoding, including cipher types 0, 1 and 56                         |
| `usm`  | USM demuxing and unmasking; `usm::ivf` frames its VP9 streams            |

Every fallible function returns `cri::Error`, which says which format the input broke and how.

## Tools

`cli/` builds `cri`, a command-line tool over the crate: `cargo install --path cli`.

| command   | what                                                                     |
| --------- | ------------------------------------------------------------------------ |
| `info`    | what each file is, at a glance                                           |
| `utf`     | an ACB's or ACF's `@UTF` tables, their schemas and rows                  |
| `cues`    | an ACB's cues, one line each                                             |
| `cue`     | one cue's program: parameters, tracks, layers, AISACs and action tracks  |
| `waves`   | an ACB's waveforms, beside their bitstreams' own headers                 |
| `awb`     | an AWB's entries                                                         |
| `acf`     | an ACF's categories, buses, effects, AISACs, REACT rules and selectors   |
| `usm`     | a USM's streams and video format                                         |
| `decode`  | an HCA or ADX file to WAV                                                |
| `extract` | every waveform of an ACB or AWB, to WAV or as stored                     |
| `demux`   | a USM's video, audio and alpha streams                                   |

Encrypted files need the keycode, given with `--key` or the `CRI_KEY` environment variable.

## Accuracy

The ADX and HCA decoders produce the same `f32` samples as CRI's own runtime, bit for bit, on the
files they were checked with. Keep it that way: no `f32::mul_add`, and no reordering of
floating-point operations.

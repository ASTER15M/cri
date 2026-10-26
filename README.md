# cri

CRI Middleware's containers and codecs in plain Rust.

| module | what                                                                    |
| ------ | ----------------------------------------------------------------------- |
| `utf`  | `@UTF` tables, the format inside ACB, ACF and USM headers               |
| `acb`  | cue banks: cue sheets, waveforms, and the cue programs (`acb::program`) |
| `afs2` | AWB archives, and the subkey they carry                                 |
| `adx`  | ADX decoding, including type 9 encryption                               |
| `hca`  | HCA decoding, including cipher types 0, 1 and 56                        |
| `usm`  | USM demuxing and unmasking; `usm::ivf` frames its VP9 streams           |

Every fallible function returns `cri::Error`, which says which format the input broke and how.

## Accuracy

The ADX and HCA decoders produce the same `f32` samples as CRI's own runtime, bit for bit, on the
files they were checked with. Keep it that way: no `f32::mul_add`, and no reordering of
floating-point operations.

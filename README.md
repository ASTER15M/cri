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

| command   | what                                                                  |
| --------- | --------------------------------------------------------------------- |
| `ls`      | the CRI files in a directory or among files, one line each            |
| `info`    | what a file holds: cues, waveforms, archive entries, buses or streams |
| `utf`     | an ACB's or ACF's `@UTF` tables, their schemas and rows               |
| `extract` | audio and video out of a file, decoded to WAV or as stored            |

Encrypted files need the keycode, given with `--key` or the `CRI_KEY` environment variable.

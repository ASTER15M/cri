# Changelog

Every release is tagged `v<version>`. Versions follow semver; before 1.0, a minor bump (0.1 → 0.2)
is a breaking change and a patch bump (0.1.0 → 0.1.1) is not.

## Unreleased

- **Breaking:** `acb::program::CueSheet`, `asr::AsrMixer` and `asr::AsrStream` own what they read
  and have no lifetime. `CueSheet::open` takes the `Acb` itself; `AsrMixer::new` takes
  `Vec<Arc<CueSheet>>` and an `Arc<Acf>`. A stream can then go to another thread without its sheets
  and ACF living for the whole program, and a sheet can serve several mixers.

## 0.3.0

- **Breaking:** `asr::Approximated` no longer has `pitched_plays`, `random_pitch_plays` or
  `off_rate_layers`: none of them is an approximation any more.
- `asr` resamples a voice at a pitch or at another sample rate as the library does. Its frequency is
  the waveform's rate times `2^(cents/1200)`, truncated to whole hertz; it steps through the source
  in 1/4096ths of a frame against the output rate and interpolates linearly between the two frames
  around each step. A resampling voice ends with the production block whose pull takes its last
  source frame. Layers at another sample rate were dropped before.
- `asr::AsrStream` is the mixer as a stream, for a live player: `AsrMixer::stream` starts one,
  `push` takes each command no earlier than `next_tick`, and `fill` hands out the next frames,
  rendering whole production blocks as it needs them. `AsrMixer::render` is now a stream with every
  command pushed first, and renders the same samples as before.

## 0.2.1

- `asr::AsrMixer::add_cue` plays a cue of the caller's own: a `CueProgram` built in code, its layers
  playing given samples rather than an ACB's waveforms. It returns the sheet index to play it by.
- `asr::Waveform`, the samples a layer plays, is public.

## 0.2.0

- **Breaking:** `acb::program` decodes every parameter command. Those with a known meaning have
  their own `CueParam` variant: cue limits, combo-sequential settings, the replay interval, biquad
  and band-pass filters, envelope stages, selectors and selector labels. Any other is kept as
  `CueParam::Other` instead of failing the program, so a player has to refuse what it does not
  model; `CueProgram::all_params` lists every scope's parameters for that check.
- **Breaking:** `CueParam` and the new enums are `#[non_exhaustive]`.
- **Breaking:** `CommandTable` has a `Category` variant, an ACF's `CommandTable`.
- Random, random-no-repeat and combo-sequential sequences resolve, with their tracks' weights
  (`ProgramTrack::weight`) and `CueProgram::sequence_type`; a player picks the track.
- Note-ons into another sequence resolve as `ProgramTrack::nested`, which includes a song preview's
  second sequence.
- Action tracks that start and stop another cue resolve as `CueProgram::action_tracks`.
- Local AISACs at sequence and synth scope, and global AISAC references at sequence and track scope,
  are listed instead of ignored or refused.
- A bus send to an empty bus name resolves; it reaches no bus.
- `acf` reads ACF project settings in both the named-effect layout and the older numbered one:
  categories with their commands and global AISACs, each DSP bus setting's buses with their
  volumes, effects (`Effect::matrix` decodes a `Matrix`) and sends, global AISACs with their graphs,
  the bus list AISAC graph types count through, REACT rules, selectors and voice limit groups.
- `CueParam::CueLimit::kind` is optional: a category's code 73 carries none.
- `asr` renders the Atom sound renderer's float output offline: sound commands in, planes out, on
  the native pass grid. Voices mix into the buses of an ACF's DSP setting, run in graph order once
  per production block: a bus is live when a send reaches it with a non-zero gain or an `Echo`
  still has a tail, only a live bus adds the anti-denormal pulse and sends, and a category's global
  AISACs scale the sends their graphs target. `Approximated` counts what is rendered other than the
  library would; cues with features it does not model fail with `Error::Unrenderable`.
- `Error::Unrenderable` and `Error::Command`, for `asr`.
- `acf::GlobalAisac::default_control` replaces `has_default` and carries the value.
- `acb::parse_commands` is public, and so are `Utf`'s typed getters and `Value::as_i64` and
  `as_bytes`. `acb` re-exports `AwbSource`, `CueProgram` and `Layer`.
- `cli/` builds `cri`, a command-line tool to list and explore ACB, ACF, AWB and USM files and to
  decode or extract their audio and video streams.

## 0.1.0

First release.

- ACB banks and their cue programs, AFS2 archives (AWB), UTF tables
- ADX and HCA decoders, including ADX type 9 and HCA cipher 56, matching CRI's own `f32` output
  bit for bit
- USM demuxing and unmasking, and IVF framing of its VP9 streams
- One error type, `cri::Error`, for every fallible function
- Keycodes are always the caller's; none is built in.

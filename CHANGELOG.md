# Changelog

Every release is tagged `v<version>`. Versions follow semver; before 1.0, a minor bump (0.1 → 0.2)
is a breaking change and a patch bump (0.1.0 → 0.1.1) is not.

## Unreleased

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

## 0.1.0

First release.

- ACB banks and their cue programs, AFS2 archives (AWB), UTF tables
- ADX and HCA decoders, including ADX type 9 and HCA cipher 56, matching CRI's own `f32` output
  bit for bit
- USM demuxing and unmasking, and IVF framing of its VP9 streams
- One error type, `cri::Error`, for every fallible function
- Keycodes are always the caller's; none is built in.

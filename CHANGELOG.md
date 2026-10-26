# Changelog

Every release is tagged `v<version>`. Versions follow semver; before 1.0, a minor bump (0.1 → 0.2)
is a breaking change and a patch bump (0.1.0 → 0.1.1) is not.

## Unreleased

## 0.1.0

First release.

- ACB banks and their cue programs, AFS2 archives (AWB), UTF tables
- ADX and HCA decoders, including ADX type 9 and HCA cipher 56, matching CRI's own `f32` output
  bit for bit
- USM demuxing and unmasking, and IVF framing of its VP9 streams
- One error type, `cri::Error`, for every fallible function
- Keycodes are always the caller's; none is built in.

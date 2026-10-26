# Developing cri

## Releasing

1. Move the changelog's `Unreleased` entries under the new version.
2. Set `version` in `Cargo.toml`; under 1.0, bump the minor version for any breaking change.
3. Commit as `release: vX.Y.Z`, then tag that commit: `git tag -a vX.Y.Z -m "cri X.Y.Z"`.
4. Push the commit and the tag: `git push origin master vX.Y.Z`.

A project moves to it by changing its `tag` and running `cargo update -p cri`.

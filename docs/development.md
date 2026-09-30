# Development and releases

Building, testing and releasing `workstats`. Back to the [README](../README.md).

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

The repository, installers, tests, and release artifacts are Rust-only.

Versions are not set by hand. A merged pull request labelled `major`, `minor`,
or `patch` decides the next one; a merge carrying none of those labels
releases nothing. [`cratis/release-action`](https://github.com/Cratis/release-action)
works out the version and cuts the GitHub release, then the release workflow
builds six native artifacts from it, executes every runnable binary, generates
SHA-256 checksums, and attaches them—no repository secrets required for the
binaries. The `HOMEBREW_TAP_DEPLOY_KEY` secret then publishes the formula to
[`woksin/homebrew-workstats`](https://github.com/woksin/homebrew-workstats) —
the tap the [Homebrew instructions](install.md#prebuilt-binaries) install from — and the job reads the
formula back afterwards to confirm it names the version just released. Without
that secret the step is skipped with a warning annotation, or fails the release
outright if this README advertises a tap that is not configured.

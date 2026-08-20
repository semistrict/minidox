# Contributing

This repository is an experimental systems project. Please open an issue before
starting a large change so its design and test environment can be discussed.

## Development

The workspace requires Rust 1.89 or newer. Run the platform-independent checks
before submitting a change:

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
```

Linux/KVM and guest-DAX changes should also run the relevant tests described in
the README. Tests must assert the intended correct behavior; a regression test
must fail before its fix rather than pass because a bug reproduces.

Vendored upstream code lives under `vendor/`. Preserve its license headers and
license files. Record downstream changes prominently in modified Apache-2.0
files, and update subtrees from the canonical upstream rather than copying files
without provenance.

## Licensing

By submitting a contribution to the original minidox code, you agree to license
it under Apache-2.0. Contributions to vendored files remain subject to the
license stated in that file and its upstream subtree.

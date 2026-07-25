## Summary

Describe the behavior changed and why.

## Validation

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] `cargo test --locked`
- [ ] `cargo build --locked --release`
- [ ] No API keys, authorization headers, Base64 image data, session contents, or local user paths are included
- [ ] Installation, configuration, protocol, or UI changes include the relevant manual or fault-injection evidence

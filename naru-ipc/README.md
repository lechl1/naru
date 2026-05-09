# naru-ipc

Types and helpers for interfacing with the [naru](https://github.com/lechl1/naru) Wayland compositor.

## Backwards compatibility

This crate follows the naru version.
It is **not** API-stable in terms of the Rust semver.
In particular, expect new struct fields and enum variants to be added in patch version bumps.

Use an exact version requirement to avoid breaking changes:

```toml
[dependencies]
naru-ipc = "=26.4.0"
```

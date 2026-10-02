# inkmark

A fast local Markdown editor for Linux/Wayland: split, code and live views over one rope-backed document. See [PLAN.md](PLAN.md).

## Build

```
cargo build --release
cargo run --release -- path/to/file.md
```

Runtime deps: a Wayland compositor, and `xdg-desktop-portal` for the file dialog. Build with `--features glow` to use the OpenGL renderer instead of wgpu.

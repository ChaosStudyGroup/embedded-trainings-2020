# `cargo-build` fails to link

If you have configured Cargo to use sccache then you'll need to disable sccache support. Unset the `RUSTC_WRAPPER` variable in your environment *before* opening VS code. Run `cargo clean` from the Cargo workspace you are working from (`beginner/apps` or `advanced/firmware`). Then open VS code.

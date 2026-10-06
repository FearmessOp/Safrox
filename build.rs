// SPDX-License-Identifier: BSD-2-Clause
//! Build script.
//!
//! Its only job is to tell Cargo that the kernel must be re-linked when the
//! linker script changes. Cargo tracks source files and Cargo.toml, but it
//! has no idea that `-C link-arg=-Tkernel/linker.ld` makes the build depend
//! on that file -- without this, editing the layout leaves a stale binary in
//! place and the next boot silently uses the old one.

fn main() {
    println!("cargo::rerun-if-changed=kernel/linker.ld");
    println!("cargo::rerun-if-changed=.cargo/config.toml");
}

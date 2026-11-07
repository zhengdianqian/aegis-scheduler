// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文

//! Publishes the BPF probe object into OUT_DIR so it can be embedded.
//! When the probe has not been built yet, an empty placeholder is written and
//! the daemon reports "probe object was not built" at runtime.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let probe = manifest.join("../../probe/dist/tempo.bpf.o");
    println!("cargo:rerun-if-changed={}", probe.display());

    let out = PathBuf::from(env::var("OUT_DIR").expect("out dir")).join("tempo.bpf.o");
    let bytes = fs::read(&probe).unwrap_or_default();
    fs::write(&out, bytes).expect("write embedded probe object");
}

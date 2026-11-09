// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文

//! Publishes the BPF probe object into OUT_DIR so it can be embedded.
//! Android builds require a non-empty probe object. Host builds may use an
//! empty placeholder for offline testing.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let probe = manifest.join("../../probe/dist/tempo.bpf.o");
    println!("cargo:rerun-if-changed={}", probe.display());

    let out = PathBuf::from(env::var("OUT_DIR").expect("out dir")).join("tempo.bpf.o");
    let bytes = if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        let bytes = fs::read(&probe)
            .expect("Android build requires probe/dist/tempo.bpf.o; build the probe first");
        assert!(
            !bytes.is_empty(),
            "Android build requires a non-empty probe/dist/tempo.bpf.o"
        );
        bytes
    } else {
        fs::read(&probe).unwrap_or_default()
    };
    fs::write(&out, bytes).expect("write embedded probe object");
}

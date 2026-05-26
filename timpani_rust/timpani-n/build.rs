/*
 * SPDX-FileCopyrightText: Copyright 2026 LG Electronics Inc.
 * SPDX-License-Identifier: MIT
 */

//! Build script for timpani-n.
//!
//! Responsibilities:
//!   1. Compile `proto/node_service.proto` into tonic client stubs (always).
//!   2. Compile BPF programs in `bpf/` into Rust skeletons (feature "bpf").
//!      - `bpf/schedstat.bpf.c` → schedstat.skel.rs
//!      - `bpf/sigwait.bpf.c` → sigwait.skel.rs
//!
//! Prerequisites
//! -------------
//! - `protoc` on PATH (or set the `PROTOC` env var).
//!   Ubuntu/Debian: sudo apt install -y protobuf-compiler
//!
//! - clang on PATH — required by libbpf-cargo to compile BPF programs.
//!   Ubuntu/Debian: sudo apt install -y clang
//!   (Only needed when building with the default "bpf" feature.)

fn main() -> Result<(), Box<dyn std::error::Error>> {
    compile_proto()?;

    #[cfg(feature = "bpf")]
    compile_bpf()?;

    Ok(())
}

/// Compile `proto/node_service.proto` → tonic gRPC client stubs.
///
/// timpani-n is a pure gRPC client — it calls Timpani-O's NodeService.
/// However, we enable server generation for testing purposes (mock servers).
fn compile_proto() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = "./proto";
    let proto_file = format!("{proto_root}/node_service.proto");

    // Re-run this build script if the proto changes.
    println!("cargo:rerun-if-changed={proto_file}");

    tonic_build::configure()
        .build_server(true) // Enable for mock servers in tests
        .build_client(true)
        .compile_protos(&[proto_file.as_str()], &[proto_root])?;

    Ok(())
}

/// Compile BPF programs in `bpf/` directory → `.skel.rs` files in OUT_DIR.
///
/// libbpf-cargo invokes clang to produce BPF bytecode, then generates
/// Rust skeleton structs that embed the bytecode and expose type-safe
/// map and program accessors.
///
/// Generated files:
///   - `schedstat.skel.rs` (SchedstatSkel)
///   - `sigwait.skel.rs` (SigwaitSkel)
///
/// These are pulled into the crate via:
///   `include!(concat!(env!("OUT_DIR"), "/schedstat.skel.rs"))`
///   `include!(concat!(env!("OUT_DIR"), "/sigwait.skel.rs"))`
/// in `src/core/mod.rs`.
#[cfg(feature = "bpf")]
fn compile_bpf() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;

    let bpf_dir = "./bpf";
    let schedstat_src = format!("{bpf_dir}/schedstat.bpf.c");
    let sigwait_src = format!("{bpf_dir}/sigwait.bpf.c");
    let trace_hdr = format!("{bpf_dir}/trace_bpf.h");

    // Re-run if any BPF source files change
    println!("cargo:rerun-if-changed={schedstat_src}");
    println!("cargo:rerun-if-changed={sigwait_src}");
    println!("cargo:rerun-if-changed={trace_hdr}");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);

    // On Debian/Ubuntu the arch-qualified include dir holds asm/types.h.
    // `/usr/include/asm` is not a symlink on these systems, so clang targeting
    // BPF cannot find it without an explicit -I flag.  This mirrors the
    // `-I/usr/include/${BPF_ARCH}-linux-gnu` line in timpani-n/bpf.cmake.
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let arch_include = format!("/usr/include/{arch}-linux-gnu");

    // Map Rust arch names to Linux arch names for vmlinux.h
    let bpf_arch = match arch.as_str() {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => "x86_64", // default fallback
    };
    let vmlinux_include = format!("{bpf_dir}/{bpf_arch}");

    // Build clang args (bpf dir + architecture include + vmlinux.h path)
    let mut clang_args = vec![
        format!("-I{bpf_dir}"),         // For trace_bpf.h
        format!("-I{vmlinux_include}"), // For vmlinux.h
    ];
    if std::path::Path::new(&arch_include).exists() {
        clang_args.push(format!("-I{arch_include}"));
    }

    // Compile schedstat.bpf.c
    let mut binding = libbpf_cargo::SkeletonBuilder::new();
    let mut builder = binding.source(&schedstat_src);
    builder = builder.clang_args(&clang_args);
    builder.build_and_generate(out_dir.join("schedstat.skel.rs"))?;

    // Compile sigwait.bpf.c
    let mut binding = libbpf_cargo::SkeletonBuilder::new();
    let mut builder = binding.source(&sigwait_src);
    builder = builder.clang_args(&clang_args);
    builder.build_and_generate(out_dir.join("sigwait.skel.rs"))?;

    Ok(())
}

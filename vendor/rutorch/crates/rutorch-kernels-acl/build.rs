//! 只在 AArch64 Linux 且显式提供 ACL 目录时构建窄 C++ FFI。

use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rustc-check-cfg=cfg(rutorch_acl_available)");
    println!("cargo:rerun-if-env-changed=RUTORCH_ACL_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=RUTORCH_ACL_LIB_DIR");
    println!("cargo:rerun-if-changed=native/acl_shim.cpp");

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo 必须提供 target arch");
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("Cargo 必须提供 target OS");
    if target_arch != "aarch64" || target_os != "linux" {
        return;
    }
    let (Ok(source_dir), Ok(lib_dir)) = (
        env::var("RUTORCH_ACL_SOURCE_DIR"),
        env::var("RUTORCH_ACL_LIB_DIR"),
    ) else {
        return;
    };
    let source_dir = PathBuf::from(source_dir);
    let lib_dir = PathBuf::from(lib_dir);
    assert!(source_dir.join("arm_compute/runtime/Tensor.h").is_file());
    assert!(lib_dir.join("libarm_compute_armv8-a.so").is_file());

    cc::Build::new()
        .cpp(true)
        .file("native/acl_shim.cpp")
        .include(&source_dir)
        .include(source_dir.join("include"))
        .flag_if_supported("-std=c++14")
        .flag("-fopenmp")
        .define("AT_HWCAP2", "26")
        .compile("rutorch_acl_shim");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=arm_compute_armv8-a");
    println!("cargo:rustc-link-lib=dylib=stdc++");
    println!("cargo:rustc-link-lib=dylib=gomp");
    println!("cargo:rustc-cfg=rutorch_acl_available");
}

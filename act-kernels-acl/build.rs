use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=native/acl_shim.cpp");
    println!("cargo:rerun-if-env-changed=ACL_SOURCE_DIR");
    if env::var_os("CARGO_FEATURE_ACL_FREESTANDING").is_none() {
        return;
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let acl = env::var_os("ACL_SOURCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            manifest
                .parent()
                .and_then(|path| path.parent())
                .expect("exokernel parent")
                .join("rutorch/qemu/build/compute-library-v52.7.0")
        });
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("out dir"));
    let compiler = env::var_os("ACL_CXX").unwrap_or_else(|| "aarch64-linux-gnu-g++".into());
    let ar = env::var_os("ACL_AR").unwrap_or_else(|| "aarch64-linux-gnu-ar".into());
    let kernel = acl.join("src/core/NEON/kernels/arm_gemm/kernels/a64_sgemm_8x12/generic.cpp");
    let kernel_obj = out.join("acl_sgemm_8x12.o");
    let shim_obj = out.join("acl_shim.o");

    let common = [
        "-O3",
        "-mcpu=cortex-a76",
        "-std=c++14",
        "-fno-exceptions",
        "-fno-rtti",
        "-fno-threadsafe-statics",
        "-fno-unwind-tables",
        "-fno-asynchronous-unwind-tables",
        "-ffunction-sections",
        "-fdata-sections",
        "-ffreestanding",
        "-fno-builtin",
        "-DARM_COMPUTE_EXCEPTIONS_DISABLED",
        "-DENABLE_NEON",
        "-DARM_COMPUTE_ENABLE_NEON",
        "-DNO_MULTI_THREADING",
        "-DBARE_METAL",
    ];
    let include_core = acl.join("src/core");
    let include_gemm = acl.join("src/core/NEON/kernels/arm_gemm");
    let compile = |source: &PathBuf, object: &PathBuf| {
        let status = Command::new(&compiler)
            .args(common)
            .arg("-I")
            .arg(&include_core)
            .arg("-I")
            .arg(&include_gemm)
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(object)
            .status()
            .expect("invoke AArch64 C++ compiler");
        assert!(status.success(), "ACL microkernel compilation failed");
    };
    compile(&kernel, &kernel_obj);
    compile(&manifest.join("native/acl_shim.cpp"), &shim_obj);

    let archive = out.join("libact_acl_microkernel.a");
    let status = Command::new(&ar)
        .arg("crus")
        .arg(&archive)
        .arg(&shim_obj)
        .arg(&kernel_obj)
        .status()
        .expect("invoke AArch64 ar");
    assert!(status.success(), "ACL microkernel archive failed");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=act_acl_microkernel");
}

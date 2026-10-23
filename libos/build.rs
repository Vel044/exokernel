//! 仅 LiteEth 场景编译原始 Linux C 驱动；其余场景不引入 C 工具链依赖。
use std::{env, fs, path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=vendor/liteeth-linux/litex_liteeth.c");
    println!("cargo:rerun-if-changed=src/drivers/linux_liteeth");
    println!("cargo:rerun-if-env-changed=LITEETH_CC");
    if env::var_os("CARGO_FEATURE_APP_LITEETH_SMOKE").is_none() {
        return;
    }
    assert_eq!(env::var("TARGET").unwrap(), "aarch64-unknown-none");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::create_dir_all(out.join("include/linux")).unwrap();
    // 六个 Linux 头文件在此窄兼容边界汇入第一方声明，不递归引入宿主 Linux。
    for header in [
        "etherdevice.h",
        "interrupt.h",
        "litex.h",
        "module.h",
        "of_net.h",
        "platform_device.h",
    ] {
        fs::write(
            out.join("include/linux").join(header),
            "#include \"compat.h\"\n",
        )
        .unwrap();
    }
    let cc = env::var("LITEETH_CC").unwrap_or_else(|_| "clang".into());
    let object = out.join("liteeth.o");
    let status = Command::new(cc)
        .args([
            "--target=aarch64-none-elf",
            "-std=gnu11",
            "-O2",
            "-g",
            "-ffreestanding",
            "-fno-stack-protector",
            "-fno-builtin",
            "-Wall",
            "-Wextra",
            "-Wno-unused-parameter",
            "-I",
            "src/drivers/linux_liteeth",
            "-I",
        ])
        .arg(out.join("include"))
        .args(["-c", "src/drivers/linux_liteeth/adapter.c", "-o"])
        .arg(&object)
        .status()
        .unwrap();
    assert!(status.success(), "Linux LiteEth C 编译失败");
    // 直接链接 ELF 对象，避免 macOS 系统 ar 将跨架构 ELF 静默过滤。
    println!("cargo:rustc-link-arg={}", object.display());
}

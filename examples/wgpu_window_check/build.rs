fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" {
        // Metal 原生窗口示例仅支持 macOS；其他平台（Windows/Linux CI）跳过
        // 原生构建，避免用 MSVC cl 编译 .m 失败（与 danmaku_perf_lab 一致）。
        return;
    }

    cc::Build::new()
        .file("native/WgpuWindowCheck.m")
        .flag("-fobjc-arc")
        .compile("erika_wgpu_window_check");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-lib=framework=QuartzCore");
    println!("cargo:rustc-link-lib=framework=Metal");
}

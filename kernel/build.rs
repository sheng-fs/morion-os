//! 构建脚本 — 通过绝对路径指定链接脚本, 避免与 kernel_test 的 -T 冲突

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    // 内核链接脚本仅对 x86_64-unknown-none 目标生效。host 单测二进制若套用该脚本,
    // 会以错误的内核内存布局链接并启动即 SIGSEGV, 故按 target_os 门控。
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        println!("cargo:rustc-link-arg=-T{}/linker.ld", std::env::var("CARGO_MANIFEST_DIR").unwrap());
    }
}
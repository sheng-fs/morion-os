//! 构建脚本 — 通过绝对路径指定链接脚本, 避免与 kernel_test 的 -T 冲突
//!
//! 仅在内核目标 (x86_64-unknown-none) 下传链接脚本: 内核 `linker.ld` 定义了
//! 自定义入口/段布局, 链到 host 测试二进制上会导致启动即 SIGSEGV。

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "x86_64-unknown-none" {
        println!(
            "cargo:rustc-link-arg=-T{}/linker.ld",
            std::env::var("CARGO_MANIFEST_DIR").unwrap()
        );
    }
}
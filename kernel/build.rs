//! 构建脚本 — 通过绝对路径指定链接脚本, 避免与 kernel_test 的 -T 冲突

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", std::env::var("CARGO_MANIFEST_DIR").unwrap());
}
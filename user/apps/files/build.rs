//! 构建脚本 — 复用 `user/linker.ld`（所有用户程序共用同一套链接地址布局）。

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let linker = std::path::Path::new(&manifest).join("../../linker.ld");
    println!("cargo:rerun-if-changed={}", linker.display());
    println!("cargo:rustc-link-arg=-T{}", linker.display());
    // 自定义 target (code-model=large) 不继承工作区 config 的 -nostdlib。
    println!("cargo:rustc-link-arg=-nostdlib");
}

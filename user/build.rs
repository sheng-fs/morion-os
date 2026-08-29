//! 构建脚本 — 通过绝对路径指定用户程序链接脚本。

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    println!(
        "cargo:rustc-link-arg=-T{}/linker.ld",
        std::env::var("CARGO_MANIFEST_DIR").unwrap()
    );
    // 自定义 target (code-model=large) 不继承工作区 config 的 -nostdlib。
    println!("cargo:rustc-link-arg=-nostdlib");
}

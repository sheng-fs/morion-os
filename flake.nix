{
  description = "Morion OS — 声明式、不可变的微内核操作系统";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Rust 工具链 (由 rust-toolchain.toml 驱动)
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # 构建依赖
        buildInputs = with pkgs; [
          rustToolchain
          nasm                    # 汇编编译
          qemu                    # 模拟器 (带 KVM 加速)
          OVMF.fd                 # UEFI 固件镜像
          xorriso                 # ISO 镜像生成
          mtools                  # FAT 镜像操作
          gdb                     # 调试
        ];

        # 构建 Morion OS
        morion-os = pkgs.stdenv.mkDerivation {
          pname = "morion-os";
          version = "0.1.0";

          src = ./.;

          nativeBuildInputs = with pkgs; [
            rustToolchain
            nasm
            xorriso
            mtools
          ];

          buildPhase = ''
            # 构建引导器和内核
            make boot
            make kernel
            make iso
          '';

          installPhase = ''
            mkdir -p $out
            cp -r build/* $out/
          '';
        };
      in
      {
        packages = {
          default = morion-os;
          morion-os = morion-os;
        };

        devShells.default = pkgs.mkShell {
          inherit buildInputs;
          shellHook = ''
            echo "Morion OS 开发环境"
            echo "  Rust:  $(rustc --version)"
            echo "  QEMU:  $(qemu-system-x86_64 --version | head -1)"
            echo ""
            echo "常用命令:"
            echo "  make         构建 ISO 镜像"
            echo "  make run     QEMU 中运行"
            echo "  make debug   GDB 调试"
            echo "  make kernel  仅构建内核"
            echo "  make boot    仅构建引导器"
          '';
        };
      }
    );
}

# Morion OS — 构建系统
#
# 目标:
#   - make              : 构建完整的 OS 镜像 (boot.efi + kernel.elf → morion.iso)
#   - make kernel       : 仅构建微内核
#   - make boot         : 仅构建 UEFI 引导器
#   - make iso          : 生成可启动 ISO 镜像
#   - make run          : 在 QEMU 中运行
#   - make clean        : 清理构建产物
#   - make docs         : 生成文档
#
# 依赖:
#   - Rust nightly (x86_64-unknown-none + x86_64-unknown-uefi)
#   - QEMU (用于测试)
#   - xorriso / mtools  (用于创建 ISO)
#   - OVMF (UEFI 固件镜像)

# ============================================================
# 工具链配置
# ============================================================
CARGO         := cargo
RUSTUP        := rustup
QEMU          := qemu-system-x86_64
NASM          := nasm
MKDIR         := mkdir -p
CP            := cp
RM            := rm -rf

# Nightly features
export RUSTC_BOOTSTRAP := 1

# 目标三元组
KERNEL_TARGET  := x86_64-unknown-none
BOOT_TARGET    := x86_64-unknown-uefi

# ============================================================
# 输出路径
# ============================================================
OUT_DIR       := build
ISO_DIR       := $(OUT_DIR)/iso
KERNEL_ELF    := $(OUT_DIR)/kernel/morion-kernel-test
# 嵌入引导器的内核 ELF 路径 (boot/src/main.rs 用 include_bytes! 读取)
KERNEL_EMBED  := boot/loader/morion-kernel.elf
BOOT_EFI      := $(OUT_DIR)/boot/morion-boot.efi
EFIBOOT_IMG   := $(OUT_DIR)/efiboot.img
ISO_IMAGE     := $(OUT_DIR)/morion-os.iso

# QEMU 配置
QEMU_MEM      ?= 2G
QEMU_SMP      ?= 4
QEMU_ACCEL    ?= kvm
OVMF_CODE     ?= /usr/share/edk2/x64/OVMF_CODE.fd
OVMF_VARS     ?= /usr/share/edk2/x64/OVMF_VARS.fd

# ============================================================
# 默认目标
# ============================================================
.PHONY: all
all: iso

# ISO 无依赖输入, 且必须从最新 kernel/boot 重新生成, 故标记为伪目标强制重建
.PHONY: $(ISO_IMAGE)

# 源文件集合: 用于 make 层面的重建触发 (cargo 内部另有指纹追踪)。
KERNEL_SRC := $(shell find kernel_test -type f 2>/dev/null)
BOOT_SRC   := $(shell find boot/src boot/loader -type f 2>/dev/null) boot/build.rs boot/Cargo.toml

# ============================================================
# 微内核构建
# ============================================================
.PHONY: kernel
kernel: $(KERNEL_ELF)

$(KERNEL_ELF): $(KERNEL_SRC)
	@echo "==> 构建测试内核..."
	$(MKDIR) $(dir $@)
	$(CARGO) build \
		--target $(KERNEL_TARGET) \
		--package morion-kernel-test \
		--release \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem
	$(CP) target/$(KERNEL_TARGET)/release/morion-kernel-test $@
	@echo "  ✓ 测试内核构建完成: $@"

# 将内核 ELF 拷贝到引导器资源目录, 供 boot/src/main.rs include_bytes! 嵌入
$(KERNEL_EMBED): $(KERNEL_ELF)
	@echo "==> 拷贝测试内核到引导器资源目录..."
	$(MKDIR) $(dir $@)
	$(CP) $(KERNEL_ELF) $@
	@echo "  ✓ 已更新: $@"

# ============================================================
# UEFI 引导器构建
# ============================================================
.PHONY: boot
boot: $(BOOT_EFI)

$(BOOT_EFI): $(BOOT_SRC) $(KERNEL_EMBED)
	@echo "==> 构建 Morion 引导器..."
	$(MKDIR) $(dir $@)
	$(CARGO) build \
		--target $(BOOT_TARGET) \
		--package morion-boot \
		--release
	$(CP) target/$(BOOT_TARGET)/release/morion-boot.efi $@
	@echo "  ✓ 引导器构建完成: $@"

# ============================================================
# ISO 镜像构建
# ============================================================
.PHONY: iso
iso: kernel boot $(ISO_IMAGE)

$(ISO_IMAGE):
	@echo "==> 创建可启动 ISO 镜像..."
	$(MKDIR) $(ISO_DIR)
	$(MKDIR) $(ISO_DIR)/EFI/BOOT
	$(MKDIR) $(ISO_DIR)/EFI/morion/loader/entries
	$(MKDIR) $(ISO_DIR)/EFI/morion/loader/resources

	# 复制 EFI 引导器 (UEFI 默认路径)
	# 可注册方式: EFI/morion/morion-boot.efi + efibootmgr
	$(CP) $(BOOT_EFI) $(ISO_DIR)/EFI/BOOT/BOOTX64.EFI
	$(CP) $(BOOT_EFI) $(ISO_DIR)/EFI/morion/morion-boot.efi

	# 复制内核
	$(CP) $(KERNEL_ELF) $(ISO_DIR)/EFI/morion/morion-kernel.elf

	# 复制引导配置
	$(CP) boot/loader/loader.conf $(ISO_DIR)/EFI/morion/loader/
	$(CP) boot/loader/theme.toml $(ISO_DIR)/EFI/morion/loader/
	$(CP) boot/loader/entries/morion.conf $(ISO_DIR)/EFI/morion/loader/entries/

	# 复制引导器资源 (主题图片)
	$(CP) -r boot/loader/resources/* $(ISO_DIR)/EFI/morion/loader/resources/

	# 创建 initrd 占位 (后续用 Nix 生成实际 initramfs)
	@echo "{}" > $(ISO_DIR)/EFI/morion/initrd.img

	# 生成 FAT32 ESP 引导镜像。
	# OVMF 要求 El Torito 的 EFI 启动映像是一个 FAT 文件系统，
	# 而非直接指向 BOOTX64.EFI；故先用 mtools 制作 fat 镜像。
	@echo "==> 生成 FAT32 ESP 引导镜像..."
	dd if=/dev/zero of=$(EFIBOOT_IMG) bs=1M count=64 status=none
	mformat -i $(EFIBOOT_IMG) -F ::
	mmd -i $(EFIBOOT_IMG) ::/EFI
	mmd -i $(EFIBOOT_IMG) ::/EFI/BOOT
	mcopy -i $(EFIBOOT_IMG) $(BOOT_EFI) ::/EFI/BOOT/BOOTX64.EFI
	$(CP) $(EFIBOOT_IMG) $(ISO_DIR)/efiboot.img

	# 生成 ISO (EFI El Torito 启动, 指向 ESP 镜像)
	xorriso -as mkisofs \
		-iso-level 3 \
		-full-iso9660-filenames \
		-volid "MORION_OS" \
		-eltorito-alt-boot \
		-e efiboot.img \
		-no-emul-boot \
		-o $(ISO_IMAGE) \
		$(ISO_DIR)

	@echo "  ✓ ISO 镜像: $(ISO_IMAGE)"
	@ls -lh $(ISO_IMAGE) 2>/dev/null || echo "  ! ISO 生成失败, 请安装 xorriso 与 mtools"

# ============================================================
# 运行 (QEMU)
# ============================================================
.PHONY: run
run: iso
	@echo "==> 启动 QEMU..."
	$(QEMU) \
		-machine pc \
		-m $(QEMU_MEM) \
		-bios /usr/share/edk2/x64/OVMF.4m.fd \
		-cdrom $(ISO_IMAGE) \
		-vga virtio \
		-no-reboot \
		-d guest_errors

# QEMU 无 KVM 回退 (CI/无虚拟化环境)
.PHONY: run-nokvm
run-nokvm: iso
	$(QEMU) \
		-machine pc \
		-m $(QEMU_MEM) \
		-bios /usr/share/edk2/x64/OVMF.4m.fd \
		-cdrom $(ISO_IMAGE) \
		-vga virtio \
		-no-reboot

# GDB 调试
.PHONY: debug
debug: iso
	$(QEMU) \
		-machine q35,accel=$(QEMU_ACCEL) \
		-m $(QEMU_MEM) \
		-smp 1 \
		-bios $(OVMF_CODE) \
		-cdrom $(ISO_IMAGE) \
		-serial stdio \
		-vga virtio \
		-s -S \
		-no-reboot &
	@sleep 1
	@echo "==> 连接 GDB:"
	@echo "    gdb -ex 'target remote localhost:1234' \\"
	@echo "        -ex 'symbol-file $(KERNEL_ELF)'"
	@echo "    或使用 rust-gdb"

# ============================================================
# 工具链检查与安装
# ============================================================
.PHONY: setup
setup:
	@echo "==> 检查 Rust 工具链..."
	$(RUSTUP) toolchain install nightly
	$(RUSTUP) component add rust-src --toolchain nightly
	$(RUSTUP) target add $(KERNEL_TARGET) --toolchain nightly
	$(RUSTUP) target add $(BOOT_TARGET) --toolchain nightly
	@echo "  ✓ Rust 工具链已就绪"
	@echo "==> 检查构建依赖..."
	@command -v $(NASM) >/dev/null 2>&1 || echo "  ! 请安装 nasm: sudo pacman -S nasm"
	@command -v $(QEMU) >/dev/null 2>&1 || echo "  ! 请安装 qemu: sudo pacman -S qemu-desktop"
	@command -v xorriso >/dev/null 2>&1 || echo "  ! 请安装 xorriso (可选, 用于ISO生成)"
	@test -f $(OVMF_CODE) || echo "  ! 请安装 edk2-ovmf: sudo pacman -S edk2-ovmf"

# ============================================================
# 文档生成
# ============================================================
.PHONY: docs
docs:
	$(CARGO) doc --no-deps --workspace --open 2>/dev/null || \
	$(CARGO) doc --no-deps --workspace

# ============================================================
# 清理
# ============================================================
.PHONY: clean
clean:
	@echo "==> 清理构建产物..."
	$(CARGO) clean
	$(RM) $(OUT_DIR)
	@echo "  ✓ 清理完成"

# ============================================================
# 代码检查
# ============================================================
.PHONY: check
check:
	$(CARGO) check --workspace

.PHONY: fmt
fmt:
	$(CARGO) fmt --all -- --check

.PHONY: clippy
clippy:
	$(CARGO) clippy --workspace -- -D warnings

# ============================================================
# Nix 构建集成
# ============================================================
.PHONY: nix-build
nix-build:
	@echo "==> Nix 构建..."
	nix build .#morion-os

.PHONY: nix-shell
nix-shell:
	nix develop

# ============================================================
# 帮助
# ============================================================
.PHONY: help
help:
	@echo "Morion OS 构建系统"
	@echo ""
	@echo "用法: make [target]"
	@echo ""
	@echo "常用目标:"
	@echo "  all         默认目标, 等同于 iso"
	@echo "  kernel      构建微内核"
	@echo "  boot        构建 UEFI 引导器"
	@echo "  iso         生成可启动 ISO 镜像"
	@echo "  run         QEMU 中运行 (需要 KVM)"
	@echo "  run-nokvm   QEMU 中运行 (无硬件虚拟化)"
	@echo "  debug       QEMU + GDB 调试模式"
	@echo "  setup       安装所需工具链和依赖"
	@echo "  clean       清理构建产物"
	@echo "  check       检查代码编译"
	@echo "  fmt         检查代码格式"
	@echo "  clippy      Clippy 代码检查"
	@echo "  docs        生成文档"
	@echo "  help        显示此帮助"
	@echo ""
	@echo "Nix 构建:"
	@echo "  nix-build   nix build .#morion-os"
	@echo "  nix-shell   nix develop"
	@echo ""
	@echo "自定义变量:"
	@echo "  QEMU_MEM=4G        分配内存大小"
	@echo "  QEMU_SMP=8         CPU 核心数"
	@echo "  QEMU_ACCEL=kvm     加速方式 (kvm/hvf/whpx)"

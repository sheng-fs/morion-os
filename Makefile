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
KERNEL_ELF    := $(OUT_DIR)/kernel/morion-kernel
# 嵌入引导器的内核 ELF 路径 (boot/src/main.rs 用 include_bytes! 读取)
KERNEL_EMBED  := boot/loader/morion-kernel.elf
BOOT_EFI      := $(OUT_DIR)/boot/morion-boot.efi
# 用户态测试程序 (kernel/src/main.rs 用 include_bytes! 嵌入)
USER_ELF      := $(OUT_DIR)/user/morion-user
USER_BIN      := $(OUT_DIR)/user/user.bin
EFIBOOT_IMG   := $(OUT_DIR)/efiboot.img
ISO_IMAGE     := $(OUT_DIR)/morion-os.iso

# QEMU 配置
QEMU_MEM      ?= 2G
QEMU_SMP      ?= 4
QEMU_ACCEL    ?= kvm
OVMF_CODE     ?= /usr/share/edk2/x64/OVMF_CODE.fd
OVMF_VARS     ?= /usr/share/edk2/x64/OVMF_VARS.fd
# 文件系统阶段: NVMe 磁盘镜像 (宿主机 mkfs.fat 生成)
NVME_IMG      ?= $(OUT_DIR)/nvme.img
# MorionFS (MFS) 磁盘镜像: 纯空白 raw, 由 mfs_srv 首次挂载时自动格式化 (namespace 2)
MFS_IMG       ?= $(OUT_DIR)/mfs.img
MFS_MIB       ?= 16
# ext2 磁盘镜像: 由宿主 mke2fs 预格式化 + debugfs 预置测试文件 (namespace 3, 只读)
EXT2_IMG      ?= $(OUT_DIR)/ext2.img
EXT2_MIB      ?= 16
# exFAT 磁盘镜像: 由宿主 mkfs.exfat 预格式化 (namespace 5, 读写)
EXFAT_IMG     ?= $(OUT_DIR)/exfat.img
EXFAT_MIB     ?= 16
# 分区测试盘: MBR 两个主分区 (FAT32 + ext2), 用于验证 block_srv 卷层的分区解析 (namespace 4)
PARTS_IMG     ?= $(OUT_DIR)/parts.img
# 文件系统阶段: IDE 磁盘镜像 (Legacy PIO 读扇区验证)
DISK_IMG      ?= $(OUT_DIR)/disk.img

# ============================================================
# 默认目标
# ============================================================
.PHONY: all
all: iso

# ISO 无依赖输入, 且必须从最新 kernel/boot 重新生成, 故标记为伪目标强制重建
.PHONY: $(ISO_IMAGE)

# 源文件集合: 用于 make 层面的重建触发 (cargo 内部另有指纹追踪)。
KERNEL_SRC := $(shell find kernel -type f 2>/dev/null)
BOOT_SRC   := $(shell find boot/src boot/loader -type f 2>/dev/null) boot/build.rs boot/Cargo.toml
USER_SRC   := $(shell find user -type f 2>/dev/null)

# ============================================================
# 微内核构建
# ============================================================
.PHONY: kernel
kernel: $(KERNEL_ELF)

$(KERNEL_ELF): $(KERNEL_SRC) $(USER_BIN)
	@echo "==> 构建微内核..."
	$(MKDIR) $(dir $@)
	$(CARGO) build \
		--target $(KERNEL_TARGET) \
		--package morion-kernel \
		--release \
		-Z build-std=core,alloc,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem
	$(CP) target/$(KERNEL_TARGET)/release/morion-kernel $@
	@echo "  ✓ 微内核构建完成: $@"

# 将内核 ELF 拷贝到引导器资源目录, 供 boot/src/main.rs include_bytes! 嵌入
$(KERNEL_EMBED): $(KERNEL_ELF)
	@echo "==> 拷贝内核到引导器资源目录..."
	$(MKDIR) $(dir $@)
	$(CP) $(KERNEL_ELF) $@
	@echo "  ✓ 已更新: $@"

# ============================================================
# 用户态测试程序构建 (内核运行时加载)
# ============================================================
.PHONY: user
user: $(USER_BIN)

$(USER_ELF): $(USER_SRC)
	@echo "==> 构建用户态测试程序..."
	$(MKDIR) $(dir $@)
	$(CARGO) build \
		--target user/x86_64-morion-user.json \
		--package morion-user \
		--release \
		-Z json-target-spec \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem
	$(CP) target/x86_64-morion-user/release/morion-user $@
	@echo "  ✓ 用户程序构建完成: $@"

$(USER_BIN): $(USER_ELF)
	@echo "==> 生成用户程序扁平二进制..."
	$(MKDIR) $(dir $@)
	# 默认 objcopy -O binary 不含 NOBITS 的 .bss 段, 导致镜像长度只覆盖 text+data,
	# 而 .bss 可能落入下一未映射页 (随程序增长反复触发缺页, 破坏控制台/滚动)。
	# 用 --set-section-flags 把 .bss 标记为有内容, 使其零填充并入扁平二进制,
	# 令内核按「完整内存占用」映射足够页表。
	objcopy -O binary --set-section-flags .bss=alloc,load,contents,data $(USER_ELF) $(USER_BIN)
	@echo "  ✓ 用户程序二进制: $@"

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

# 文件系统阶段: 挂载 NVMe 磁盘运行 (q35 + 单控制器四 namespace)
#   nsid=1 -> $(NVME_IMG)  (FAT32, 挂载 /)
#   nsid=2 -> $(MFS_IMG)   (MorionFS, 挂载 /mfs, 首次挂载自动格式化)
#   nsid=3 -> $(EXT2_IMG)  (ext2 只读, 挂载 /ext2, 宿主预格式化)
#   nsid=4 -> $(PARTS_IMG) (MBR 分区测试盘: FAT32 + ext2, 验证卷层分区解析)
#   nsid=5 -> $(EXFAT_IMG) (exFAT 读写, 挂载 /usb, 宿主 mkfs.exfat 预格式化)
.PHONY: run-nvme
run-nvme: iso $(NVME_IMG) $(MFS_IMG) $(EXT2_IMG) $(PARTS_IMG) $(EXFAT_IMG)
	@echo "==> 启动 QEMU (q35 + NVMe, nsid1=FAT32, nsid2=MFS, nsid3=ext2, nsid4=分区盘, nsid5=exFAT)..."
	$(QEMU) \
		-machine q35 \
		-m $(QEMU_MEM) \
		-bios /usr/share/edk2/x64/OVMF.4m.fd \
		-cdrom $(ISO_IMAGE) \
		-device nvme,serial=MORION,id=nvme0 \
		-drive file=$(NVME_IMG),if=none,id=nvme0n1,format=raw \
		-device nvme-ns,drive=nvme0n1,bus=nvme0,nsid=1 \
		-drive file=$(MFS_IMG),if=none,id=nvme0n2,format=raw \
		-device nvme-ns,drive=nvme0n2,bus=nvme0,nsid=2 \
		-drive file=$(EXT2_IMG),if=none,id=nvme0n3,format=raw \
		-device nvme-ns,drive=nvme0n3,bus=nvme0,nsid=3 \
		-drive file=$(PARTS_IMG),if=none,id=nvme0n4,format=raw \
		-device nvme-ns,drive=nvme0n4,bus=nvme0,nsid=4 \
		-drive file=$(EXFAT_IMG),if=none,id=nvme0n5,format=raw \
		-device nvme-ns,drive=nvme0n5,bus=nvme0,nsid=5 \
		-vga virtio \
		-no-reboot \
		-d guest_errors

# MorionFS 磁盘镜像: 空白 raw, mfs_srv 首次挂载时写入超级块完成格式化
$(MFS_IMG):
	@echo "==> 创建 MFS 磁盘镜像 (空白 raw $(MFS_MIB)MiB, 首次挂载自动格式化)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(MFS_IMG) bs=1M count=$(MFS_MIB) status=none
	@echo "  ✓ MFS 镜像: $(MFS_IMG)"

# ext2 磁盘镜像: 宿主 mke2fs 预格式化 (只读兼容, 首挂载不自动格式化),
# 并用 debugfs 预置测试文件与子目录, 保证启动后无需写盘即可验证。
$(EXT2_IMG):
	@echo "==> 创建 ext2 磁盘镜像 (mke2fs + debugfs 预置测试文件)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(EXT2_IMG) bs=1M count=$(EXT2_MIB) status=none
	mke2fs -q -t ext2 -F -b 1024 $(EXT2_IMG)
	@printf 'Hello from ext2!\nThis is a read-only test file.\n' > $(OUT_DIR)/ext2hello.txt
	debugfs -w -R "write $(OUT_DIR)/ext2hello.txt HELLO.TXT" $(EXT2_IMG) >/dev/null 2>&1
	debugfs -w -R "mkdir /SUBDIR" $(EXT2_IMG) >/dev/null 2>&1
	@printf 'nested file in ext2 subdir!\n' > $(OUT_DIR)/ext2nested.txt
	debugfs -w -R "write $(OUT_DIR)/ext2nested.txt SUBDIR/NESTED.TXT" $(EXT2_IMG) >/dev/null 2>&1
	@echo "  ✓ ext2 镜像: $(EXT2_IMG)"

# 分区测试盘: 32 MiB, MBR 两个主分区 —— 分区 1 FAT32 (16 MiB, 起点 2048),
# 分区 2 ext2 (4 MiB, 起点 34816)。分区内容先在独立小镜像上格式化再 dd 进分区
# (宿主 mkfs 只认整盘/偏移, 先格式化再拼接最直观), 用于验证卷层:
#   - 能解析 MBR 分区表并登记分区为独立卷;
#   - 能按卷首签名探测出 FAT32 / ext2 类型。
# 注意: 这是**额外**的一卷测试盘, 不影响现有三张整盘镜像的卷号 (0/1/2)。
$(PARTS_IMG):
	@echo "==> 创建分区测试盘 (MBR: FAT32 + ext2)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(PARTS_IMG) bs=1M count=32 status=none
	printf '2048,32768,0x0c\n34816,8192,0x83\n' | sfdisk --quiet --no-tell-kernel $(PARTS_IMG)
	dd if=/dev/zero of=$(OUT_DIR)/part1.fat bs=512 count=32768 status=none
	mkfs.fat -F 32 $(OUT_DIR)/part1.fat >/dev/null 2>&1
	@printf 'partition 1 (FAT32) test file\n' > $(OUT_DIR)/part1.txt
	mcopy -i $(OUT_DIR)/part1.fat $(OUT_DIR)/part1.txt ::/PART1.TXT
	dd if=$(OUT_DIR)/part1.fat of=$(PARTS_IMG) bs=512 seek=2048 conv=notrunc status=none
	dd if=/dev/zero of=$(OUT_DIR)/part2.ext2 bs=1M count=4 status=none
	mke2fs -q -t ext2 -F -b 1024 $(OUT_DIR)/part2.ext2
	@printf 'partition 2 (ext2) test file\n' > $(OUT_DIR)/part2.txt
	debugfs -w -R "write $(OUT_DIR)/part2.txt PART2.TXT" $(OUT_DIR)/part2.ext2 >/dev/null 2>&1
	dd if=$(OUT_DIR)/part2.ext2 of=$(PARTS_IMG) bs=512 seek=34816 conv=notrunc status=none
	@echo "  ✓ 分区测试盘: $(PARTS_IMG)"

# exFAT 磁盘镜像: 宿主 mkfs.exfat 预格式化 (exfatprogs), 首挂载即可读;
# 服务**不**自动格式化 (与 ext2 同: 定位是读写既有的 exFAT 卷/U 盘)。
$(EXFAT_IMG):
	@echo "==> 创建 exFAT 磁盘镜像 (mkfs.exfat)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(EXFAT_IMG) bs=1M count=$(EXFAT_MIB) status=none
	mkfs.exfat -L MORIONUSB $(EXFAT_IMG) >/dev/null
	@echo "  ✓ exFAT 镜像: $(EXFAT_IMG)"

# 文件系统阶段: 挂载 IDE 磁盘运行 (Legacy PIO 读扇区, 不依赖 DMA/MSI-X)
.PHONY: run-ide
run-ide: iso $(DISK_IMG)
	@echo "==> 启动 QEMU (IDE PIO)..."
	$(QEMU) \
		-machine pc \
		-m $(QEMU_MEM) \
		-bios /usr/share/edk2/x64/OVMF.4m.fd \
		-cdrom $(ISO_IMAGE) \
		-drive file=$(DISK_IMG),if=ide,format=raw \
		-vga virtio \
		-no-reboot \
		-d guest_errors,int \
		-D $(OUT_DIR)/qemu.log

# 创建 NVMe 磁盘镜像并格式化为 FAT32, 写入与 IDE 镜像一致的测试文件
# 另含一个 VFAT 长名文件 (Long File Name.txt, 短名派生为 LONGFI~1.TXT),
# 供 VFAT 长名读取 / 按长名打开的自测与交互验证使用。
$(NVME_IMG):
	@echo "==> 创建 NVMe 磁盘镜像 (FAT32)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(NVME_IMG) bs=1M count=64 status=none
	mkfs.fat -F 32 $(NVME_IMG) >/dev/null 2>&1
	@printf 'Hello from FAT32!\nThis is a test file.\n' > $(OUT_DIR)/hello.txt
	mcopy -i $(NVME_IMG) $(OUT_DIR)/hello.txt ::/HELLO.TXT
	mmd -i $(NVME_IMG) ::/DIR1
	@printf 'nested file via path!\n' > $(OUT_DIR)/nested.txt
	mcopy -i $(NVME_IMG) $(OUT_DIR)/nested.txt ::/DIR1/NESTED.TXT
	@printf 'long name read via VFAT LFN!\n' > $(OUT_DIR)/longname.txt
	mcopy -i $(NVME_IMG) $(OUT_DIR)/longname.txt ::/"Long File Name.txt"
	@echo "  ✓ NVMe 镜像: $(NVME_IMG)"

# 创建 IDE 磁盘镜像并格式化为 FAT32
$(DISK_IMG):
	@echo "==> 创建 IDE 磁盘镜像 (FAT32)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(DISK_IMG) bs=1M count=1024 status=none
	mkfs.fat -F 32 $(DISK_IMG) >/dev/null 2>&1
	@printf 'Hello from FAT32!\nThis is a test file.\n' > $(OUT_DIR)/hello.txt
	mcopy -i $(DISK_IMG) $(OUT_DIR)/hello.txt ::/HELLO.TXT
	mmd -i $(DISK_IMG) ::/DIR1
	@printf 'nested file via path!\n' > $(OUT_DIR)/nested.txt
	mcopy -i $(DISK_IMG) $(OUT_DIR)/nested.txt ::/DIR1/NESTED.TXT
	@echo "  ✓ IDE 镜像: $(DISK_IMG)"

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
# 三个 crate 目标各不相同 (kernel: x86_64-unknown-none, user: 自定义 json target,
# boot: x86_64-unknown-uefi), 没有单一 target 能覆盖全工作区, 故逐个检查 ——
# 直接用 `cargo check --workspace` 会退化到宿主 target, 在 no_std bin 上报
# `#[panic_handler] required` 而失败。
.PHONY: check
check:
	$(CARGO) check --package morion-kernel --target $(KERNEL_TARGET)
	$(CARGO) check --package morion-user \
		--target user/x86_64-morion-user.json -Z json-target-spec \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem
	$(CARGO) check --package morion-boot --target $(BOOT_TARGET)

.PHONY: fmt
fmt:
	$(CARGO) fmt --all -- --check

# clippy 是**门禁**: `-D warnings`, 三个 crate 任一有告警即失败。
.PHONY: clippy
clippy:
	$(CARGO) clippy --package morion-kernel --target $(KERNEL_TARGET) -- -D warnings
	$(CARGO) clippy --package morion-user \
		--target user/x86_64-morion-user.json -Z json-target-spec \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem -- -D warnings
	$(CARGO) clippy --package morion-boot --target $(BOOT_TARGET) -- -D warnings

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

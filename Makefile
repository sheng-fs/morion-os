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
# 可选: 让 QEMU 暴露 Intel VT-d (IOMMU) —— 校验 E1b 的 DMA 重映射路径。用法: `make run-nvme IOMMU=1`
# 注意: 新版 QEMU (11.x) 已移除 `-machine ...,intel-iommu=on` 属性, 须用 `-device intel-iommu`。
IOMMU         ?=
IOMMU_ARG     := $(if $(IOMMU),-device intel-iommu,)
# 版本 / 变体注入 (V1 版本串 + V2 无图形收口 + 安装盘变体) —— 都以**编译期环境变量**交给 rustc:
#   MORION_BUILD —— git 短哈希 (无 git 时用日期), 供内核 `SYS_UNAME` 报告构建号;
#   NOGUI=1      —— 无图形变体: release 串带 `-nogui`, shell 不开屏幕镜像。
#   INSTALL=1    —— 安装盘变体: release 串带 `-install`, 且 `mkfs.mfs` **默认允许格式化非空白卷**
#                   (发行版装机 = 先 U 盘启动、再把系统装到本机盘上, 那时盘上原有文件系统正是
#                   要被覆盖的东西; 逐条 `--force` 只会把安装脚本写得很脆)。日常镜像不受影响。
#   SAFE=1       —— 安全模式变体 (Phase 0): release 串带 `-safe`, 且 `app` 自测**整体跳过** ——
#                   真机启动时内核会把本机盘当卷挂上, 自测的创建/删除文件、拍快照、分区写等于
#                   对着真实系统盘动手。首次真机启动用这个变体。
# 读它们的唯一来源: kernel/src/version.rs 与 user/libmorion/src/syscall.rs。
MORION_BUILD  ?= $(shell git rev-parse --short HEAD 2>/dev/null || date +%Y%m%d)
NOGUI         ?=
INSTALL       ?=
SAFE          ?=
export MORION_BUILD
ifneq ($(NOGUI),)
export MORION_NOGUI := 1
endif
ifneq ($(INSTALL),)
export MORION_INSTALL := 1
endif
ifneq ($(SAFE),)
export MORION_SAFE := 1
endif
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
# 变体用**独立输出子目录** (放在已忽略的 build/ 里), 免得与常规构建的镜像 /
# 指纹互相污染 —— 切换变体不需要 `make clean` (`NOGUI=1 make iso` → 产物落在 build/nogui/)。
# 同时置 `INSTALL=1 NOGUI=1` 时优先落 build/install (安装盘大概率还要图形, 该组合很少用)。
# `SAFE=1` (真机安全模式) 优先级最高, 单独落 `build/safe/`。
OUT_DIR       ?= $(if $(SAFE),build/safe,$(if $(INSTALL),build/install,$(if $(NOGUI),build/nogui,build)))
ISO_DIR       := $(OUT_DIR)/iso
KERNEL_ELF    := $(OUT_DIR)/kernel/morion-kernel
# 嵌入引导器的内核 ELF 路径 (boot/src/main.rs 用 include_bytes! 读取)
KERNEL_EMBED  := boot/loader/morion-kernel.elf
BOOT_EFI      := $(OUT_DIR)/boot/morion-boot.efi
# 用户态系统服务 (E2b): 每个服务都是**独立程序** (独立 crate bin → 独立 ELF)。
# E3b 起它们**不再嵌进内核**: 由 UEFI 引导器从 ESP 的 EFI/morion/services/ 读入内存,
# 经 BootInfo 模块表交给内核按固定域号加载 —— 故内核不依赖 $(SRV_ELFS), 只有 ISO 需要。
SRV_NAMES     := sender receiver pager echo kbd block_srv fat32_srv app shell mount_srv tmpfs_srv mfs_srv ext2_srv exfat_srv init gfx_srv net_srv virtio_blk_srv ahci_srv xhci_srv iso9660_srv netstack_srv e1000e_srv httpd_srv e1000_srv wifi_srv
SRV_DIR       := $(OUT_DIR)/user/srv
SRV_ELFS      := $(addprefix $(SRV_DIR)/,$(addsuffix .elf,$(SRV_NAMES)))
SRV_STAMP     := $(SRV_DIR)/.built
# 可执行文件加载 (E1) 的演示程序: 独立 crate → 独立 ELF, 由 `SYS_SPAWN_ELF` 运行时载入。
# 主程序用 include_bytes! 把它带进镜像当"运输方式", 自测再写进文件系统读回来跑 (见 roadmap E1)。
HELLO_ELF     := $(OUT_DIR)/user/hello.elf
EFIBOOT_IMG   := $(OUT_DIR)/efiboot.img
ISO_IMAGE     := $(OUT_DIR)/morion-os.iso
# 可引导 U 盘镜像 (Phase 0 / P0.4): 与 ISO 同源, 但额外叠了 GPT/ESP 混合布局。
USB_IMAGE     := $(OUT_DIR)/morion-usb.img

# QEMU 配置
QEMU_MEM      ?= 2G
QEMU_SMP      ?= 4
QEMU_ACCEL    ?= kvm
OVMF_CODE     ?= /usr/share/edk2/x64/OVMF_CODE.fd
OVMF_VARS     ?= /usr/share/edk2/x64/OVMF_VARS.fd
# 文件系统阶段: NVMe 磁盘镜像 (宿主机 mkfs.fat 生成)
NVME_IMG      ?= $(OUT_DIR)/nvme.img
# 可选的每簇扇区数 (1/2/4/.../128); 默认 8 (= 4 KiB 簇)。
# mkfs.fat 对 64 MiB 盘会给 512 B 簇 (每簇 1 扇区), 写大文件要跨数百簇、慢到
# 不可用且不现实; 4 KiB 簇是 fat32 的常见默认。大簇写路径用 `make NVME_CLU=64`
# (= 32 KiB 簇) 单独验证。
NVME_CLU      ?= 8
# MorionFS (MFS) 磁盘镜像: 纯空白 raw, 由 mfs_srv 首次挂载时自动格式化 (namespace 2)
# 大小刻意取 256 MiB (而非 16): mfs_srv 现在按**卷的真实容量**格式化, 镜像比默认值大
# 才能让「按几何定尺寸」这条路径真正被走到 (见 FS-21 自测); 且 MFS7 起位图外置后
# 超过 128 MiB 会走到**多块位图**路径 (bb ≥ 2, 见 FS-23(a))。
MFS_IMG       ?= $(OUT_DIR)/mfs.img
MFS_MIB       ?= 256
# ext2 磁盘镜像: 由宿主 mke2fs 预格式化 + debugfs 预置测试文件 (namespace 3, 只读)
EXT2_IMG      ?= $(OUT_DIR)/ext2.img
EXT2_MIB      ?= 16
# exFAT 磁盘镜像: 由宿主 mkfs.exfat 预格式化 (namespace 5, 读写)
EXFAT_IMG     ?= $(OUT_DIR)/exfat.img
EXFAT_MIB     ?= 16
# 可选的簇大小 (如 4K/32K/128K); 留空 = 让 mkfs.exfat 按卷大小选默认值。
# 用于验证大簇 (多页 DMA / 按需位图): make EXFAT_MIB=2048 EXFAT_CLU=32K ...
EXFAT_CLU     ?=
# 分区测试盘: MBR 两个主分区 (FAT32 + ext2), 用于验证 block_srv 卷层的分区解析 (namespace 4)
PARTS_IMG     ?= $(OUT_DIR)/parts.img
# 空白测试盘 (namespace 6): 纯零, **不含任何文件系统** —— 卷层探测为 unknown, 正是真盘上
# 「新买一块盘」的样子。`mkfs.mfs <卷号>` 拿它验证「格式化空白卷 -> 作为额外卷挂载」这条
# 路径 (见 FS-22); FS-21 之前的用例不碰它。
SPARE_IMG     ?= $(OUT_DIR)/spare.img
SPARE_MIB     ?= 16
# 分区表测试盘 (namespace 7): 纯零, **不预格式化**, 专供 block_srv 的**写**分区表路径 ——
# `part.*` 在它上面建/删 GPT 与 MBR 分区 (见 FS-26)。与 parts.img (只读解析) 分开,
# 免得把「解析既有分区表」的用例与「改写分区表」的用例搅在一块。
PT_IMG        ?= $(OUT_DIR)/pt.img
PT_MIB        ?= 64
# virtio-blk 测试盘 (驱动路线 D3): 空白 raw, **扇区 0 预写已知签名** —— 第二个真实驱动
# virtio_blk_srv (域 17) 起来后读扇区 0 校验签名、写扇区 1 再读回, 打 `VBLK1` marker。
# 盘内容必须确定 (签名是自测判据), 故每次重建而非增量。
VBLK_IMG      ?= $(OUT_DIR)/vblk.img
VBLK_MIB      ?= 1
# AHCI/SATA 测试盘 (驱动路线 D4): 空白 raw, **扇区 0 预写已知签名** —— 域 18 的 ahci_srv 起来后
# 用 IDENTIFY + LBA48 DMA 读扇区 0 校验签名, 打 `AHCI1 ... sig=ok` marker。全程只读; 盘内容
# 必须确定 (签名是自测判据), 故每次重建而非增量。
AHCI_IMG      ?= $(OUT_DIR)/ahci.img
AHCI_MIB      ?= 1
# USB 存储测试盘 (驱动路线 03c): 空白 raw, **扇区 0 预写已知签名** —— 域 19 的 xhci_srv 起来后
# 经 qemu-xhci + usb-storage 用 SCSI READ(10) 读扇区 0 校验签名, 打 `USB1 ... sig=ok` marker。
# 盘内容必须确定 (签名是自测判据), 故每次重建而非增量。
USB_IMG       ?= $(OUT_DIR)/usb.img
USB_MIB       ?= 1
# 文件系统阶段: IDE 磁盘镜像 (Legacy PIO 读扇区验证)
DISK_IMG      ?= $(OUT_DIR)/disk.img
# ISO9660 测试盘 (03c 续 / 安装介质): `morion-os.iso` 的**只读副本** —— 同一文件不能既作
# `-cdrom` 启动介质、又被 QEMU 当块设备打开。副本接成 nvme-ns nsid=8, 域 20 的 iso9660_srv
# 从中读出整盘 ISO9660 (根下有 EFIBOOT.IMG 作自测判据)。
ISO_IMG       ?= $(OUT_DIR)/iso.img

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

# 内核不再 include_bytes! 服务 ELF (E3b: 由引导器从 ESP 读入), 故不依赖 $(SRV_STAMP)。
$(KERNEL_ELF): $(KERNEL_SRC)
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
# 用户态系统服务构建 (E2b: 每个服务一份独立 ELF, 内核引导期加载)
# ============================================================
.PHONY: user
user: $(SRV_STAMP)

# 只构建演示程序 (独立 ELF): 便于单独改动/检查一个"运行时加载"的程序。
.PHONY: hello
hello: $(HELLO_ELF)

# 演示程序 (E1/E2 可执行文件加载): 独立 crate、独立 ELF, 依赖运行库 libmorion。
# 它不进引导期服务表, 而是经 `make` 放进 FAT32 卷 (见 $(NVME_IMG) 规则),
# 由 shell 的 `run /hello.mex` 或 app 的 FS-27 自测**从文件**加载。
$(HELLO_ELF): $(shell find user/hello -type f 2>/dev/null) $(shell find user/libmorion -type f 2>/dev/null) user/linker.ld
	@echo "==> 构建演示程序 (独立 ELF, 供 SYS_SPAWN_ELF 加载)..."
	$(MKDIR) $(dir $@)
	$(CARGO) build \
		--target user/x86_64-morion-user.json \
		--package morion-hello \
		--release \
		-Z json-target-spec \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem
	$(CP) target/x86_64-morion-user/release/morion-hello $@
	@echo "  ✓ 演示程序: $@"

# 14 个服务 ELF: `morion-srv` 一次构建 14 个 bin, 再逐个拷成 `<name>.elf`
# (内核 `SERVICE_ELFS` 按名 include_bytes!)。用 stamp 让"多产物一次构建"只跑一遍。
SRV_SRC := $(shell find user/srv user/libmorion user/libdevice -type f 2>/dev/null) user/linker.ld
$(SRV_STAMP): $(SRV_SRC)
	@echo "==> 构建用户态服务 (E2b: 14 个独立 ELF)..."
	$(MKDIR) $(SRV_DIR)
	$(CARGO) build \
		--target user/x86_64-morion-user.json \
		--package morion-srv \
		--release \
		-Z json-target-spec \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem
	@for n in $(SRV_NAMES); do \
		$(CP) target/x86_64-morion-user/release/$$n $(SRV_DIR)/$$n.elf; \
	done
	@touch $(SRV_STAMP)
	@echo "  ✓ 14 个服务 ELF: $(SRV_DIR)/*.elf"

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
iso: kernel boot $(SRV_STAMP) $(ISO_IMAGE)

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

	# 服务 ELF (E3b): 引导器在 exit_boot_services 之前从**自己所在的那个卷**读它们
	# (EFI/morion/services/<name>.elf), 所以必须放进 ESP —— 只放进 ISO 目录树是不够的。
	mmd -i $(EFIBOOT_IMG) ::/EFI/morion
	mmd -i $(EFIBOOT_IMG) ::/EFI/morion/services
	@for n in $(SRV_NAMES); do \
		mcopy -o -i $(EFIBOOT_IMG) $(SRV_DIR)/$$n.elf ::/EFI/morion/services/$$n.elf; \
	done

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
# 可引导 U 盘镜像 (Phase 0 / P0.4)
# ============================================================
# 标准 ISO 是 El Torito 结构: `-cdrom` 启动没问题, 但**真机把 U 盘当磁盘看** —— 固件只在
# 磁盘上找 ESP (FAT 分区), 所以直接 dd 一个 ISO 通常**起不来**。
#
# 这里不碰 ISO, 另做一个**标准 GPT + ESP 磁盘镜像**: 把 `iso` 规则已经建好的 ESP
# (`$(EFIBOOT_IMG)`, 64 MiB FAT32, 内含 BOOTX64.EFI + 内核 + 全部服务 ELF) **原样写进
# 一块 GPT 磁盘的 EFI 系统分区**。真机 UEFI 按磁盘方式找到它并启动。
# (不采用 `-isohybrid-gpt-basdat`: 它要求 El Torito 载入映像 ≤ 32 MiB, 而 64 MiB 的 ESP
#  是 FAT32 的最小可用尺寸, 缩小会被 mformat 拒绝。)
#
# 真机首启建议配安全模式: `make SAFE=1 usbimg` → build/safe/morion-usb.img
.PHONY: usbimg
usbimg: iso
	@echo "==> 生成可引导 U 盘镜像 (GPT + ESP)..."
	@esp_secs=$$(stat -c %s $(EFIBOOT_IMG) | awk '{print int($$1/512)}'); \
	rm -f $(USB_IMAGE); \
	dd if=/dev/zero of=$(USB_IMAGE) bs=1M count=80 status=none; \
	sgdisk -n 1:2048:+$$esp_secs -t 1:ef00 -c 1:MORIONOS $(USB_IMAGE) >/dev/null; \
	dd if=$(EFIBOOT_IMG) of=$(USB_IMAGE) bs=1M seek=1 conv=notrunc status=none; \
	sync
	@ls -lh $(USB_IMAGE)
	@sgdisk -p $(USB_IMAGE) 2>/dev/null | sed -n '4,20p' || true
	@echo ""
	@echo "  写入 U 盘 (⚠ 会清空目标盘, 先用 lsblk 确认设备名, 别写错盘!):"
	@echo "    sudo dd if=$(USB_IMAGE) of=/dev/sdX bs=4M status=progress conv=fsync"
	@echo "  真机启动前请进 BIOS **关闭 Secure Boot** (MorionOS 的引导器未签名)。"

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

# 文件系统阶段: 挂载 NVMe 磁盘运行 (q35 + 单控制器六 namespace)
#   nsid=1 -> $(NVME_IMG)  (FAT32, 挂载 /)
#   nsid=2 -> $(MFS_IMG)   (MorionFS, 挂载 /mfs, 首次挂载自动格式化)
#   nsid=3 -> $(EXT2_IMG)  (ext2 只读, 挂载 /ext2, 宿主预格式化)
#   nsid=4 -> $(PARTS_IMG) (MBR 分区测试盘: FAT32 + ext2, 验证卷层分区解析)
#   nsid=5 -> $(EXFAT_IMG) (exFAT 读写, 挂载 /usb, 宿主 mkfs.exfat 预格式化)
#   nsid=6 -> $(SPARE_IMG) (空白盘, 供 `mkfs.mfs` 自测: 格式化后作额外卷挂到 /usb<卷号>)
#   nsid=7 -> $(PT_IMG)    (空白盘, 供 `part.*` 自测: 建/删 GPT 与 MBR 分区)
#   另挂一台 virtio-blk ($(VBLK_IMG)) 给域 17 的 virtio_blk_srv (驱动路线 D3) 做块设备自测。
#   再挂一台 AHCI/SATA 盘 ($(AHCI_IMG), q35 自带的 ich9-ahci) 给域 18 的 ahci_srv (D4) 做只读自测。
#   另挂一台 xHCI 控制器 (qemu-xhci) + usb-storage 盘 ($(USB_IMG)) 给域 19 的 xhci_srv (03c)。
#   nsid=8 -> $(ISO_IMG)   (morion-os.iso 的只读副本, 整盘 ISO9660, 挂 /cdrom, 域 20 的 iso9660_srv)。
.PHONY: run-nvme
run-nvme: iso $(NVME_IMG) $(MFS_IMG) $(EXT2_IMG) $(PARTS_IMG) $(EXFAT_IMG) $(SPARE_IMG) $(PT_IMG) $(VBLK_IMG) $(AHCI_IMG) $(USB_IMG) $(ISO_IMG)
	@echo "==> 启动 QEMU (q35 + NVMe, nsid1=FAT32, nsid2=MFS, nsid3=ext2, nsid4=分区盘, nsid5=exFAT, nsid6=空白, nsid7=分区表测试, nsid8=ISO9660)..."
	$(QEMU) \
		-machine q35 \
		$(IOMMU_ARG) \
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
		-drive file=$(SPARE_IMG),if=none,id=nvme0n6,format=raw \
		-device nvme-ns,drive=nvme0n6,bus=nvme0,nsid=6 \
		-drive file=$(PT_IMG),if=none,id=nvme0n7,format=raw \
		-device nvme-ns,drive=nvme0n7,bus=nvme0,nsid=7 \
		-drive file=$(ISO_IMG),if=none,id=nvme0n8,format=raw,readonly=on \
		-device nvme-ns,drive=nvme0n8,bus=nvme0,nsid=8 \
		-netdev user,id=n0 \
		-device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56 \
		-netdev user,id=n1 \
		-device e1000e,netdev=n1,mac=52:54:00:aa:bb:cc \
		-netdev user,id=n2 \
		-device e1000,netdev=n2,mac=52:54:00:dd:ee:ff \
		-drive file=$(VBLK_IMG),if=none,id=vblk0,format=raw \
		-device virtio-blk-pci,drive=vblk0 \
		-drive file=$(AHCI_IMG),if=none,id=ahci0,format=raw \
		-device ide-hd,drive=ahci0 \
		-drive file=$(USB_IMG),if=none,id=usb0,format=raw \
		-device qemu-xhci,id=xhci \
		-device usb-storage,bus=xhci.0,drive=usb0 \
		-vga virtio \
		-no-reboot \
		-d guest_errors

# 分区表测试盘: 纯零 raw。**不预格式化** —— FS-26 自己在客户机里建/删分区表。
$(PT_IMG):
	@echo "==> 创建分区表测试盘 ($(PT_MIB)MiB, 无分区表, 供 part.* 自测)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(PT_IMG) bs=1M count=$(PT_MIB) status=none
	@echo "  ✓ 分区表测试盘: $(PT_IMG)"

# 空白测试盘: 纯零 raw。**不预格式化** —— 留给 `mkfs.mfs` 在客户机里格式化。
$(SPARE_IMG):
	@echo "==> 创建空白测试盘 ($(SPARE_MIB)MiB, 无文件系统, 供 mkfs.mfs 自测)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(SPARE_IMG) bs=1M count=$(SPARE_MIB) status=none
	@echo "  ✓ 空白盘: $(SPARE_IMG)"

# virtio-blk 测试盘: 空白 raw + 扇区 0 写入已知签名 (供域 17 的 D3 自测读回校验)。
$(VBLK_IMG):
	@echo "==> 创建 virtio-blk 测试盘 ($(VBLK_MIB)MiB, 扇区 0 = 签名, 供 D3 自测)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(VBLK_IMG) bs=1M count=$(VBLK_MIB) status=none
	printf 'MORION-VBLK-TST!' | dd of=$(VBLK_IMG) bs=512 count=1 conv=notrunc,sync status=none
	@echo "  ✓ virtio-blk 盘: $(VBLK_IMG)"

# AHCI/SATA 测试盘: 空白 raw + 扇区 0 写入已知签名 (供域 18 的 D4 自测读回校验)。
# 挂在 q35 自带的 ich9-ahci 上 (`-device ide-hd`), 驱动全程只读不发写命令。
$(AHCI_IMG):
	@echo "==> 创建 AHCI/SATA 测试盘 ($(AHCI_MIB)MiB, 扇区 0 = 签名, 供 D4 自测)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(AHCI_IMG) bs=1M count=$(AHCI_MIB) status=none
	printf 'MORION-AHCI-TST!' | dd of=$(AHCI_IMG) bs=512 count=1 conv=notrunc,sync status=none
	@echo "  ✓ AHCI 盘: $(AHCI_IMG)"

# USB 存储测试盘: 空白 raw + 扇区 0 写入已知签名 (供域 19 的 03c 自测读回校验)。
# 挂在 qemu-xhci 的 usb-storage 上 (`-device usb-storage,bus=xhci.0`)。
$(USB_IMG):
	@echo "==> 创建 USB 存储测试盘 ($(USB_MIB)MiB, 扇区 0 = 签名, 供 03c 自测)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(USB_IMG) bs=1M count=$(USB_MIB) status=none
	printf 'MORION-USB-TST!!' | dd of=$(USB_IMG) bs=512 count=1 conv=notrunc,sync status=none
	@echo "  ✓ USB 盘: $(USB_IMG)"

# ISO9660 测试副本 (03c 续): 见 ISO_IMG 注释。ISO_IMAGE 是伪目标 (每轮重建), 故这里恒拷贝。
$(ISO_IMG): $(ISO_IMAGE)
	@echo "==> 生成 ISO9660 测试副本 ($(ISO_IMG))..."
	cp -f $(ISO_IMAGE) $(ISO_IMG)
	@echo "  ✓ ISO 副本: $(ISO_IMG)"

# MorionFS 磁盘镜像: 空白 raw, mfs_srv 首次挂载时写入超级块完成格式化
$(MFS_IMG):
	@echo "==> 创建 MFS 磁盘镜像 (空白 raw $(MFS_MIB)MiB, 首次挂载自动格式化)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(MFS_IMG) bs=1M count=$(MFS_MIB) status=none
	@echo "  ✓ MFS 镜像: $(MFS_IMG)"

# ext2 磁盘镜像: 宿主 mke2fs 预格式化 (只读兼容, 首挂载不自动格式化),
# 并用 debugfs 预置测试文件与子目录, 保证启动后无需写盘即可验证。
$(EXT2_IMG): Makefile
	@echo "==> 创建 ext2 磁盘镜像 (mke2fs + debugfs 预置测试文件)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(EXT2_IMG) bs=1M count=$(EXT2_MIB) status=none
	mke2fs -q -t ext2 -F -b 1024 $(EXT2_IMG)
	@printf 'Hello from ext2!\nThis is a read-only test file.\n' > $(OUT_DIR)/ext2hello.txt
	debugfs -w -R "write $(OUT_DIR)/ext2hello.txt hello.txt" $(EXT2_IMG) >/dev/null 2>&1
	debugfs -w -R "mkdir /subdir" $(EXT2_IMG) >/dev/null 2>&1
	@printf 'nested file in ext2 subdir!\n' > $(OUT_DIR)/ext2nested.txt
	debugfs -w -R "write $(OUT_DIR)/ext2nested.txt subdir/nested.txt" $(EXT2_IMG) >/dev/null 2>&1
	@echo "  ✓ ext2 镜像: $(EXT2_IMG)"

# 分区测试盘: 32 MiB, MBR 两个主分区 —— 分区 1 FAT32 (16 MiB, 起点 2048),
# 分区 2 ext2 (4 MiB, 起点 34816)。分区内容先在独立小镜像上格式化再 dd 进分区
# (宿主 mkfs 只认整盘/偏移, 先格式化再拼接最直观), 用于验证卷层:
#   - 能解析 MBR 分区表并登记分区为独立卷;
#   - 能按卷首签名探测出 FAT32 / ext2 类型。
# 注意: 这是**额外**的一卷测试盘, 不影响现有三张整盘镜像的卷号 (0/1/2)。
$(PARTS_IMG): Makefile
	@echo "==> 创建分区测试盘 (MBR: FAT32 + ext2)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(PARTS_IMG) bs=1M count=32 status=none
	printf '2048,32768,0x0c\n34816,8192,0x83\n' | sfdisk --quiet --no-tell-kernel $(PARTS_IMG)
	dd if=/dev/zero of=$(OUT_DIR)/part1.fat bs=512 count=32768 status=none
	mkfs.fat -F 32 $(OUT_DIR)/part1.fat >/dev/null 2>&1
	@printf 'partition 1 (FAT32) test file\n' > $(OUT_DIR)/part1.txt
	mcopy -i $(OUT_DIR)/part1.fat $(OUT_DIR)/part1.txt ::/part1.txt
	dd if=$(OUT_DIR)/part1.fat of=$(PARTS_IMG) bs=512 seek=2048 conv=notrunc status=none
	dd if=/dev/zero of=$(OUT_DIR)/part2.ext2 bs=1M count=4 status=none
	mke2fs -q -t ext2 -F -b 1024 $(OUT_DIR)/part2.ext2
	@printf 'partition 2 (ext2) test file\n' > $(OUT_DIR)/part2.txt
	debugfs -w -R "write $(OUT_DIR)/part2.txt part2.txt" $(OUT_DIR)/part2.ext2 >/dev/null 2>&1
	dd if=$(OUT_DIR)/part2.ext2 of=$(PARTS_IMG) bs=512 seek=34816 conv=notrunc status=none
	@echo "  ✓ 分区测试盘: $(PARTS_IMG)"

# exFAT 磁盘镜像: 宿主 mkfs.exfat 预格式化 (exfatprogs), 首挂载即可读;
# 服务**不**自动格式化 (与 ext2 同: 定位是读写既有的 exFAT 卷/U 盘)。
$(EXFAT_IMG):
	@echo "==> 创建 exFAT 磁盘镜像 (mkfs.exfat, $(EXFAT_MIB)MiB$(if $(EXFAT_CLU), 簇 $(EXFAT_CLU),))..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(EXFAT_IMG) bs=1M count=$(EXFAT_MIB) status=none
	mkfs.exfat -L MORIONUSB $(if $(EXFAT_CLU),-c $(EXFAT_CLU),) $(EXFAT_IMG) >/dev/null
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
# 还放入 hello.mex —— 可执行文件加载 (E1/E2) 的演示程序: `run /hello.mex` 从这张盘上
# 加载它。它是**独立编译的 ELF**, 故镜像依赖 $(HELLO_ELF) (hello 变了就重建镜像)。
#
# 以及 `/system/services/*.elf` (E3c): 监督者 init 的**重启源** —— 它从这张根盘读回服务
# 镜像, 用 `SYS_SPAWN_ELF_AT` 把退出的服务原地拉起来。故镜像依赖 $(SRV_STAMP)。
$(NVME_IMG): $(HELLO_ELF) $(SRV_STAMP) Makefile
	@echo "==> 创建 NVMe 磁盘镜像 (FAT32$(if $(NVME_CLU), 簇 $(NVME_CLU) 扇区,)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(NVME_IMG) bs=1M count=64 status=none
	mkfs.fat -F 32 $(if $(NVME_CLU),-s $(NVME_CLU),) $(NVME_IMG) >/dev/null 2>&1
	@printf 'Hello from FAT32!\nThis is a test file.\n' > $(OUT_DIR)/hello.txt
	mcopy -i $(NVME_IMG) $(OUT_DIR)/hello.txt ::/hello.txt
	mmd -i $(NVME_IMG) ::/dir1
	@printf 'nested file via path!\n' > $(OUT_DIR)/nested.txt
	mcopy -i $(NVME_IMG) $(OUT_DIR)/nested.txt ::/dir1/nested.txt
	@printf 'long name read via VFAT LFN!\n' > $(OUT_DIR)/longname.txt
	mcopy -i $(NVME_IMG) $(OUT_DIR)/longname.txt ::/"Long File Name.txt"
	mcopy -o -i $(NVME_IMG) $(HELLO_ELF) ::/hello.mex
	# 服务镜像 (E3c): init 的重启源。与 ESP 上那份同源 —— 都在 build/user/srv/。
	mmd -i $(NVME_IMG) ::/system
	mmd -i $(NVME_IMG) ::/system/services
	@for n in $(SRV_NAMES); do \
		mcopy -o -i $(NVME_IMG) $(SRV_DIR)/$$n.elf ::/system/services/$$n.elf; \
	done
	@echo "  ✓ NVMe 镜像: $(NVME_IMG)"

# 创建 IDE 磁盘镜像并格式化为 FAT32
$(DISK_IMG): Makefile
	@echo "==> 创建 IDE 磁盘镜像 (FAT32)..."
	$(MKDIR) $(OUT_DIR)
	dd if=/dev/zero of=$(DISK_IMG) bs=1M count=1024 status=none
	mkfs.fat -F 32 $(DISK_IMG) >/dev/null 2>&1
	@printf 'Hello from FAT32!\nThis is a test file.\n' > $(OUT_DIR)/hello.txt
	mcopy -i $(DISK_IMG) $(OUT_DIR)/hello.txt ::/hello.txt
	mmd -i $(DISK_IMG) ::/dir1
	@printf 'nested file via path!\n' > $(OUT_DIR)/nested.txt
	mcopy -i $(DISK_IMG) $(OUT_DIR)/nested.txt ::/dir1/nested.txt
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
#
# 依赖 $(KERNEL_EMBED) —— 两个 crate 都靠 include_bytes! 嵌生成物, 干净树上必须先生成:
#   kernel/src/main.rs   → build/user/srv/*.elf   (SRV_ELFS)
#   boot/src/main.rs     → boot/loader/morion-kernel.elf (KERNEL_EMBED, 见 .gitignore)
# $(KERNEL_EMBED) 的依赖链已经把 SRV_ELFS 带上 (KERNEL_EMBED ← KERNEL_ELF ← SRV_STAMP),
# 所以写这一个就够。缺了它, 干净克隆上第一个包就报
# `error: couldn't read .../sender.elf: No such file or directory`。
# 本地不易发现: build/ 早被前面的 `make` 填好了 —— CI 是干净树, 才暴露。
.PHONY: check
check: $(KERNEL_EMBED)
	$(CARGO) check --package morion-kernel --target $(KERNEL_TARGET)
	$(CARGO) check --package morion-srv \
		--target user/x86_64-morion-user.json -Z json-target-spec \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem
	$(CARGO) check --package morion \
		--target user/x86_64-morion-user.json -Z json-target-spec \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem
	$(CARGO) check --package morion-hello \
		--target user/x86_64-morion-user.json -Z json-target-spec \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem
	$(CARGO) check --package morion-boot --target $(BOOT_TARGET)

.PHONY: fmt
fmt:
	$(CARGO) fmt --all -- --check

# clippy 是**门禁**: `-D warnings`, 五个 crate 任一有告警即失败。
# 同样依赖 $(KERNEL_EMBED): clippy 也会展开 kernel / boot 的 include_bytes! (见上)。
.PHONY: clippy
clippy: $(KERNEL_EMBED)
	$(CARGO) clippy --package morion-kernel --target $(KERNEL_TARGET) -- -D warnings
	$(CARGO) clippy --package morion-srv \
		--target user/x86_64-morion-user.json -Z json-target-spec \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem -- -D warnings
	$(CARGO) clippy --package morion \
		--target user/x86_64-morion-user.json -Z json-target-spec \
		-Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem -- -D warnings
	$(CARGO) clippy --package morion-hello \
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

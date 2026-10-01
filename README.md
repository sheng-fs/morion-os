<div align="center">

# 墨渊操作系统 · Morion OS

[中文](./README.md) | [English](./README.en.md)

---

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
[![Language](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org)
[![Arch](https://img.shields.io/badge/arch-x86__64%20|%20AArch64%20|%20RISC--V-brightgreen.svg)]()
[![Stage](https://img.shields.io/badge/stage-design%20&%20rewrite-yellow.svg)]()
[![Platform](https://img.shields.io/badge/platform-UEFI-lightgrey.svg)]()
[![Security](https://img.shields.io/badge/security-CHERI%20|%20IOMMU-red.svg)]()
[![PRs](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)]()

</div>

---

## 概述

墨渊操作系统是一个基于 **Rust** 语言从零构建的现代操作系统。项目当前处于**重新设计与重写阶段**，彻底梳理了前期实现中的架构冲突，重新确立了以 **微内核 + 外核混合架构** 为核心的技术路线。

旧代码已归档（`legacy` 分支），主线重新开始。目前已从纯设计推进到**可运行的微内核原型**：可在 QEMU（UEFI）下启动进入用户态 Shell，并跑通「应用 → libvfs → 文件服务 → 块设备服务 → NVMe 磁盘」的完整文件读写链路。

### 核心理念

- **微内核可信基**：内核仅暴露 10~20 个系统调用（IPC、地址空间映射、保护域管理等），所有传统内核功能均由用户态服务实现。
- **外核高性能路径**：通过"性能飞地"机制，利用 IOMMU / CHERI 等硬件能力，让游戏、AI 等高性能应用直接操作硬件，实现零内核陷落、零数据拷贝。
- **能力安全模型**：抛弃传统 UID/GID 权限体系，以能力（Capability）作为唯一访问凭证，从根本上消除"root 可做任何事"的隐患。
- **Anykernel 双形态驱动**：同一套驱动源码可编译为用户态服务进程（共享场景）或直通库（高性能场景），共享超过 90% 的代码。

> 详细架构设计见 [docs/architecture.md](./docs/architecture.md)。

---

## 当前进展

系统已从设计文档落到**能在真机 / QEMU 上启动运行的微内核原型**。下表区分「已跑通」与「尚未开始」，
避免把设计目标误读为既有能力。

| 方向 | 状态 | 说明 |
|------|------|------|
| 微内核核心 | ✅ 已跑通 | 保护域、同步 / 异步 IPC、抢占式调度（含**带超时阻塞**）、地址空间与按需分页、中断路由（PIC + **LAPIC/MSI-X**，含**阻塞等中断**与**多向量 `wait_any`**）、能力系统（含**能力随 IPC 传递**） |
| 系统调用接口 | ✅ 约 40 个 | 编号与语义见 [docs/app-dev-guide.md](./docs/app-dev-guide.md) 第 3 节 |
| 能力安全模型 | ✅ 已跑通 | 能力槽 + **能力句柄**（打开时签发、每次 I/O 前校验、关闭时撤销），默认零能力；运行时可经 IPC **移交句柄**（移动）与**委派能力**（复制、无放大）——不必全靠启动期静态授权 |
| 用户态驱动 | ✅ 部分 | 键盘驱动（IRQ1）；块设备服务（NVMe 驱动，含 IDE PIO 回退）；网络驱动 **`net_srv`（virtio-net，域 16）**—— N2/N3 驱动已跑通（modern：解析 virtio 能力 / 取 MAC / 建 RX·TX virtqueue / `DRIVER_OK` / **MSI-X 中断驱动收帧**，并自发 ARP 请求收到网关应答 `NET1 … ARP reply OK`）。**通用设备授权（D1）**：内核 [`device.rs`](./kernel/src/device.rs) 交出 `DeviceGrant`（BAR + 连续 DMA 块 + MSI-X 参数，不含设备语义），内核**不再有 NVMe 专属代码**，队列布局与设备协议都回到驱动域 —— 驱动还可用 `SYS_DEVICE_CONFIG_READ` 自行解析自己那台设备的 PCI 能力，MSI-X 表在别的 BAR 上时内核按 `BIR` 另映射给驱动 |
| 用户态文件系统 | ✅ 部分 | FAT32（含 VFAT 长名）、tmpfs、原创 MorionFS v2（COW + 快照 + 空闲位图/空间回收 + 大文件间接块 + 变长目录项/长名 + 节点元数据 + inode 号间接层/硬链接/软链接 + **按卷几何格式化** + **显式格式化 `mkfs.mfs`、多卷与主卷切换**）、ext2 **只读**、exFAT（读 + 写，支持大容量/大簇卷） |
| 分区 / 卷层 | ✅ 已跑通 | block_srv 解析各盘 **MBR/GPT** 分区表 → 卷表，按卷首签名探测 FS 类型；**也能写分区表**（`part.create/del/wipe/reload`：建/删分区、清空、重读，GPT 与 MBR 都支持）；`dev` 已升级为「卷号」，块层支持多页 DMA（单命令 ≤ 128 KiB）；**多卷挂载**：同类的额外卷自动挂到 `/usb<卷号>`，一份代码可同时服务多块盘，为读真实 U 盘分区铺路 |
| Shell 与统一目录树 | ✅ 已跑通 | `help/echo/pwd/ls/cat/cd/mkdir/touch/rm/mv/ln/ln -s/chmod/truncate/stat/lstat/readlink/mkfs.mfs/mfs.primary/df/part.create/part.del/part.wipe/part.reload/clear`（`ls -l` 长格式，软链接显示为 `l`）；多文件系统经挂载层拼成单根 `/`，支持运行时挂载 |
| 图形 / GUI | 🚧 进行中 | **G1** 帧缓冲交用户态 `gfx_srv`（域 15）独占：新增 `Capability::Fb` + `SYS_FB_INFO/MAP/TAKEOVER`，接管后内核终端不再写屏（输出只留 COM1）。**G2** 绘制原语 `fill/rect/blit` + 共享表面 + 客户端库 [`libmorion::gfx`](./user/libmorion/src/gfx.rs)；`blit` 拷完**回读帧缓冲**校验通过才回成功（自测 GS-1，已肉眼确认画面）。**G3a** 文本渲染外移：字库（ASCII 8×16 + 汉字 16×16 `cjk.bin` ≈276 KB）与终端状态（光标/换行/滚动/清屏，按**显示列**排版）从内核搬到 [`gfx/`](./user/srv/src/gfx/)，新协议 `GFX_OP_TEXT/CLEAR/MOVE/QUERY`，落笔**逐像素写后回读**校验（自测 GT-1 断言 ASCII 5 列 / 汉字 10 列）。**G3b** shell 输出上屏：新增 `SYS_CONSOLE_READY(47)`，`libmorion` 的打印出口 `sink()` 支持按进程**镜像**一份到屏幕控制台（只有 shell 开），shell 的横幅/中文欢迎语/命令输出同时进串口与屏幕。**G3c** 内核卸掉汉字字库（−276 KB，内核 ELF 347 KB → 69 KB），终端降为 ASCII + 豆腐块，汉字渲染只在用户态。**G4** 输入搬出内核：新增 `SYS_KEY_PUSH(48)/SYS_KEY_READ(49)` 键字节队列（内核只做搬运），行编辑/回显落客户端库 `morion::console::readline`，内核侧输入机件全部删除、终端降为只输出。**G6** 服务自愈：`ipc::call` 不再永久挂起（超时 + 目标无存活任务即失败）+ 帧缓冲登记内核保留区间 + 重启丢弃目标邮箱旧请求 + 客户端重建共享会话 + `gfx_srv` 纳入 init 监督（自测 GS-2）。内核文本控制台仅剩引导期与 panic 输出 |
| 网络 / 虚拟化 / 飞地 / 包管理 | ⏳ 未开始 | 设计已确定，尚无实现 |
| 面向系统 AI 的能力接口 | 📐 已定规范 | 应用如何把功能暴露给系统 AI 见 [docs/app-dev-guide.md](./docs/app-dev-guide.md) 第 9 节 |

> 快速上手：构建与运行命令见 [docs/commands.md](./docs/commands.md)；
> 内核与接口速查见 [docs/dev-reference.md](./docs/dev-reference.md)；
> 应用开发（含 AI 可调用能力）见 [docs/app-dev-guide.md](./docs/app-dev-guide.md)；
> 文件系统路线见 [docs/roadmap-fs.md](./docs/roadmap-fs.md)；
> 驱动与飞地路线见 [docs/roadmap-driver.md](./docs/roadmap-driver.md)。

---

## 架构概览（目标）

```
┌──────────────────────────────────────────────────────┐
│                   普通应用层                          │
│    POSIX 接口 (libc)  |  高性能直通 API                │
├──────────────────────────────────────────────────────┤
│             用户态系统服务                             │
│  文件系统  │  网络栈  │  设备服务  │  安全服务          │
│  ext4/vfat │ TCP/IP  │  驱动服务  │  认证/审计         │
├──────────────────────────────────────────────────────┤
│        性能飞地 (Enclave) — 可选加速                   │
│   GPU 直通  │  NPU 直通  │  用户态网卡驱动             │
│   (IOMMU 强制隔离, CHERI 边界保护)                    │
├──────────────────────────────────────────────────────┤
│                    微内核                             │
│  IPC  │  调度  │  地址空间  │  中断路由  │  能力授权    │
└──────────────────────────────────────────────────────┘
```

---

## 核心设计

### 微内核原语

内核仅包含不可精简的最小化功能——

| 原语 | 说明 |
|------|------|
| `send` / `receive` / `call` | 同步/异步 IPC，支持能力传递 |
| `map` / `unmap` | 地址空间映射管理 |
| `create_domain` / `destroy_domain` | 保护域（进程）生命周期 |
| `schedule` | CPU 调度 |
| `allocate_frame` / `free_frame` | 物理内存帧管理 |
| `register_interrupt` / `ack_interrupt` | 中断授权与应答 |
| `create_enclave` | 硬件隔离飞地创建 |

### 外部页管理器

- 内核仅负责缺页捕获与转发，由用户态分页服务决策页面内容和置换策略
- 每个进程可指定专属分页器，支持按需分页、压缩内存池、网络存储等

### Anykernel 双形态驱动

| 形态 | 场景 | 特点 |
|------|------|------|
| **驱动服务进程** | 普通应用 | 设备共享、安全隔离、通过 IPC 间接访问 |
| **直通驱动库 (LibDevice)** | 游戏 / AI | 运行时直接链接，零内核陷落操作 MMIO/DMA |

同一套 Rust trait 接口，feature flag 切换编译后端，共享 >90% 代码。

### 性能飞地 (Enclave)

1. IOMMU 将设备 MMIO、DMA 窗口映射进进程地址空间
2. 链接 LibDevice 直通驱动库
3. GPU/NPU 命令直接提交，零内核干预

破坏半径被硬件锁死在飞地资源范围内。

### 基于能力的安全模型

- 能力为唯一访问凭证，不依赖 UID/GID
- 新进程默认零能力，由父进程显式授予
- POSIX 权限 API (`chmod`/`chown`) 由 libc 转译为能力操作
- 安全策略由用户态策略引擎解释，支持动态更新

### 用户态服务

所有传统内核功能以独立用户态进程运行：

| 服务 | 职责 | 状态 |
|------|------|------|
| 文件系统服务 | FAT32（含 VFAT 长名）、tmpfs、原创 MorionFS、ext2 只读、exFAT（读 + 写），通过 libvfs 统一接口 | ✅ 已实现（ext2 只读 / exFAT 读写） |
| 设备服务 | 块设备（NVMe 驱动）、键盘驱动、中断分发 | ✅ 部分实现 |
| Shell 服务 | 命令行解释器 + 统一目录树 / 运行时挂载 | ✅ 已实现 |
| 网络协议栈 | TCP/IP 用户态实现，支持零拷贝共享内存 | ⏳ 规划中 |
| 安全/审计服务 | 认证、策略引擎、入侵检测 | ⏳ 规划中 |
| 飞地管理器 | 飞地生命周期、日志流、迁移与暂停 | ⏳ 规划中 |
| 包管理器 | Nix 风格声明式构建、原子切换、版本回滚 | ⏳ 规划中 |
| GUI 服务 | 亚克力半透明风格桌面环境，高度可自定义 | ⏳ 规划中 |
| 音频 / 输入法 / 容器 / 时间 / 电源 / 日志 / 配置服务 | 系统基础支撑 | ⏳ 规划中 |
| AI 能力注册 / 网关服务 | 应用能力注册与发现、AI 调用鉴权与审计 | 📐 规范已定（见 [app-dev-guide.md](./docs/app-dev-guide.md) 第 9 节） |

### 虚拟化

- 微内核同时作为 Hypervisor（Intel VT-x / AMD-V）
- 支持 unikernel 及未经修改的 Linux/Windows 客户机
- PCIe 设备直通 (VT-d / IOMMU)、嵌套飞地

### 启动加载

- 基于 UEFI 原生运行，跳过传统实模式
- GOP 高分辨率引导菜单，亚克力主题
- Nix 闭包存储启动项，支持原子切换与回滚
- TPM 2.0 测量 + Secure Boot 验签
- kexec 热启动、多系统共存

---

## 仓库结构

### 当前实际结构

> 项目为 Rust workspace（根 `Cargo.toml`），当前包含 `boot`、`kernel`、`user/srv`、`user/libmorion`、
> `user/libdevice`、`user/hello`、`kernel_test` 等 crate。内核之外的全部系统服务（块设备 / 文件系统 / 挂载 / Shell / 键盘驱动等）
> 都是**各自独立的用户态程序**（`user/srv` 里一个服务一个 `[[bin]]` → 一份独立 ELF），由**引导器
> 在启动期从 ESP（`EFI/morion/services/`）读入**、经 `BootInfo` 模块表交给内核，内核再把它们
> 逐个载入各自固定域（E2b：不再是"一份扁平二进制按域 id 分流"；E3b：服务也不再进内核镜像）。
> UI 素材统一按用途归档：引导期资源在 `boot/loader/resources/`，系统全局资源在 `resources/system/`。

```
.
├── .github/
│   ├── workflows/            # GitHub Actions (自动同步到 Gitee)
│   ├── ISSUE_TEMPLATE/       # Issue 模板
│   └── PULL_REQUEST_TEMPLATE.md
├── boot/                     # UEFI 引导加载器 (morion-boot)
│   ├── asm/
│   │   └── boot_stub.asm     #   引导入口汇编存根
│   ├── loader/               # 引导期资源与配置
│   │   ├── entries/          #   启动项配置 (.conf)
│   │   │   └── morion.conf
│   │   ├── resources/        #   引导器主题资源 (BMP/PNG)
│   │   │   ├── animation/    #     加载动画帧
│   │   │   ├── background/   #     背景图 (dark/light/default/mask)
│   │   │   ├── icons/        #     分类图标 (dialog/power/security/system/ui)
│   │   │   ├── logo/         #     Logo 变体 (horizontal/monochrome/square/system)
│   │   │   ├── progress/     #     进度条 (bar_bg/bar_fill)
│   │   │   └── splash/       #     启动闪屏 (background/logo)
│   │   ├── kernel_placeholder.bin
│   │   ├── loader.conf       #   引导加载器配置
│   │   └── theme.toml        #   亚克力主题配置
│   └── src/
│       ├── boot/             #   引导流程 (loader/menu/kexec)
│       ├── config/           #   配置解析 (entries/theme)
│       ├── gfx/              #   图形渲染 (framebuffer/font/renderer/animation)
│       ├── security/         #   安全 (hash/secure_boot/tpm)
│       ├── lib.rs
│       └── main.rs
├── kernel/                   # 微内核 (morion-kernel, 最小可信基)
│   └── src/
│       ├── arch/             #   x86_64 架构 (gdt/idt/pic/pit/keyboard/pci)
│       ├── memory/           #   内存管理 (paging/frame_allocator)
│       ├── scheduler/        #   调度器 (context 上下文切换)
│       ├── video/            #   内核文本控制台 (framebuffer/font/logo/bg/unicode) —— 引导期与 panic 输出
│       ├── bootinfo.rs       #   引导信息 (内存图 + GOP 帧缓冲)
│       ├── cap.rs            #   能力系统 (能力槽 + 能力句柄表)
│       ├── domain.rs         #   保护域 (进程, 每域独立页表)
│       ├── elf.rs            #   ELF64 解析与校验 (可执行文件加载的信任边界)
│       ├── exec.rs           #   运行时加载 ELF → 建新域 → 映射 → 起任务
│       ├── ipc.rs            #   进程间通信
│       ├── irq.rs            #   中断路由 (中断即 IPC)
│       ├── device.rs         #   通用设备授权 (BAR / DMA / MSI-X → DeviceGrant)
│       ├── pager.rs          #   用户态分页器接口
│       ├── syscall.rs        #   系统调用入口与编号表
│       ├── lib.rs
│       └── main.rs
├── user/                     # 用户态: 运行库 + 驱动公共库 + 服务程序
│   ├── libmorion/            #   运行库 (crate `morion`): syscall / 打印 / libvfs / libgfx / 入口样板
│   ├── libdevice/            #   设备驱动公共库 (crate `libdevice`, D2/D2b): 设备授权 / MMIO / MSI-X / virtio 传输层+vring
│   ├── hello/                #   演示: **独立 ELF 程序** (由 SYS_SPAWN_ELF 运行时载入)
│   └── srv/                  #   系统服务 (crate `morion-srv`): 每个服务一个 [[bin]] → 一份独立 ELF
│       └── src/
│           ├── common.rs     #     各服务共用的线协议 / 块客户端 / 名字工具
│           ├── block_srv.rs  #     域 5  块设备驱动服务
│           ├── fat32_srv.rs  #     域 6  FAT32 文件服务
│           ├── app.rs        #     域 7  自测程序
│           ├── shell.rs      #     域 8  命令行
│           ├── mount_srv.rs  #     域 9  挂载服务
│           ├── tmpfs_srv.rs  #     域 10 内存文件系统
│           ├── mfs_srv.rs    #     域 11 MorionFS
│           ├── ext2_srv.rs   #     域 12 ext2 只读
│           ├── exfat_srv.rs  #     域 13 exFAT 读写
│           ├── init.rs       #     域 14 监督者 (巡检服务域, 退出后用内存镜像原地重启)
│           ├── gfx_srv.rs    #     域 15 图形服务 (持帧缓冲, 用户态渲染: 绘图原语 + 文本终端)
│           ├── net_srv.rs    #     域 16 网络驱动 (virtio-net; N0–N3: MSI-X 中断 + ARP 自测)
│           ├── virtio_blk_srv.rs  # 域 17 virtio-blk 块设备驱动 (D3: 通用授权, 读签名/写读回自测)
│           ├── gfx/          #     图形服务内部: framebuffer 视图 + 字库 (font/glyphs/cjk.bin) + 终端
│           ├── sender.rs / receiver.rs / pager.rs / echo.rs / kbd.rs  # 域 0..4 演示与键盘
│           └── bin/          #     18 个入口 (每个写 morion_main → 对应模块 run())
├── kernel_test/              # 早期引导联调用测试内核 (临时保留)
│   └── src/main.rs
├── resources/
│   └── system/               # 全局系统资源
│       ├── device/           #   设备图标 (.ico)
│       ├── file/             #   文件类型图标 (.ico)
│       ├── github/           #   GitHub 封面 (.png)
│       ├── icons/            #   通用 UI 图标 (.ico)
│       ├── logo/             #   系统 Logo (.ico/.svg/.png)
│       ├── service/          #   服务图标 (.ico)
│       └── terminal/         #   终端背景 (.raw)
├── docs/
│   ├── architecture.md       #   技术架构设计
│   ├── app-dev-guide.md      #   应用开发指南 (含 AI 可调用能力规范)
│   ├── dev-reference.md      #   内核与接口速查手册
│   ├── commands.md           #   构建 / 运行 / 验证命令
│   ├── shell-reference.md    #   Shell 使用参考
│   └── roadmap-fs.md         #   文件系统路线图
├── Cargo.toml                # Rust workspace (boot/kernel/user/kernel_test)
├── Cargo.lock
├── Makefile                  # 构建系统 (make iso/run/run-nvme/debug/check/clippy)
├── flake.nix                 # Nix 构建集成
├── rust-toolchain.toml       # Rust nightly 工具链
├── linker.ld                 # 内核链接脚本
├── .gitattributes
├── .gitignore
├── CONTRIBUTING.md
├── LICENSE
├── README.md
└── README.en.md
```

### 目标开发结构

```
├── bootloader/       # 引导加载器 (UEFI)
├── kernel/           # 内核源码
│   ├── arch/         #   架构相关 (x86_64 / AArch64 / RISC-V)
│   ├── core/         #   微内核核心 (IPC, 调度, 地址空间, 能力)
│   └── compat/       #   兼容层 (POSIX / Linux / RTOS)
├── services/         # 用户态系统服务
│   ├── fs/           #   文件系统服务
│   ├── net/          #   网络协议栈
│   ├── device/       #   设备服务 + 驱动
│   ├── security/     #   安全 / 认证 / 审计服务
│   ├── enclave/      #   飞地管理器
│   ├── gui/          #   GUI 服务
│   ├── shell/        #   Shell 服务
│   ├── audio/        #   音频服务
│   ├── ime/          #   输入法服务
│   └── ...           #   更多服务
├── userland/         # 用户空间
│   ├── libs/         #   libc, libvfs, libdevice 等
│   └── bin/          #   基本命令 (ls, cat, mkdir, rm)
├── resources/        # 资源文件
├── docs/             # 文档
└── pkg/              # 包管理 (Nix 风格)
```

---

## 开发路线

> 详细文件系统路线见 [docs/roadmap-fs.md](./docs/roadmap-fs.md)，图形子系统路线见 [docs/roadmap-gfx.md](./docs/roadmap-gfx.md)。勾选项表示**已在 QEMU 实机跑通**。

### 阶段一 — 微内核核心（基本完成）

- [x] 保护域（进程）创建与地址空间隔离
- [x] 同步 / 异步 IPC（`send` / `recv` / `call` / `reply`；96 字节载荷 + 共享内存传大数据）
- [x] 任务调度与上下文切换
- [x] 地址空间映射 / 解除映射 + 用户态分页器（按需分页）
- [x] 中断路由（「中断即 IPC」）+ MMIO / I/O 端口授权
- [x] 能力系统（能力槽 + 能力句柄：签发 / 校验 / 撤销）
- [x] **能力随 IPC 传递**（句柄移交 `SYS_HANDLE_SEND`：把已打开对象**移入**目标域，移动语义；能力委派 `SYS_CAP_SEND`：把自己持有的能力**复制**给对方，不允许放大。两者都要求 `SendTo(to)`。域 0/1/3 启动期自测覆盖 8 条正/负例：委派自己没有的能力被拒、往不可达域塞能力被拒、非法参数被拒、移走后原域句柄立即失效、重复委派幂等，以及**反面对照** —— receiver 拿到 `SendTo(3)` 前调 echo 必须失败、拿到后必须成功）

### 阶段二 — 基础服务（进行中）

- [x] PCI 枚举 + NVMe 用户态驱动（块设备服务，含 IDE PIO 回退）
- [x] MBR/GPT **分区解析 + 卷层**（block_srv 内，`dev` = 卷号；按卷首签名探测 FAT / exFAT / MFS / ext2）
- [x] **多卷挂载**：请求 tag 高 32 位携带卷编码、一次打开绑定一个卷、切卷时重解析该卷几何；各文件服务把自己那类的**额外卷**自动上报挂载（`/usb<卷号>`）；顺带把 fat32 簇缓冲从 2 页扩到 16 页（**64 KiB 簇**上限，真机 U 盘常见 32 KiB 簇可挂）、ext2 块组上限 16 → 4096
- [x] 文件系统：FAT32（含 VFAT 长名）/ tmpfs / 原创 MorionFS（COW 写时复制 + 快照）
- [x] ext2 **只读**兼容（挂载既有 Linux 分区）
- [x] exFAT 兼容（`exfat_srv` 域 13，挂载 `/usb`）：**只读** = 引导区 + boot checksum、FAT 链、目录 entry set、分配位图、upcase 表；**读写** = 位图分配/释放、FAT 链扩展、entry set 增删（含 NameHash/SetChecksum 生成），`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`（`rename`/`chmod`/`link` 除外）；**大容量卷** = 块层多页 DMA（单命令 ≤ 256 扇区 = 128 KiB，更大请求自动切段）+ exFAT 去掉 4 KiB 簇 / 4 KiB 位图 / 8 KiB upcase 三处硬上限（集群缓冲依簇大小动态分配、位图与 upcase 改按需扇区窗口）
- [x] libvfs + 挂载层：多文件系统拼成单根 `/`，支持运行时挂载
- [x] Shell（内置命令 + cwd 相对路径）与用户态键盘驱动
- [x] **MorionFS v2 格式定稿 + 空间回收 / GC**（空闲位图 + mark & sweep，快照仍引用的旧块不回收；旧 `MFS1` 盘首挂自动重格式化；自动格式化**仅限**空白盘与 MFS 盘，非 MFS 卷拒绝挂载以免误格式化真机 U 盘分区）
- [x] **MorionFS 大文件**（`MFS3`：1008 直接块 + 一/二级间接块，单文件上限 = 整卷容量，突破旧 ≈4 MiB）
- [x] **MorionFS 目录与长名**（`MFS4`：ext2 风格变长目录项 + `MFXI` 扩展目录块，名字 ≤255 字节、大小写敏感；IPC payload 32 → 96 字节以承载长路径）
- [x] **MorionFS 节点元数据**（`MFS5`：时间戳（CMOS RTC）/权限/owner/链接数，`rename`（跨目录）/`truncate`（稀疏）/`chmod`，shell 增 `mv`/`chmod`/`truncate`/`stat`/`ls -l`）
- [x] **MorionFS inode 号间接层 + 硬链接**（`MFS6`：目录项改存 inode 号，inode 表（索引块 → 表块）让多个名字共享一个对象；`ln` 落地；顺带删掉沿祖先链的逐级回写，写代价与目录深度无关）
- [x] **MorionFS 软链接**（`MFSL` 节点类型：目标内联在节点里；路径解析跟随（绝对/相对/中间分量）+ 限深防环 16 层；`stat`/`cat` 跟随而 `rm`/`mv` 作用于链接自身；`ls -l` 显示 `l`；shell 增 `ln -s`；配套 `readlink`（读回目标）与 `lstat`（看链接自身）；跨文件系统目标创建即拒绝）
- [x] **MorionFS 按卷几何格式化**（**M7**：block_srv 补 `Identify Namespace` 的 NSZE，整盘卷不再「容量未知」；首次格式化按该卷真实容量定尺寸 —— 此前无论卷多大都写死 16 MiB；挂载时校验盘上总块数不超过卷容量；**IDE PIO 回退路径也补上容量探测**（ATA IDENTIFY DEVICE，夹在 28 位 LBA 上限内），不再恒为 `sectors = 0`）
- [x] **MorionFS 显式格式化 + 多卷**（**M8 / S2**：新 tag `MKFS` + shell `mkfs.mfs <卷号>`，护栏**只接受空白卷或 MFS 卷**，FAT/exFAT/ext2 分区与不存在的卷号一律拒绝 —— 「新盘可以格、别人的分区绝不吞」；`mfs_load_state` 拆出后 mfs_srv **按请求切卷**并把额外 MFS 卷挂到 `/usb<卷号>`；block_srv 启动打印卷表 `vol: <卷号> … kind=…`；新增空白测试盘 + FS-22）
- [x] **MorionFS 容量扩容（位图外置）**（**S3a**：空闲位图移出超级块 —— 块 2/3 为 `MFBH` 位图头块、块 4 起为两份裸位图数据副本（`bb = ceil(total/32768)`）；`mfs_bmp_flush` 取代 `mfs_write_super` 作唯一提交出口，按脏区间增量落盘 + 每块 CRC32 校验；三张位图由编译期定长数组改为动态页窗口（挂载前按卷容量预算，只增不缩）；容量上限 ≈119 MiB → **≈127.25 GiB**；测试卷 64 → 256 MiB，新增 FS-23(a)）
- [x] **MorionFS 单文件突破 4 GiB**（**S3b**：VFS 协议 `offset`/`size` 端到端 **u64**（含 `Stat`/`DirEntry`）；文件节点 `size` → u64 并新增**三级间接块** `MFI3`（直接区 1008 → 1005，元数据偏移不变），块映射四段、单文件上限 ≈ 整卷容量；内部仍 32 位的 fat32/tmpfs/ext2/exFAT 在协议边界加守卫；GC 补齐 `MFI3` 可达标记；新增 FS-23(b)(c)）
- [x] **MorionFS 主卷切换**（**S2 补齐**：超级块 `+256` 存**主卷序号**（不升 magic，老卷为 0 = 非主卷）；`mkfs.mfs` 置「现有最大 + 1」并随提交落盘，认领时**序号最大者胜出** —— 于是「最近一次显式格式化过的卷」稳定地是**下次启动**的 `/mfs`，不再由卷表扫描顺序决定；`MKFS` 回复改为盘上回读的序号；新增 FS-24）
- [x] **MorionFS 只改标记换主卷**（**S2 补齐**：新 tag `MFS_SETPRIMARY_TAG` + shell `mfs.primary <卷号>` —— **不动数据**地把一块**已有数据**的 MFS 卷升为主卷（`mkfs.mfs` 换主卷会擦除，等于删数据）；护栏**只接受已是 MFS 的卷**、没有格式化兜底；与 mkfs 共用同一个只增序号；新增 FS-25）
- [x] **`df` 空间用量**（shell `df` 报 `/mfs` 的总量 / 已用 / 空闲块与使用率；目前**只有 MorionFS 上报容量**，其余服务不维护块分配，故不列）
- [x] **卷管理收口：写分区表**（**S2 收口**：block_srv 新增建/删/清空/重读分区表与裸读一扇区五个 opcode，**按 nsid 寻址**；GPT 写全「保护性 MBR + 主头/主项数组 + 盘尾备份」并算对头与项数组 CRC32，MBR 建/删也支持；风格按盘自适应、空白盘默认 GPT、起点 1 MiB 对齐、GUID 确定性派生；**只动表不动数据**，删到最后一个就整表清空；改动后立即重扫重建卷表；shell 加 `part.create/del/wipe/reload`；新增 `build/pt.img` + FS-26，含宿主 `sgdisk -v` 跨实现校验）
- [x] **NVMe 中断化（MSI/MSI-X）**（**S3**：内核补 LAPIC 最小支撑（`IA32_APIC_BASE` / `SVR` / `TPR` / `LVT0`-ExtINT 透传 / `EOI`）+ PCI 能力链表遍历与 MSI-X 定位，IDT 装 MSI 向量段 `0x50..0x5F`；**内核管中断配置**（LAPIC + PCI 配置空间 + 向量段），**驱动写 MSI-X 表**（表在那个 4 GiB 以上、内核到不了的 BAR 里）；中断不投 IPC 而只置「待处理位」（与块请求邮箱混用会打乱 `reply` 路由），驱动用新 syscall `SYS_IRQ_POLL` 取位、`SYS_MSIX_ENABLE` 请内核开 MSI-X；`submit_wait` 改「先等中断再查 CQE」，等不到就**自动回退轮询**。修掉一个隐蔽 bug：`Create I/O CQ` 漏了 **IEN=1**，该队列根本不投中断 —— 轮询看不出来，中断路径会一直等。运行期证据：`nvme: stats … irq_cmds=N poll_cmds=0 irqs=N mode=irq`）
- [x] **阻塞等中断（等待原语）**（**S4**：调度器补**带超时阻塞**（TCB `wake_deadline` + `block_current_timeout_ms`，`tick()` 到期唤醒），伪等待键 `irq_wait_token(vector)`（**S5 起改为按域取键 + 掩码**）让 `irq::set_pending` 直接**唤醒**等待该向量的驱动域（与 IPC 的域 id 键不重叠，不会误唤醒）；新 syscall `SYS_IRQ_WAIT`（阻塞等向量中断，超时返回 0）；空闲任务改 `hlt(); yield_now();`，被中断唤醒的域立刻接手。驱动 `submit_wait` 因此改为「`SYS_IRQ_POLL` 快路径 → 未命中 `SYS_IRQ_WAIT` 阻塞」，**去掉上一轮「每轮踢一次宿主」的自旋**，等不到中断仍是「轮数 × 超时」看门狗后粘性回退。实测中断路径 `irq_cmds=28672 poll_cmds=0` 零回退、自测 314 s（旧实现 342 s））
- [x] **多向量 + `wait_any`（中断/等待原语做深）**（**S5**：等待原语从「按向量取键」改为「**按域取键 + 向量掩码**」—— `irq_wait_token(domain)` + `irq::ANY_MASK[域]`；`SYS_IRQ_POLL` / `SYS_IRQ_WAIT` 入参由向量号改为**掩码**，返回**命中的向量号**，一次等多条队列。NVMe 建成 **admin + 2 条 I/O 队列**、每条 CQ 用**自己的向量**（0x50/0x51/0x52），驱动按段轮转选队列、I/O 完成等 `1<<IO_QUEUES` 掩码。修掉一个踩坑：`Create I/O CQ` 的 `CDW11` 里 **IV 必须等于完成队列下标**，写成队列序号会让 qid 1 与 admin 抢向量 0，I/O 完成永远等不到（13 条命令后即回退）。运行期证据：`nvme: stats cmds=8192 irq_cmds=8192 poll_cmds=0 irqs=8192 vecs=0x7 mode=irq`，三条向量都真实投递）
- [x] **终端中文渲染（汉字 / 全角 / 宽窄混排）**（内核终端原本只有 8x16 ASCII 位图，而 `SYS_PUTS` 传的是 UTF-8 —— 汉字被逐字节喂进 `draw_char` 后落进「不可打印」分支，中文因此完全不显示。新增 `video/unicode.rs` + 生成的字库 `video/cjk.bin`：字源 **GNU Unifont**（OFL-1.1），字符集 = **GB2312 全集** ∪ 仓库里出现过的非 ASCII 字符 ≈ 7500 字 / 276 KB，16x16 汉字占 **2 个字符格**；记录里**自带宽度**，内核无需维护 East Asian Width 表，缺字形画空心豆腐块。终端行模型由「字节 = 一列」改为**按显示列**：满行判定、渲染步进、光标折算与下划线宽度都按列，退格/←/→ 按字符走不切开多字节。字库由 `scripts/gen-cjk-font.py` 生成并随仓库提交，构建不依赖网络）（**内核侧字库已于 G3c 卸除**，字库与汉字渲染现归用户态 `gfx_srv`，见下方 G3c）
- [x] **可执行文件加载（ELF + 运行时 spawn）**（**E1**：内核新增 **ELF64 加载器**（`elf.rs` 全量校验 + `exec.rs` 映射）与新 syscall `SYS_SPAWN_ELF`（`Capability::Spawn` 门禁）：解析 `ET_EXEC` 镜像 → 建**新域**（`domain::create()` + 能力/邮箱/分页器表补行）→ 按段映射（一页只映射一次、新页清零、`.bss` 补零）→ 映射用户栈 → 起 Ring 3 任务，返回新域 id；新域**零能力**、分页器登记为加载者。配套：`MAX_TASKS` 16 → 32 + 内核堆 1 → 4 MiB（每任务 32 KiB 栈）、任务表满时**返回失败而不再 panic**、`Domain::new` **显式跳过 P4[1]**（否则运行时建域会与调用者共用用户空间页表 —— 既无隔离又会撞车）。演示程序 `user/hello` 是**独立 crate / 独立 ELF**：自测把它写进 `/tmp` 再从**文件**读回来加载运行，子程序自己打印 `exec:` 行。自测 **FS-27**）
- [x] **用户态运行库 libmorion + `run` 命令**（**E2a**：抽 `user/libmorion`（crate `morion`）= syscall 封装 + 打印 + `domain_id()` + libvfs + 入口样板（`_start`/`morion_main`/panic），`morion-user` 与 `morion-hello` 共用，`hello` 瘦成 20 行；新增 `exec::spawn_file(path)`；shell 加 **`run <file>`**（+ `Capability::Spawn`），把可执行文件加载变成**用户可见的功能**；FS-27 改为从**磁盘文件** `/hello.mex` 加载。交互实测 `run /hello.mex` → 新域 14 跑起来）
- [x] **服务拆成独立程序 + 域销毁/帧回收**（**E2b**：① 域销毁——`domain::destroy` 摘域表槽位 + 释放用户地址空间（逐页按帧引用计数归还、回收页表帧）+ 清各子系统按域状态（能力/句柄、邮箱、分页器、中断）+ 摘除并终止其任务；域 id **复用空槽**（`slot_for`），配 `SYS_DOMAIN_DESTROY/COUNT/FRAME_FREE`。② **退出即回收**——任务退出时若为本域最后一个任务，登记该域、由时钟 `tick` 在别的上下文销毁（不可就地拆自己的栈/页表），引导域白名单永不销毁。③ **服务拆成独立程序**——新建 `user/srv`（crate `morion-srv`）：14 个服务各一个 `[[bin]]` → 各一份**独立 ELF**（`cfg` 门控，一个 bin 只编自己的服务 + `common`），删掉 17814 行的单文件 `user/src/main.rs` 与 `morion-user`；内核改 `SERVICE_ELFS` 表 + `exec::spawn_elf_at` 逐个载入固定域，删 `load_user_program`；每个程序启动打印 `[up] <name> (domain N)`。自测新增 **FS-28**，交互 `run` 域号复用）
- [x] **服务生命周期收口（E3a / E3b / E3c）**：① **用户页 W^X**（E3a）——段权限 → 页权限（`.text` RX、其余 RW+NX）、开 `EFER.NXE`、拒绝 W+X 段与页，链接脚本在 `.data` 前页对齐（否则三段挤在一页，页级 W^X 不可能满足）；顺带把用户态 `P=1` 保护违例改为**终止该任务**而不是转给分页器。② **服务移出内核镜像**（E3b）——引导器从**自己所在的 ESP** 读 `EFI/morion/services/*.elf`（`LOADER_DATA` 页），经扩展 `BootInfo` 的模块表交给内核按固定域号加载；内核 `include_bytes!` 全删，**内核 ELF 体积 −51%**（702 KB → 345 KB）；引导期进度与失败原因镜像到 COM1。③ **监督者 + 原地重启**（E3c）——新增域 14 `init`：巡检被监督服务域（`SYS_DOMAIN_ALIVE`），发现实例退出就从 FAT32 根盘 `/system/services/*.elf` 读回镜像、经 `SYS_SPAWN_ELF_AT` **原地**重启（域号不变，`domain::reset` 清用户地址空间但保留域与其分页器/能力注册）；自测 **FS-29** 让 echo 自杀再被拉起。④ **重启不依赖盘**（E3c 后续）——新增 `SYS_SPAWN_ELF_MODULE`：内核按域号从 E3b 的引导模块**内存镜像**取，init 重启优先走它、失败才回退盘；监督范围因此覆盖到 `pager / fat32_srv / mfs_srv`（文件服务自己崩了也能自救，解掉"读盘要靠文件服务"的鸡生蛋问题），唯一排除的是内核为其保留 NVMe 映射的 `block_srv`
- [x] **图形子系统 G1（帧缓冲用户态化）**（帧缓冲从内核交到用户态 `gfx_srv`（域 15）独占：新增无参能力 `Fb` 与三个 syscall —— `SYS_FB_INFO`（取几何）/ `SYS_FB_MAP`（整块映射进本域）/ `SYS_FB_TAKEOVER`（宣告接管）；内核 `video` 加 `FB_TAKEN_OVER`，置位后终端不再写帧缓冲、只留 COM1（headless 回归不受影响）；`gfx_srv` 取几何 → 映射到 `USER_BASE+1 GiB` → 画测试图案 → **回读校验** → 接管 → 清屏写横幅，打印 `gfx: 1280x800 text console ready (kernel console detached)`。规划见 [docs/roadmap-gfx.md](./docs/roadmap-gfx.md)）
- [x] **图形子系统 G2（绘制原语 + 共享表面 + `libmorion::gfx`）**（`gfx_srv` 加请求循环与 `GFX_OP_FILL/RECT/BLIT/PING`；客户端 `Surface` 用 `SYS_ALLOC_PAGE`+`SYS_SHARE_PAGE` 把表面**同址**共享给服务（地址按域 id 错开，避免多客户端撞 `PageAlreadyMapped`）；`blit` 拷完**回读帧缓冲**抽 5 点比对、全等才回 1 —— 无显示器也能断言；自测 GS-1。顺带把 `CAP_SLOTS` 16 → 32 并让 `grant` 槽满时报 `[WARN]`：app 的槽被 `gfx` 的两张凭证占满会导致其后的 `SendTo(echo)` 静默失败）
- [x] **图形子系统 G3a（服务内终端 + 文本渲染外移）**（字库（ASCII 8×16 + 汉字 16×16 `cjk.bin` ≈276 KB，`git mv` 进服务）与终端状态（光标/换行/滚动/清屏，按**显示列**排版，宽窄混排）从内核搬到 `gfx_srv` 的 `gfx/` 模块；新协议 `GFX_OP_TEXT/CLEAR/MOVE/QUERY`，文本经**共享页**传；落笔**逐像素写后回读**校验，不一致就回 0 —— 无显示器也能断言；自测 GT-1 断言 `"GT-1 "` 前进 5 列、`"汉字宽字符"` 前进 10 列（5×2），越界 `move_cursor` 必被拒）
- [x] **图形子系统 G3b（shell 输出上屏）**（新增 `SYS_CONSOLE_READY(47)`（无能力门禁，回答"显示是否已交用户态"）；`libmorion::syscall` 把打印唯一出口收成 `sink()`（`SYS_PUTS` → 内核终端 + COM1，**可选**镜像给 `gfx::print`），开关 `screen_mirror_on()` **按进程 opt-in**（只有 shell 打开 —— 自测成千上万条打印不该每条多一次 IPC 往返）；shell 启动**有界等待**控制台就绪（`CONSOLE_WAIT_MS = 1000`，到点退回只写串口，绝不卡死）并确认 `gfx_srv` 活着后开镜像；之后用 `GFX_OP_QUERY` 问光标做**盲测自证**：`shell: screen console mirror OK (gfx_srv cursor advanced)`。**接管只停重绘、不停输入**：`term_put` 的编辑与回车提交、`SYS_READLINE` 唤醒照旧执行（早退会导致 shell 永远收不到命令，表现为终端卡死）。已知限制归 G4：输入行回显仍在内核终端，屏幕上打字看不见）
- [x] **图形子系统 G3c（内核卸 CJK 字库：−276 KB）**（内核侧 `cjk.bin` 的 `include_bytes!`、定长记录二分查表与「按字形宽度排版」全部删除 —— 内核不再携带任何汉字点阵，**内核 ELF 347376 → 70504 字节（−276872 / ≈ −80%）**；只留接管前那几秒 + panic 屏够用的最小能力：UTF-8 `decode`/`prev_index`/`next_index`（退格与左右移不切开多字节）、宽度改按**东亚宽度**粗判（ASCII 1 格、汉字类 2 格，与用户态口径一致）、非 ASCII 画**空心豆腐块**。代价是 `gfx_srv` 接管前那几秒屏上中文是豆腐块、panic 屏同理；**COM1 串口全程原样 UTF-8**，headless 回归判据不受影响。`video/unicode.rs` 加 4 条 host 单测（内核单测 16 → 20））
- [x] **图形子系统 G4（输入搬出内核）**（内核侧新增唯一的输入机件 `key.rs`：64 字节环形键队列 + `SYS_KEY_PUSH(48)`（中断/驱动推字节）/ `SYS_KEY_READ(49)`（阻塞取键，空则 `block_current(KEY_WAIT)` 睡下、有键 `wake_one` 唤醒）—— 内核只做"按键字节搬运"、不再理解编辑语义。行编辑/回显改落客户端库 `morion::console::readline`（可打印字符入缓冲并回显、`\b`/`0x7F` 退格删缓冲并擦屏、回车提交，`\b` 由 `Term::write` 左移一列 + 涂背景色实现）；`kbd_srv` 的所有输入动作改为 `sys_key_push` 推字节。内核侧输入机件全删：`term_put / term_backspace / term_left/right / input_read / INPUT_QUEUE / SCROLL_OFFSET / CURSOR_*` 与 `SYS_TERM_PUT / SYS_BACKSPACE / SYS_SCROLL_UP/DOWN / SYS_TERM_LEFT/RIGHT / SYS_READLINE`，`video` 终端降为**只输出**（512 行历史环 + 当前行，只服务引导期与 panic 屏）；`INPUT_WAIT` → `KEY_WAIT`。**顺带修掉一个真竞态**：`gfx_srv` 原"整屏绘制 → 回读校验 → 接管"在接管前会被内核重绘擦掉被校验的像素 → 假失败；改为"顶部带 `probe` 探测映射可写 → 接管 → 独占后 `paint`+`verify`"。内核 ELF 70504 → **66040 字节**；`key.rs` 加 3 条 host 单测（内核单测 20 → 23）。无头实测（QEMU `sendkey`）：注入 `help⏎` 屏幕命令列表、`echo hiZ⌫⏎` 串口回显 `echo hiz\x08\r\nhi`，证明"取键 → 回显 → 退格删缓冲并擦屏 → 回车提交"整条链路）
- [x] **图形子系统 G6（图形服务自愈：监督重启 + 客户端会话重建）**（补掉 G4 后遗留的真实缺口：`gfx_srv` 一崩，屏幕永久死，且 `ipc::call` 的调用方会**永久挂起**。内核三处 —— ① `ipc::call` 改**带超时轮询**（`CALL_POLL_MS=200`）并在目标域**无存活任务**时立即失败返回 `u64::MAX`（挂死根因：调用方阻塞在**自己的域键**上、服务死在回复前就没人回它）；② 帧缓冲登记为**内核保留区间**（`frame_allocator::pin_range`，`free_frame` 空操作）—— `SYS_FB_MAP` 走 `map_mmio` 不计引用计数，「同域重启」清地址空间时会误把显存当独占帧释放；③ 「原地重启」加 `ipc::remove_domain` **丢弃目标域邮箱里未处理的请求**（否则新实例去读旧请求的悬空共享页而再崩）。客户端 [`morion::gfx`](./user/libmorion/src/gfx.rs)：`call` 收 `u64::MAX` 作废会话；`ensure_shared` 重建**只重发 `share` 不再 `alloc`**、且先**有界等待服务活过来**；服务端对悬空共享页回新码 `GFX_REPLY_NO_SESSION(2)`，`print`/`blit` **重建共享再试一次**（覆盖"重启落在两次调用之间、客户端没察觉"）。服务侧加 `GFX_OP_EXIT(8)` 自测钩子（先回复再退出）；`init` 把 `gfx_srv(15)` 纳入 `SUPERVISED`（10 个）。顺带修：GT-1 的列数断言原依赖**全局光标**、被 shell 启动输出插队会偏大 → 改**重试到干净窗口**。自测 **GS-2**：杀 `gfx_srv` 两轮（空窗期不调用走 `NO_SESSION` 重建 / 空窗期调用走快速失败），日志 `init: restarted gfx_srv (domain 15, total N, from memory)` ×2 + `app: GS2 gfx_srv restart + client session rebuild OK (screen recovered)`)
- [ ] **更多文件系统兼容**（ext4 写、UDF 等）
- [ ] 网络协议栈

### 阶段三 — 性能飞地（未开始）

- [ ] IOMMU 直通（**E1a 已做**：ACPI DMAR 探测 + DRHD 取证；**E1b 已做**：建根表/上下文表 + 恒等二级页表并打开 `GCMD.TE`，全量回归在 `-device intel-iommu` 下全绿；E1c 越界拒绝待做，见 [docs/roadmap-driver.md](./docs/roadmap-driver.md)）、LibDevice 直通驱动库、飞地管理器

### 阶段四 — 网络与安全（未开始）

- [x] **网卡驱动（virtio-net）**（`net_srv` 域 16 + 通用设备授权，见 [docs/roadmap-driver.md](./docs/roadmap-driver.md) 的 N0–N3：virtio-modern bring-up + MSI-X 中断收帧 + ARP 端到端自测）
- [x] **第二个真实驱动（virtio-blk）**（`virtio_blk_srv` 域 17，仍走通用设备授权、内核无设备专属逻辑：D3 —— 读签名 / 写读回自测）
- [ ] TCP/IP 协议栈、能力审计、策略引擎

### 阶段五 — GUI 与生态（未开始）

- [ ] 桌面环境、包管理、虚拟化

### 贯穿 — 面向系统 AI 的能力接口（规范已定，实现未开始）

- [x] 设计规范：应用如何把功能标注 / 描述为 **AI 可调用能力**（见 [docs/app-dev-guide.md](./docs/app-dev-guide.md) 第 9 节）
- [ ] 能力注册与发现服务
- [ ] AI 调用网关（能力校验 / 审计日志 / 超时与限额）

---

## 许可证

本项目采用 [MIT 许可证](./LICENSE)。

---

## 联系方式

- 项目主页：[github.com/sheng-fs/morion-os](https://github.com/sheng-fs/morion-os)
- 邮箱：3555679134@qq.com

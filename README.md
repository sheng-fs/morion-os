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
| 微内核核心 | ✅ 已跑通 | 保护域、同步 / 异步 IPC、抢占式调度、地址空间与按需分页、中断路由、能力系统（含**能力随 IPC 传递**） |
| 系统调用接口 | ✅ 约 33 个 | 编号与语义见 [docs/app-dev-guide.md](./docs/app-dev-guide.md) 第 3 节 |
| 能力安全模型 | ✅ 已跑通 | 能力槽 + **能力句柄**（打开时签发、每次 I/O 前校验、关闭时撤销），默认零能力；运行时可经 IPC **移交句柄**（移动）与**委派能力**（复制、无放大）——不必全靠启动期静态授权 |
| 用户态驱动 | ✅ 部分 | 键盘驱动（IRQ1）；块设备服务（NVMe 驱动，含 IDE PIO 回退） |
| 用户态文件系统 | ✅ 部分 | FAT32（含 VFAT 长名）、tmpfs、原创 MorionFS v2（COW + 快照 + 空闲位图/空间回收 + 大文件间接块 + 变长目录项/长名 + 节点元数据 + inode 号间接层/硬链接/软链接 + **按卷几何格式化** + **显式格式化 `mkfs.mfs`、多卷与主卷切换**）、ext2 **只读**、exFAT（读 + 写，支持大容量/大簇卷） |
| 分区 / 卷层 | ✅ 已跑通 | block_srv 解析各盘 **MBR/GPT** 分区表 → 卷表，按卷首签名探测 FS 类型；**也能写分区表**（`part.create/del/wipe/reload`：建/删分区、清空、重读，GPT 与 MBR 都支持）；`dev` 已升级为「卷号」，块层支持多页 DMA（单命令 ≤ 128 KiB）；**多卷挂载**：同类的额外卷自动挂到 `/usb<卷号>`，一份代码可同时服务多块盘，为读真实 U 盘分区铺路 |
| Shell 与统一目录树 | ✅ 已跑通 | `help/echo/pwd/ls/cat/cd/mkdir/touch/rm/mv/ln/ln -s/chmod/truncate/stat/lstat/readlink/mkfs.mfs/mfs.primary/df/part.create/part.del/part.wipe/part.reload/clear`（`ls -l` 长格式，软链接显示为 `l`）；多文件系统经挂载层拼成单根 `/`，支持运行时挂载 |
| 图形 / GUI | ⏳ 未开始 | 目前仅有内核帧缓冲**文本控制台**；帧缓冲 MMIO 映射能力（`sys_map_mmio`）已就绪 |
| 网络 / 虚拟化 / 飞地 / 包管理 | ⏳ 未开始 | 设计已确定，尚无实现 |
| 面向系统 AI 的能力接口 | 📐 已定规范 | 应用如何把功能暴露给系统 AI 见 [docs/app-dev-guide.md](./docs/app-dev-guide.md) 第 9 节 |

> 快速上手：构建与运行命令见 [docs/commands.md](./docs/commands.md)；
> 内核与接口速查见 [docs/dev-reference.md](./docs/dev-reference.md)；
> 应用开发（含 AI 可调用能力）见 [docs/app-dev-guide.md](./docs/app-dev-guide.md)；
> 文件系统路线见 [docs/roadmap-fs.md](./docs/roadmap-fs.md)。

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

> 项目为 Rust workspace（根 `Cargo.toml`），当前包含 `boot`、`kernel`、`user`、`kernel_test` **四个 crate**。
> 其中 `user` 是一个**扁平二进制**，内核之外的全部系统服务（块设备 / 文件系统 / 挂载 / Shell / 键盘驱动等）
> 都在其中按**域 id** 分流实现，由内核在启动时加载。
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
│       ├── video/            #   帧缓冲文本控制台 (framebuffer/font/logo/bg)
│       ├── bootinfo.rs       #   引导信息 (内存图 + GOP 帧缓冲)
│       ├── cap.rs            #   能力系统 (能力槽 + 能力句柄表)
│       ├── domain.rs         #   保护域 (进程)
│       ├── ipc.rs            #   进程间通信
│       ├── irq.rs            #   中断路由 (中断即 IPC)
│       ├── nvme.rs           #   NVMe 控制器初始化 (队列 / DMA)
│       ├── pager.rs          #   用户态分页器接口
│       ├── syscall.rs        #   系统调用入口与编号表
│       ├── lib.rs
│       └── main.rs
├── user/                     # 用户态程序与系统服务 (morion-user, 扁平二进制)
│   └── src/
│       ├── syscall.rs        #   系统调用封装 + 打印辅助 (libuser)
│       ├── vfs.rs            #   libvfs: fd / 挂载路由 / 能力句柄守卫
│       └── main.rs           #   _start 按域 id 分流: 各服务与 shell 实现
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

> 详细文件系统路线见 [docs/roadmap-fs.md](./docs/roadmap-fs.md)。勾选项表示**已在 QEMU 实机跑通**。

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
- [ ] **更多文件系统兼容**（ext4 写、UDF 等）
- [ ] 可执行文件加载（当前所有服务共用一份扁平二进制，按域 id 分流）
- [ ] 帧缓冲对用户态开放 / GUI 服务
- [ ] 网络协议栈

### 阶段三 — 性能飞地（未开始）

- [ ] IOMMU 直通、LibDevice 直通驱动库、飞地管理器

### 阶段四 — 网络与安全（未开始）

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

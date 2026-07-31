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

旧代码已归档（`legacy` 分支），主线重新开始。

### 核心理念

- **微内核可信基**：内核仅暴露 10~20 个系统调用（IPC、地址空间映射、保护域管理等），所有传统内核功能均由用户态服务实现。
- **外核高性能路径**：通过"性能飞地"机制，利用 IOMMU / CHERI 等硬件能力，让游戏、AI 等高性能应用直接操作硬件，实现零内核陷落、零数据拷贝。
- **能力安全模型**：抛弃传统 UID/GID 权限体系，以能力（Capability）作为唯一访问凭证，从根本上消除"root 可做任何事"的隐患。
- **Anykernel 双形态驱动**：同一套驱动源码可编译为用户态服务进程（共享场景）或直通库（高性能场景），共享超过 90% 的代码。

> 详细架构设计见 [docs/architecture.md](./docs/architecture.md)。

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

| 服务 | 职责 |
|------|------|
| 文件系统服务 | ext4、FAT32、tmpfs 等，通过 libvfs 统一接口 |
| 网络协议栈 | TCP/IP 用户态实现，支持零拷贝共享内存 |
| 设备服务 | 驱动管理、中断分发 |
| 安全/审计服务 | 认证、策略引擎、入侵检测 |
| 飞地管理器 | 飞地生命周期、日志流、迁移与暂停 |
| 包管理器 | Nix 风格声明式构建、原子切换、版本回滚 |
| GUI 服务 | 亚克力半透明风格桌面环境，高度可自定义 |
| Shell 服务 | 命令行解释器 |
| 音频 / 输入法 / 容器 / 时间 / 电源 / 日志 / 配置服务 | 系统基础支撑 |

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

```
.
├── boot/
│   └── loader/
│       └── theme.toml     # 引导加载器主题配置
├── docs/
│   └── architecture.md    # 架构设计文档
├── resources/
│   ├── animation/         # 动画资源
│   │   └── loading/       #   加载帧动画 (.png)
│   ├── background/        # 背景图 (dark/light/default/mask)
│   ├── cursor/            # 光标 (default/hover/loading)
│   ├── icons/             # 分类图标 (PNG)
│   │   ├── dialog/        #   对话框 (bg/error/info/overlay/warning)
│   │   ├── power/         #   电源 (kexec/reboot/shutdown)
│   │   ├── security/      #   安全 (enclave/lock/secure_boot/tpm/...)
│   │   ├── system/        #   系统 (default/linux/windows/uefi_settings/...)
│   │   └── ui/            #   界面 (about/console/log/refresh/rollback/...)
│   ├── images/            # 图片资源
│   │   ├── boot/          #   启动画面 (.bmp)
│   │   ├── device/        #   设备图标 (.ico)
│   │   ├── file/          #   文件类型图标 (.ico)
│   │   ├── github/        #   GitHub 封面 (.png)
│   │   ├── icons/         #   通用 UI 图标 (.ico)
│   │   ├── logo/          #   系统 Logo (.ico, .svg, .png)
│   │   ├── service/       #   服务图标 (.ico)
│   │   └── terminal/      #   终端背景 (.raw)
│   ├── logo/              # Logo 变体 (horizontal/monochrome/square)
│   ├── progress/          # 进度条 (bar_bg/bar_fill)
│   └── splash/            # 启动闪屏 (background/logo)
├── .gitattributes
├── .gitignore
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

- [ ] **阶段一**：微内核核心 — IPC、调度、地址空间、能力系统
- [ ] **阶段二**：基础服务 — 文件系统、设备驱动、Shell
- [ ] **阶段三**：性能飞地 — IOMMU 直通、LibDevice、飞地管理器
- [ ] **阶段四**：网络与安全 — TCP/IP 协议栈、能力审计、策略引擎
- [ ] **阶段五**：GUI 与生态 — 桌面环境、包管理、虚拟化

---

## 许可证

本项目采用 [MIT 许可证](./LICENSE)。

---

## 联系方式

- 项目主页：[github.com/sheng-fs/morion-os](https://github.com/sheng-fs/morion-os)
- 邮箱：3555679134@qq.com

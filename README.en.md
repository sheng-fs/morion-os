<div align="center">

# Morion OS

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

## Overview

Morion OS is a modern operating system built from scratch in **Rust**. The project is currently in a **redesign & rewrite phase**, having thoroughly re-examined architectural conflicts from earlier implementations and re-established the **microkernel + exokernel hybrid architecture** as the core technical direction.

Legacy code has been archived (`legacy` branch); the mainline is starting fresh.

### Core Principles

- **Microkernel Trusted Computing Base**: The kernel exposes only 10–20 system calls (IPC, address space mapping, protection domain management, etc.). All traditional kernel functionality is implemented by user-space services.
- **Exokernel Performance Path**: Through the "Performance Enclave" mechanism, IOMMU / CHERI hardware capabilities allow high-performance applications (games, AI) to operate hardware directly — zero kernel traps, zero data copies.
- **Capability-Based Security Model**: Abandons traditional UID/GID permission systems. Capabilities serve as the sole access credential, fundamentally eliminating "root can do anything" vulnerabilities.
- **Anykernel Dual-Mode Drivers**: The same driver source compiles into either a user-space service process (shared scenario) or a direct-pass library (high-performance scenario), sharing over 90% of the code.

> Detailed architecture design: [docs/architecture.md](./docs/architecture.md).

---

## Architecture Overview (Target)

```
┌──────────────────────────────────────────────────────┐
│                  Application Layer                    │
│    POSIX Interface (libc)  |  High-Perf Direct API    │
├──────────────────────────────────────────────────────┤
│            User-Space System Services                 │
│  Filesystem │ Network Stack │ Device Svc │ Security   │
│  ext4/vfat  │    TCP/IP     │ Driver Svc │ Auth/Audit │
├──────────────────────────────────────────────────────┤
│       Performance Enclave — Optional Acceleration     │
│   GPU Direct  │  NPU Direct  │  Userspace NIC Driver  │
│   (IOMMU-enforced isolation, CHERI bounds protection) │
├──────────────────────────────────────────────────────┤
│                    Microkernel                        │
│  IPC │ Scheduling │ Address Space │ IRQ Route │ Caps  │
└──────────────────────────────────────────────────────┘
```

---

## Core Design

### Microkernel Primitives

Contains only the irreducible minimal set:

| Primitive | Description |
|-----------|-------------|
| `send` / `receive` / `call` | Sync/async IPC with capability transfer |
| `map` / `unmap` | Address space mapping management |
| `create_domain` / `destroy_domain` | Protection domain (process) lifecycle |
| `schedule` | CPU scheduling |
| `allocate_frame` / `free_frame` | Physical memory frame management |
| `register_interrupt` / `ack_interrupt` | Interrupt authorization and acknowledgment |
| `create_enclave` | Hardware-isolated enclave creation |

### External Pager

- The kernel only captures and forwards page faults; user-space pager services decide content and replacement policy
- Each process can designate a dedicated pager — on-demand paging, compressed memory pools, network storage, etc.

### Anykernel Dual-Mode Drivers

| Mode | Target | Characteristics |
|------|--------|-----------------|
| **Driver Service Process** | General apps | Device sharing, secure isolation, indirect access via IPC |
| **Direct Driver Library (LibDevice)** | Gaming / AI | Runtime-linked, zero kernel traps for MMIO/DMA operations |

Single Rust trait interface, backend selection via compile-time feature flags, >90% code sharing.

### Performance Enclave

1. IOMMU maps device MMIO and DMA windows into the process address space
2. LibDevice direct-driver library is linked in
3. GPU/NPU commands submitted directly with zero kernel intervention

Blast radius is hardware-locked to the enclave's resource bounds.

### Capability-Based Security Model

- Capabilities as the sole access credential, no UID/GID dependency
- New processes start with zero capabilities, explicitly granted by the parent
- POSIX permission APIs (`chmod`/`chown`) translated to capability operations by libc
- Security policy interpreted by user-space policy engine, supports dynamic updates

### User-Space Services

All traditional kernel functionality runs as independent user-space processes:

| Service | Responsibility |
|---------|---------------|
| Filesystem Service | ext4, FAT32, tmpfs, etc., unified via libvfs |
| Network Stack | User-space TCP/IP, zero-copy shared memory |
| Device Service | Driver management, interrupt dispatch |
| Security / Audit | Authentication, policy engine, intrusion detection |
| Enclave Manager | Enclave lifecycle, log streams, migration & suspend |
| Package Manager | Nix-style declarative builds, atomic switching, version rollback |
| GUI Service | Acrylic translucent desktop, highly customizable |
| Shell Service | Command-line interpreter |
| Audio / IME / Container / Time / Power / Log / Config | System infrastructure services |

### Virtualization

- Microkernel also acts as a Hypervisor (Intel VT-x / AMD-V)
- Supports unikernels and unmodified Linux/Windows guests
- PCIe device passthrough (VT-d / IOMMU), nested enclaves

### Boot

- UEFI native, no legacy real-mode transitions
- GOP high-resolution boot menu, acrylic theme
- Nix closure-based boot entries, atomic switching & rollback
- TPM 2.0 measurement + Secure Boot verification
- kexec warm boot, multi-OS coexistence

---

## Repository Structure

### Current Structure

```
.
├── docs/
│   └── architecture.md    # Architecture design document
├── resources/
│   └── images/            # Image assets
│       ├── boot/          #   Boot screens (.bmp)
│       ├── device/        #   Device icons (.ico)
│       ├── file/          #   File type icons (.ico)
│       ├── github/        #   GitHub cover (.png)
│       ├── icons/         #   General UI icons (.ico)
│       ├── logo/          #   System logo (.ico, .svg)
│       ├── service/       #   Service icons (.ico)
│       └── terminal/      #   Terminal backgrounds (.raw)
├── .gitattributes
├── .gitignore
├── LICENSE
├── README.md
└── README.en.md
```

### Target Development Structure

```
├── bootloader/       # Bootloader (UEFI)
├── kernel/           # Kernel source
│   ├── arch/         #   Architecture-specific (x86_64 / AArch64 / RISC-V)
│   ├── core/         #   Microkernel core (IPC, scheduler, address space, caps)
│   └── compat/       #   Compatibility layers (POSIX / Linux / RTOS)
├── services/         # User-space system services
│   ├── fs/           #   Filesystem service
│   ├── net/          #   Network stack
│   ├── device/       #   Device service + drivers
│   ├── security/     #   Security / auth / audit
│   ├── enclave/      #   Enclave manager
│   ├── gui/          #   GUI service
│   ├── shell/        #   Shell service
│   ├── audio/        #   Audio service
│   ├── ime/          #   IME service
│   └── ...           #   More services
├── userland/         # User space
│   ├── libs/         #   libc, libvfs, libdevice, etc.
│   └── bin/          #   Basic commands (ls, cat, mkdir, rm)
├── resources/        # Resource files
├── docs/             # Documentation
└── pkg/              # Package management (Nix-style)
```

---

## Roadmap

- [ ] **Phase 1**: Microkernel core — IPC, scheduler, address spaces, capability system
- [ ] **Phase 2**: Foundation services — filesystem, device drivers, shell
- [ ] **Phase 3**: Performance enclaves — IOMMU passthrough, LibDevice, enclave manager
- [ ] **Phase 4**: Networking & security — TCP/IP stack, capability auditing, policy engine
- [ ] **Phase 5**: GUI & ecosystem — desktop environment, package manager, virtualization

---

## License

This project is licensed under the [MIT License](./LICENSE).

---

## Contact

- Project Homepage: [github.com/sheng-fs/morion-os](https://github.com/sheng-fs/morion-os)
- Email: 3555679134@qq.com

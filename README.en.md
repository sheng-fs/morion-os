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

Legacy code has been archived (the `legacy` branch) and the mainline restarted. It has since moved from pure design to a **runnable microkernel prototype**: it boots under QEMU (UEFI) into a user-space shell and drives the full file read/write path — "application → libvfs → filesystem service → block device service → NVMe disk".

### Core Principles

- **Microkernel Trusted Computing Base**: The kernel exposes a single boundary — the system-call interface; filesystems, networking, device drivers, graphics, the shell and every other traditional kernel function are implemented as user-space services. Only a handful of core primitives live in the kernel (IPC, address-space mapping, protection domains, capabilities) — and even within the full 0–53 syscall table these make up just a small part (see [docs/dev-reference.md](./docs/dev-reference.md) §5).
- **Exokernel Performance Path**: Through the "Performance Enclave" mechanism, IOMMU / CHERI hardware capabilities allow high-performance applications (games, AI) to operate hardware directly — zero kernel traps, zero data copies.
- **Capability-Based Security Model**: Abandons traditional UID/GID permission systems. Capabilities serve as the sole access credential, fundamentally eliminating "root can do anything" vulnerabilities.
- **Anykernel Dual-Mode Drivers**: The same driver source compiles into either a user-space service process (shared scenario) or a direct-pass library (high-performance scenario), sharing over 90% of the code.

> Detailed architecture design: [docs/architecture.md](./docs/architecture.md).

---

## Current Status

The system has moved from design documents to a **microkernel prototype that boots and runs on real hardware / QEMU**. The table below separates what is "working" from what is "not started", so design goals are not mistaken for existing capabilities.

| Area | Status | Notes |
|------|--------|-------|
| Microkernel core | ✅ Working | Protection domains, sync/async IPC, preemptive scheduling (incl. **timeout-bounded blocking**), address spaces and demand paging, interrupt routing (PIC + **LAPIC/MSI-X**, incl. **blocking interrupt waits** and **multi-vector `wait_any`**), capability system (incl. **capability transfer over IPC**) |
| Syscall interface | ✅ 46 (IDs 0–53) | Kernel's complete ID table: [docs/dev-reference.md](./docs/dev-reference.md) §5; application-facing subset: [docs/app-dev-guide.md](./docs/app-dev-guide.md) §3 |
| Capability security model | ✅ Working | Capability slots + **capability handles** (issued on open, checked before every I/O, revoked on close), zero capabilities by default; **handle transfer** (move) and **capability delegation** (copy, no amplification) at runtime over IPC — not everything must be statically granted at boot |
| User-space drivers | ✅ Partial | Keyboard driver (IRQ1); block device service (NVMe driver, with IDE PIO fallback); network driver **`net_srv` (virtio-net, domain 16)** — the N2/N3 driver works (modern virtio bring-up: parse virtio caps / read MAC / build RX·TX virtqueues / `DRIVER_OK` / **MSI-X interrupt-driven RX**, and its self-initiated ARP request is answered by the gateway `NET1 … ARP reply OK`). **Generic device grant (D1)**: the kernel's `device.rs` hands out a `DeviceGrant` (BAR + contiguous DMA block + MSI-X parameters, no device semantics); the kernel **no longer contains NVMe-specific code** — queue layout and device protocol live in the driver domain; a driver can also parse its own device's PCI capabilities via `SYS_DEVICE_CONFIG_READ`, and the kernel maps the MSI-X table from a different BAR per `BIR` |
| User-space filesystems | ✅ Partial | FAT32 (incl. VFAT long names), tmpfs, original MorionFS v2 (COW + snapshots + free bitmap/space reclaim + indirect blocks for large files + variable-length dirents/long names + node metadata + inode-number indirection layer/hard links/symlinks + **per-volume-geometry formatting** + **explicit `mkfs.mfs`, multi-volume and primary-volume switching**), ext2 **read-only**, exFAT (read + write, large and high-cluster-count volumes) |
| Partition / volume layer | ✅ Working | block_srv parses each disk's **MBR/GPT** partition table into a volume table and probes the FS type by the per-volume leading signature; it can also **write partition tables** (`part.create/del/wipe/reload`: create/delete partitions, wipe, rescan — both GPT and MBR); `dev` is now a "volume number" and the block layer supports multi-page DMA (single command ≤ 128 KiB); **multi-volume mount**: additional volumes of the same type auto-mount at `/usb<vol>`, so one codebase serves several disks — paving the way for reading real USB partitions |
| Shell & unified directory tree | ✅ Working | `help/echo/uname/version/pwd/ls/cat/cd/mkdir/touch/rm/mv/ln/ln -s/chmod/truncate/stat/lstat/readlink/mkfs.mfs/mfs.primary/df/part.create/part.del/part.wipe/part.reload/clear` (`ls -l` long format, symlinks shown as `l`; `uname`/`version` report the version string via `SYS_UNAME`); multiple filesystems are assembled into a single `/` root by the mount layer, with runtime mounting |
| Graphics / GUI | 🚧 In progress | **G1** framebuffer handed to user-space `gfx_srv` (domain 15) exclusively: adds `Capability::Fb` + `SYS_FB_INFO/MAP/TAKEOVER`; after takeover the kernel terminal no longer writes the screen (output stays on COM1). **G2** drawing primitives `fill/rect/blit` + shared surfaces + client library `libmorion::gfx`; `blit` re-reads the framebuffer and verifies before replying (selftest GS-1). **G3a** text rendering moved out: font (ASCII 8×16 + CJK 16×16 `cjk.bin` ≈276 KB) and terminal state move from the kernel to `gfx/`, new protocol `GFX_OP_TEXT/CLEAR/MOVE/QUERY`, per-pixel write-then-readback verification (selftest GT-1). **G3b** shell output on screen: `SYS_CONSOLE_READY(47)`; `libmorion`'s print sink `sink()` can **mirror** output to the screen console per process (only the shell opts in). **G3c** the kernel drops the CJK font (−276 KB, kernel ELF 347 KB → 69 KB); the kernel terminal falls back to ASCII + tofu and CJK rendering happens only in user space. **G4** input moved out of the kernel: `SYS_KEY_PUSH(48)/SYS_KEY_READ(49)` key-byte queue (the kernel only shuttles bytes), line editing/echo in the client library `morion::console::readline`. **G6** service self-healing: `ipc::call` no longer hangs forever (timeout + fail fast when the target has no live task), framebuffer registered as a reserved kernel range, stale mailbox requests dropped on restart, client rebuilds shared sessions, `gfx_srv` supervised by init (selftest GS-2). The kernel text console remains only for boot and panic output |
| Networking / virtualization / enclaves / package manager | 🚧 In progress | **Networking**: the `net_srv` virtio-net driver works and has passed an end-to-end ARP selftest; the TCP/IP stack is not done. **Enclaves**: IOMMU/VT-d hardware isolation is done (since E1c the target device window is constrained and out-of-window DMA is refused with evidence); the enclave manager E2/E3 is not done. **Virtualization & package management**: design settled, no implementation yet |
| System-AI capability interface | 📐 Spec defined | How an application exposes functionality to the system AI: [docs/app-dev-guide.md](./docs/app-dev-guide.md) §9 |

> Quick start (build & run commands): [docs/commands.md](./docs/commands.md);
> kernel and interface reference: [docs/dev-reference.md](./docs/dev-reference.md);
> application development (incl. AI-callable capabilities): [docs/app-dev-guide.md](./docs/app-dev-guide.md);
> filesystem roadmap: [docs/roadmap-fs.md](./docs/roadmap-fs.md);
> driver & enclave roadmap: [docs/roadmap-driver.md](./docs/roadmap-driver.md).

---

## Version

Current version **`0.4.0`**; the released graphics-less variant is tagged **`v0.4.0-nogui`**.

- The **single source of truth** for version constants is the kernel's `kernel/src/version.rs` (`SYSTEM_NAME` / `VERSION` / `MACHINE` / `VARIANT` / `BUILD`), reported to user space via `SYS_UNAME(51)` — the shell's `uname` / `version` commands read it (the build number is the git short hash injected by the `Makefile`).
- Two build variants:
  - **Regular (with graphics)**: `make ...`; shell output is additionally **mirrored** to `gfx_srv`'s screen console.
  - **No graphics**: `make NOGUI=1 ...`; the shell does not open the screen mirror (input/echo go through the serial port), the release string reports `0.4.0-nogui`, and artifacts land in `build/nogui/`. All other services and the full regression baseline are unchanged.
- See [CHANGELOG.md](./CHANGELOG.md) for the change history.

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

| Service | Responsibility | Status |
|---------|---------------|--------|
| Filesystem Service | FAT32 (incl. VFAT long names), tmpfs, original MorionFS, ext2 read-only, exFAT (read + write), unified via libvfs | ✅ Implemented (ext2 read-only / exFAT read-write) |
| Device Service | Block device (NVMe driver), keyboard driver, interrupt dispatch | ✅ Partial |
| Shell Service | Command-line interpreter + unified directory tree / runtime mounting | ✅ Implemented |
| Network Stack | User-space TCP/IP, zero-copy shared memory | ⏳ Planned |
| Security / Audit | Authentication, policy engine, intrusion detection | ⏳ Planned |
| Enclave Manager | Enclave lifecycle, log streams, migration & suspend | ⏳ Planned |
| Package Manager | Nix-style declarative builds, atomic switching, version rollback | ⏳ Planned |
| GUI Service | Acrylic translucent desktop, highly customizable (`gfx_srv` framebuffer / text console / drawing primitives implemented, desktop environment not started) | 🚧 In progress |
| Audio / IME / Container / Time / Power / Log / Config | System infrastructure services | ⏳ Planned |
| AI capability registration / gateway service | Application capability registration & discovery, AI call authorization and auditing | 📐 Spec defined (see [app-dev-guide.md](./docs/app-dev-guide.md) §9) |

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

The project is a Rust workspace (root `Cargo.toml`) currently containing the `boot`, `kernel`, `user/srv`, `user/libmorion`, `user/libdevice`, `user/hello` and `kernel_test` crates. Every system service outside the kernel (block device / filesystem / mount / shell / keyboard driver, …) is a **separate user-space program** (one `[[bin]]` per service in `user/srv` → one standalone ELF); the **bootloader reads them from the ESP (`EFI/morion/services/`) at boot** and passes them to the kernel via the `BootInfo` module table, after which the kernel loads them each into their fixed domain (E2b: no longer a single flat binary dispatched by domain id; E3b: services are no longer embedded into the kernel image). UI assets are organized by purpose: boot-time resources under `boot/loader/resources/`, system-wide resources under `resources/system/`.

```
.
├── .github/
│   ├── workflows/            # GitHub Actions (auto-sync to Gitee)
│   ├── ISSUE_TEMPLATE/       # Issue templates
│   └── PULL_REQUEST_TEMPLATE.md
├── boot/                     # UEFI bootloader (morion-boot)
│   ├── asm/
│   │   └── boot_stub.asm     #   Boot entry assembly stub
│   ├── loader/               # Boot-time resources & config
│   │   ├── entries/          #   Boot entries (.conf)
│   │   │   └── morion.conf
│   │   ├── resources/        #   Bootloader theme assets (BMP/PNG)
│   │   │   ├── animation/    #     Loading animation frames
│   │   │   ├── background/   #     Backgrounds (dark/light/default/mask)
│   │   │   ├── icons/        #     Category icons (dialog/power/security/system/ui)
│   │   │   ├── logo/         #     Logo variants (horizontal/monochrome/square/system)
│   │   │   ├── progress/     #     Progress bar (bar_bg/bar_fill)
│   │   │   └── splash/       #     Splash screen (background/logo)
│   │   ├── kernel_placeholder.bin
│   │   ├── loader.conf       #   Bootloader config
│   │   └── theme.toml        #   Acrylic theme config
│   └── src/
│       ├── boot/             #   Boot flow (loader/menu/kexec)
│       ├── config/           #   Config parsing (entries/theme)
│       ├── gfx/              #   Graphics rendering (framebuffer/font/renderer/animation)
│       ├── security/         #   Security (hash/secure_boot/tpm)
│       ├── lib.rs
│       └── main.rs
├── kernel/                   # Microkernel (morion-kernel, minimal TCB)
│   └── src/
│       ├── arch/             #   x86_64 (gdt/idt/pic/pit/keyboard/pci)
│       ├── memory/           #   Memory management (paging/frame_allocator)
│       ├── scheduler/        #   Scheduler (context switching)
│       ├── video/            #   Kernel text console (framebuffer/font/logo/bg/unicode) — boot & panic output
│       ├── bootinfo.rs       #   Boot info (memory map + GOP framebuffer)
│       ├── cap.rs            #   Capability system (slots + handle table)
│       ├── domain.rs         #   Protection domains (processes, per-domain page tables)
│       ├── elf.rs            #   ELF64 parsing & validation (trust boundary for executable loading)
│       ├── exec.rs           #   Load ELF at runtime → create domain → map → start task
│       ├── ipc.rs            #   Inter-process communication
│       ├── irq.rs            #   Interrupt routing (interrupts as IPC)
│       ├── device.rs         #   Generic device grant (BAR / DMA / MSI-X → DeviceGrant)
│       ├── pager.rs          #   User-space pager interface
│       ├── syscall.rs        #   Syscall entry and ID table
│       ├── lib.rs
│       └── main.rs
├── user/                     # User space: runtime + driver common libs + services
│   ├── libmorion/            #   Runtime (crate `morion`): syscall / print / libvfs / libgfx / entry boilerplate
│   ├── libdevice/            #   Driver common library (crate `libdevice`, D2/D2b): device grant / MMIO / MSI-X / virtio transport + vring
│   ├── hello/                #   Demo: a **standalone ELF program** (loaded at runtime by SYS_SPAWN_ELF)
│   └── srv/                  #   System services (crate `morion-srv`): one [[bin]] per service → one standalone ELF
│       └── src/
│           ├── common.rs     #     Wire protocol / block client / name helpers shared by services
│           ├── block_srv.rs  #     domain 5  block device driver service
│           ├── fat32_srv.rs  #     domain 6  FAT32 filesystem service
│           ├── app.rs        #     domain 7  selftest program
│           ├── shell.rs      #     domain 8  command line
│           ├── mount_srv.rs  #     domain 9  mount service
│           ├── tmpfs_srv.rs  #     domain 10 in-memory filesystem
│           ├── mfs_srv.rs    #     domain 11 MorionFS
│           ├── ext2_srv.rs   #     domain 12 ext2 read-only
│           ├── exfat_srv.rs  #     domain 13 exFAT read-write
│           ├── init.rs       #     domain 14 supervisor (watches service domains, restarts in place from the memory image)
│           ├── gfx_srv.rs    #     domain 15 graphics service (holds the framebuffer, user-space rendering: primitives + text terminal)
│           ├── net_srv.rs    #     domain 16 network driver (virtio-net; N0–N3: MSI-X interrupts + ARP selftest)
│           ├── virtio_blk_srv.rs  # domain 17 virtio-blk block driver (D3: generic grant, read-signature/write-readback selftest)
│           ├── gfx/          #     graphics-service internals: framebuffer view + fonts (font/glyphs/cjk.bin) + terminal
│           ├── sender.rs / receiver.rs / pager.rs / echo.rs / kbd.rs  # domains 0..4 demos & keyboard
│           └── bin/          #     18 entry points (each writes morion_main → its module's run())
├── kernel_test/              # Early boot-integration test kernel (temporarily kept)
│   └── src/main.rs
├── resources/
│   └── system/               # System-wide resources
│       ├── device/           #   Device icons (.ico)
│       ├── file/             #   File type icons (.ico)
│       ├── github/           #   GitHub cover (.png)
│       ├── icons/            #   General UI icons (.ico)
│       ├── logo/             #   System logo (.ico/.svg/.png)
│       ├── service/          #   Service icons (.ico)
│       └── terminal/         #   Terminal backgrounds (.raw)
├── docs/
│   ├── architecture.md       #   Technical architecture design
│   ├── app-dev-guide.md      #   Application development guide (incl. AI-callable capability spec)
│   ├── dev-reference.md      #   Kernel & interface quick reference
│   ├── commands.md           #   Build / run / verify commands
│   ├── shell-reference.md    #   Shell usage reference
│   └── roadmap-fs.md         #   Filesystem roadmap
├── Cargo.toml                # Rust workspace (boot/kernel/user/kernel_test)
├── Cargo.lock
├── Makefile                  # Build system (make iso/run/run-nvme/debug/check/clippy)
├── flake.nix                 # Nix build integration
├── rust-toolchain.toml       # Rust nightly toolchain
├── linker.ld                 # Kernel linker script
├── .gitattributes
├── .gitignore
├── CONTRIBUTING.md
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

> Detailed filesystem roadmap: [docs/roadmap-fs.md](./docs/roadmap-fs.md); graphics subsystem roadmap: [docs/roadmap-gfx.md](./docs/roadmap-gfx.md). Checked items are **verified running under QEMU**.

### Phase 1 — Microkernel core (basically complete)

- [x] Protection domain (process) creation and address-space isolation
- [x] Sync/async IPC (`send` / `recv` / `call` / `reply`; 96-byte payload + shared memory for bulk data)
- [x] Task scheduling and context switching
- [x] Address-space map/unmap + user-space pager (demand paging)
- [x] Interrupt routing ("interrupts as IPC") + MMIO / I/O port grants
- [x] Capability system (slots + handles: issue / check / revoke)
- [x] **Capability transfer over IPC** (handle transfer `SYS_HANDLE_SEND`: move an opened object **into** the target domain, move semantics; capability delegation `SYS_CAP_SEND`: **copy** a held capability, no amplification; both require `SendTo(to)`). Domains 0/1/3 cover 8 positive/negative selftest cases at boot

### Phase 2 — Foundation services (in progress)

- [x] PCI enumeration + user-space NVMe driver (block device service, with IDE PIO fallback)
- [x] MBR/GPT **partition parsing + volume layer** (inside block_srv, `dev` = volume number; FS type probed by per-volume leading signature)
- [x] **Multi-volume mount**: request-tag high 32 bits carry the volume code, one open binds one volume; each FS service auto-reports its extra volumes (mounted at `/usb<vol>`); fat32 cluster buffer 2 → 16 pages (**64 KiB** cluster ceiling), ext2 block-group limit 16 → 4096
- [x] Filesystems: FAT32 (incl. VFAT long names) / tmpfs / original MorionFS (COW + snapshots)
- [x] ext2 **read-only** compatibility (mounts existing Linux partitions)
- [x] exFAT compatibility (`exfat_srv` domain 13, mounted at `/usb`): **read-only** + **read-write** (`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`) + **large volumes** (multi-page DMA, single command ≤ 128 KiB)
- [x] libvfs + mount layer: multiple filesystems assembled into a single `/` root, runtime mounting supported
- [x] Shell (builtins + cwd-relative paths) and user-space keyboard driver
- [x] **MorionFS v2 format finalized + space reclaim / GC** (free bitmap + mark & sweep)
- [x] **MorionFS large files** (`MFS3`: indirect blocks, breaking the old ≈4 MiB limit)
- [x] **MorionFS directories & long names** (`MFS4`: variable-length dirents + extension blocks, names ≤255 bytes)
- [x] **MorionFS node metadata** (`MFS5`: timestamps / permissions / owner / link count, `rename` / `truncate` / `chmod`)
- [x] **MorionFS inode-number indirection + hard links** (`MFS6`)
- [x] **MorionFS symlinks** (`MFSL`: path resolution with following + loop protection, `readlink` / `lstat` / `ln -s`)
- [x] **MorionFS per-volume-geometry formatting** (**M7**)
- [x] **MorionFS explicit format + multi-volume** (**M8 / S2**: `mkfs.mfs <vol>`, guard accepts only blank or MFS volumes)
- [x] **MorionFS capacity scale-up (bitmap externalized)** (**S3a**: capacity ceiling ≈119 MiB → **≈127.25 GiB**)
- [x] **MorionFS single file beyond 4 GiB** (**S3b**: VFS `offset`/`size` u64 end to end + third-level indirect block `MFI3`)
- [x] **MorionFS primary-volume switching** (**S2**: superblock stores the primary-volume serial; the highest serial wins)
- [x] **MorionFS repoint primary volume by marker only** (**S2**: `mfs.primary <vol>` without touching data)
- [x] **`df` space usage** (MorionFS reports total / used / free)
- [x] **Volume management closeout: write partition tables** (**S2**: `part.create/del/wipe/reload`, GPT + MBR, verified against host `sgdisk -v`)
- [x] **NVMe interrupts (MSI/MSI-X)** (**S3**: LAPIC minimal support + PCI capability walk + MSI vector segment `0x50..0x5F`)
- [x] **Blocking interrupt wait (wait primitive)** (**S4**: timeout-bounded blocking + `SYS_IRQ_WAIT`)
- [x] **Multi-vector + `wait_any`** (**S5**: per-domain wait key + vector mask, admin + 2 I/O queues each with its own vector)
- [x] **Terminal CJK rendering (CJK / full-width / mixed-width)** (kernel side later removed in G3c, font moved to user-space `gfx_srv`)
- [x] **Executable loading (ELF + runtime spawn)** (**E1**: ELF64 loader + `SYS_SPAWN_ELF`; demo `user/hello` is a standalone ELF)
- [x] **User-space runtime libmorion + `run` command** (**E2a**)
- [x] **Services split into standalone programs + domain destroy / frame reclaim** (**E2b**: 14 services, one ELF each; `SYS_DOMAIN_DESTROY/COUNT/FRAME_FREE`)
- [x] **Service lifecycle closeout** (**E3a/E3b/E3c**: user-page W^X; services moved out of the kernel image; `init` supervisor + in-place restart from the memory image, no disk dependency)
- [x] **Graphics G1 (framebuffer moved to user space)**
- [x] **Graphics G2 (drawing primitives + shared surfaces + `libmorion::gfx`)**
- [x] **Graphics G3a (in-service terminal + text rendering moved out)**
- [x] **Graphics G3b (shell output on screen)**
- [x] **Graphics G3c (kernel drops the CJK font, −276 KB)**
- [x] **Graphics G4 (input moved out of the kernel)**
- [x] **Graphics G6 (gfx_srv self-healing: supervised restart + client session rebuild)**
- [ ] **More filesystem compatibility** (ext4 write, UDF, etc.)
- [ ] Network stack

### Phase 3 — Performance enclaves (in progress)

- [x] **IOMMU passthrough** (**E1a** ACPI DMAR probe / **E1b** root table + context table + identity second-level page tables and `GCMD.TE` / **E1c** the target device window narrowed to `[0, 3 GiB)`, out-of-window device DMA refused by the IOMMU with evidence — all done, see [docs/roadmap-driver.md](./docs/roadmap-driver.md))
- [ ] **LibDevice passthrough form & enclave manager** (E2/E3 pending; the common library `user/libdevice` is already in place)

### Phase 4 — Networking & security (in progress)

- [x] **NIC driver (virtio-net)** (`net_srv` domain 16 + generic device grant, see [docs/roadmap-driver.md](./docs/roadmap-driver.md) N0–N3: virtio-modern bring-up + MSI-X interrupt RX + ARP end-to-end selftest)
- [x] **Second real driver (virtio-blk)** (`virtio_blk_srv` domain 17, still via generic device grant, no device-specific kernel logic: D3 — read signature / write-readback selftest)
- [x] **Runtime device grant (D1b)** (`SYS_DEVICE_INFO` / `SYS_DEVICE_GRANT` + `Mmio` capability gate; NVMe / `net_srv` / `virtio_blk_srv` obtain a `DeviceGrant` via runtime syscall, behavior unchanged)
- [ ] TCP/IP stack, capability auditing, policy engine

### Phase 5 — GUI & ecosystem (not started)

- [ ] Desktop environment, package manager, virtualization

### Cross-cutting — system-AI capability interface (spec defined, implementation not started)

- [x] Design spec: how apps annotate / describe functions as **AI-callable capabilities** (see [docs/app-dev-guide.md](./docs/app-dev-guide.md) §9)
- [ ] Capability registration & discovery service
- [ ] AI call gateway (capability checks / audit log / timeouts & quotas)

---

## License

This project is licensed under the [MIT License](./LICENSE).

---

## Contact

- Project Homepage: [github.com/sheng-fs/morion-os](https://github.com/sheng-fs/morion-os)
- Email: 3555679134@qq.com

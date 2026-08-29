# Morion OS 开发速查手册

> 用途：集中记录各模块的 API、常量、配置与文件位置，作为后续开发/复盘的快速索引。
> 本文档与代码同步维护；若某处签名/常量发生变化，请同步更新本文件。

## 1. 项目结构

| 路径 | 说明 |
| --- | --- |
| `boot/` | UEFI 引导器 (crate: `morion-boot`)，加载内核 ELF 并跳转 |
| `kernel/` | 微内核 (crate: `morion-kernel`)，`x86_64-unknown-none` |
| `user/` | 用户态测试程序 (crate: `morion-user`)，编译为扁平二进制由内核加载 |
| `kernel_test/` | 早期引导测试用的小内核 (已弃用，保留) |
| `docs/architecture.md` | 技术架构文档 |
| `docs/dev-reference.md` | 本速查手册 |
| `resources/system/` | 全局系统资源 (图标/Logo) |
| `Makefile` | 构建入口 |

## 2. 启动流程

```
UEFI 固件
  → boot/src/main.rs (efi_main)
      读取 loader.conf / entries，选中 Morion 内核
      加载 kernel ELF 到 0x100000
      ExitBootServices()，保留 UEFI 内存图 + GOP 帧缓冲
      写入 BootInfo @ 0x7000
      跳转到内核入口 (_start)
  → kernel/src/main.rs (_start)
      bootinfo::get() 校验 BootInfo
      依次初始化 video / gdt / idt / 内存 / 中断 / syscall / 调度器
      scheduler::run() 切到第一个任务
```

## 3. 关键地址与常量

### 引导 / BootInfo（[kernel/src/bootinfo.rs](../../kernel/src/bootinfo.rs)）

| 名称 | 值 | 说明 |
| --- | --- | --- |
| `BOOT_INFO_ADDR` | `0x7000` | BootInfo 物理地址 |
| `BOOT_MAGIC` | `0x4D4F5249` | "MORI" 魔数 |
| 内核加载地址 | `0x100000` | linker.ld `ENTRY(_start)` |

`BootInfo` 字段：`magic, version, fb_addr, fb_width, fb_height, fb_stride, fb_bpp, mmap_addr, mmap_entry_count, mmap_entry_size`。

### 分页 / 地址空间（[kernel/src/memory/paging.rs](../../kernel/src/memory/paging.rs)）

| 名称 | 值 | 说明 |
| --- | --- | --- |
| `PHYS_OFFSET` | `0xFFFF_8000_0000_0000` | 物理内存 offset 映射（P4[256]） |
| `USER_SPACE_BASE` | `0x0000_0080_0000_0000` | 用户空间基址（P4[1]） |
| `HEAP_START` | `0x4444_4444_0000` | 内核堆起始虚拟地址 |
| `HEAP_SIZE` | `256 * 1024` | 内核堆 256 KiB |
| `MANAGED_MEMORY` | `4 GiB` | 管理的物理内存上限 |

## 4. GDT 选择子（[kernel/src/arch/gdt.rs](../../kernel/src/arch/gdt.rs)）

| 名称 | 值 | 说明 |
| --- | --- | --- |
| `KERNEL_CODE_SEL` | `0x08` | 内核代码段 |
| `KERNEL_DATA_SEL` | `0x10` | 内核数据段 |
| `USER_DATA_SEL` | `0x18` | 用户数据段 (index 3) |
| `USER_CODE_SEL` | `0x20` | 用户代码段 (index 4) |
| `USER_DATA_SEL_RPL3` | `0x1B` | 用户数据段 RPL3 |
| `USER_CODE_SEL_RPL3` | `0x23` | 用户代码段 RPL3 |
| `DOUBLE_FAULT_IST_INDEX` | `0` | 双重异常 IST 索引 |

## 5. 系统调用 ABI（[kernel/src/syscall.rs](../../kernel/src/syscall.rs)）

编号在 `rax`，参数在 `rdi/rsi/rdx`，返回值在 `rax`。

| 编号 | 名称 | 参数 | 说明 |
| --- | --- | --- | --- |
| 0 | `SYS_YIELD` | — | 主动让出 CPU |
| 1 | `SYS_SLEEP` | `rdi=ms` | 睡眠毫秒 |
| 2 | `SYS_SEND` | `rdi=to, rsi=tag` | 发送 IPC 消息；需 `Capability::SendTo(to)`，返回 1 成功 / 0 失败 |
| 3 | `SYS_RECV` | — | 接收 IPC 消息（阻塞），返回 `tag` |
| 4 | `SYS_PUTS` | `rdi=ptr, rsi=len` | 打印用户字符串 |
| 5 | `SYS_EXIT` | — | 终止当前用户任务（标记 `Terminated`） |
| 6 | `SYS_ALLOC_PAGE` | `rdi=vaddr` | 分配一物理帧映射到本域 `vaddr`，返回 1 成功 / 0 失败 |
| 7 | `SYS_SHARE_PAGE` | `rdi=vaddr, rsi=to` | 把本域 `vaddr` 的页映射进 `to` 域同地址；需 `Capability::MapInto(to)`，返回 1/0 |

### MSR 配置（`syscall::init()`）

| MSR | 配置 | 说明 |
| --- | --- | --- |
| `EFER` | 置位 `SYSTEM_CALL_EXTENSIONS` | 启用 syscall/sysret |
| `STAR` | sysret CS=4 / SS=3 (RPL3)，syscall CS=1 / SS=2 (RPL0) | 段基址 |
| `LSTAR` | `syscall_entry` | syscall 入口 |
| `SFMASK` | `INTERRUPT_FLAG` | 进入时清 IF |

### Ring3 切入与上下文（`syscall.rs`）

- `switch_to_user(entry: u64, stack_top: u64, arg: u64) -> !`：构造 iret 帧首次切入 Ring 3；`arg` 经 `rdi` 传入用户 `_start(domain_id)`（传递所属域 id）。
- `syscall_entry` 用 `r10` 暂存用户 `rsp` 并保留 `rbx`（`r10` 为 caller-saved 且不在 syscall ABI 中；`rbx` 为 callee-saved，用户态跨 syscall 复用）。
- 用户态 syscall 封装（[user/src/syscall.rs](../../user/src/syscall.rs)）声明 `rcx/r11/r8/r9/r10` clobber，且参数寄存器 `rdi/rsi/rdx` 用 `inout(..) => _`（内核 `syscall_entry` 会改写它们，仅 `in` 会让编译器误以为跨 syscall 不变）。

## 6. 模块 API 索引

### 视频（[kernel/src/video/mod.rs](../../kernel/src/video/mod.rs)）

- `init(info: &BootInfo)` / `ready() -> bool`
- `print(s: &str)` / `println(s: &str)`
- `print_hex(v: u64)` / `print_u64(v: u64)`
- `clear(color: u32)` / `set_cursor(x: u32, y: u32)`
- `width() -> u32` / `height() -> u32`

### 物理帧分配（[kernel/src/memory/frame_allocator.rs](../../kernel/src/memory/frame_allocator.rs)）

- `init(info: &BootInfo)` / `print_stats()`
- `allocate_frame() -> Option<u64>`（返回物理地址）
- `free_frame(addr: u64)`
- `total_frames() / free_frames() / total_memory_bytes() / free_memory_bytes()`
- `FRAME_SIZE = 4096`

### 分页（[kernel/src/memory/paging.rs](../../kernel/src/memory/paging.rs)）

- `init()`
- `map_user_page(domain_id: u64, vaddr: u64, paddr: u64)`（USER 权限映射）
- `resolve_user_page(domain_id: u64, vaddr: u64) -> Option<u64>`（遍历页表把 vaddr 反查为物理地址）
- `heap_start() / heap_size()`

### 域（[kernel/src/domain.rs](../../kernel/src/domain.rs)）

- `create() -> u64`（返回域 id）
- `pml4_of(id: u64) -> u64`（返回该域 PML4 物理地址）

### 调度器（[kernel/src/scheduler/mod.rs](../../kernel/src/scheduler/mod.rs)）

- `init()`
- `spawn(entry: extern "C" fn(), domain: u64)`（内核任务）
- `spawn_user(entry: u64, user_stack: u64, domain: u64)`（Ring 3 用户任务）
- `run() -> !`
- `tick()` / `yield_now()` / `sleep(ms: u64)`
- `block_current(on_domain: u64)` / `wake_one(domain: u64)`
- `current_domain() -> u64`
- `exit_current() -> !`（`SYS_EXIT` 调用的任务退出入口）

任务表常量：`MAX_TASKS = 8`，内核栈 `STACK_SIZE = 4096 * 8`（32 KiB）。

### IPC（[kernel/src/ipc.rs](../../kernel/src/ipc.rs)）

- `init(domain_count: usize)`
- `send(to: u64, tag: u64, payload: &[u8]) -> bool`（非阻塞）
- `receive() -> Message`（阻塞）
- `PAYLOAD_LEN = 32`，邮箱容量 `MAILBOX_CAP = 16`
- `Message { from, to, tag, payload }`

### 能力系统（[kernel/src/cap.rs](../../kernel/src/cap.rs)）

- `init(domain_count: usize)`
- `has(domain: u64, cap: Capability) -> bool`
- `grant(domain: u64, cap: Capability) -> bool`
- `revoke(domain: u64, cap: Capability) -> bool`
- `grant` / `revoke` 保存并恢复中断使能状态，避免 boot 期（IF=0）被提前开中断。
- `Capability::SendTo(u64)` / `Capability::MapInto(u64)`，每域 `CAP_SLOTS = 16`

### 架构（[kernel/src/arch/](../../kernel/src/arch/)）

- `gdt::init()` / `gdt::set_rsp0(stack_top: u64)`
- `idt::init()`
- `pic::init()` / `pic::send_eoi()`
- `pit::init()`（100 Hz 定时器）
- `keyboard::read_scancode()`

## 7. 构建 / 测试命令（Makefile）

| 命令 | 说明 |
| --- | --- |
| `make kernel` | 仅构建微内核 |
| `make user` | 仅构建用户态程序 → `build/user/user.bin` |
| `make boot` | 仅构建引导器 |
| `make iso` | 构建完整 ISO（`build/morion-os.iso`） |
| `make run` | QEMU 运行（KVM） |
| `make run-nokvm` | QEMU 运行（无 KVM） |
| `make debug` | QEMU + GDB（`-s -S`） |
| `make clean` / `check` / `clippy` | 清理 / 检查 / 静态检查 |

内核目标：`x86_64-unknown-none`；引导器目标：`x86_64-unknown-uefi`。

### 用户态程序构建（[user/](../../user/)）

- 自定义 target：`user/x86_64-morion-user.json`（`code-model=large` + `rustc-abi=softfloat` + `relocation-model=static`），解决用户基址 `0x8000_0000_0000` 超出 32 位重定位范围的问题。
- 链接脚本：`user/linker.ld`，`ENTRY(_start)`，链接到 `0x8000000000`，`_start` 置于镜像最前端。
- 入口 `_start(domain_id: u64)` 接收内核经 `rdi` 传入的域 id，据此分流角色（当前用于 sender/receiver 演示）。
- 构建链：`cargo build --target user/x86_64-morion-user.json ... -Z json-target-spec` → `objcopy -O binary` → `build/user/user.bin`。
- 内核经 `include_bytes!("../../build/user/user.bin")` 在编译期嵌入，运行期按页映射到 `USER_SPACE_BASE` 加载。

## 8. 工程约定

- 引导 Logo 必须为 32 位 BGRA BMP（`BITMAPINFOHEADER + BI_RGB`，自底向上像素序，4 字节行对齐）。
- `boot/loader/resources/` 下图片需同时保留 PNG 与转换后的 BMP。
- 系统主 Logo 位于 `resources/system/logo/`。
- 不要删除未确认无用的资源或系统 Logo；资源路径变更时必须同步更新引导配置。
- 硬件不支持鼠标输入，相关资源应删除。

## 9. 阶段进度

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| 1 | CPU 初始化（GDT/TSS/IDT + video + bootinfo） | ✅ |
| 2 | 物理内存管理（位图帧分配器） | ✅ |
| 3 | 虚拟内存 + 内核堆（4 级页表） | ✅ |
| 4 | 硬件中断（PIC + PIT + 键盘） | ✅ |
| 5 | 任务 + 上下文切换 + 抢占调度 | ✅ |
| 6 | 进程/域抽象（独立 PML4） | ✅ |
| 7 | IPC（消息邮箱 + 阻塞唤醒） | ✅ |
| 8 | 能力系统（最小权限） | ✅ |
| 9 | 用户态运行模型（Ring 3）+ 系统调用 ABI | ✅ |
| 10 | libuser + 可加载用户程序（`user/` crate + `SYS_EXIT`） | ✅ |
| 11 | IPC + 能力系统整合（多域授权 + 用户态 sender/receiver 演示） | ✅ |
| 12 | 跨域共享内存（`MapInto` 能力 + `SYS_ALLOC_PAGE`/`SYS_SHARE_PAGE`） | ✅ |

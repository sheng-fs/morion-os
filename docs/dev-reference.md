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
| `HEAP_SIZE` | `1 MiB` | 内核堆 1 MiB |
| `MANAGED_MEMORY` | `4 GiB` | 管理的物理内存上限 |

> **启动页表放在 `.bss`**（`BOOT_PML4` / `BOOT_PDPT` / `BOOT_PDS`，共 24 KiB，4 KiB 对齐），
> 不再向帧分配器申请。历史上启动页表取自「镜像尾部相邻帧」，一旦镜像变大使该帧与内核栈顶或仍在
> 使用的引导器页表重合，就会在 `paging::init` 处 triple fault（且随镜像大小变化时有时无）。

> **ELF 入口必须是纯汇编桩**：`_start` 由 `global_asm!` 定义（`lea rsp, [rip + _stack_end]` + `cli` +
> `jmp kernel_main`），而不是普通 Rust 函数。若把「设置 rsp」写进 `extern "C" fn _start`，LLVM 会在
> 函数入口处先按**引导器**的 rsp 分配栈帧，随后该 asm 把 rsp 重置到 `_stack_end`，整个栈帧就被平移
> 到 `[_stack_end, _stack_end + frame_size)` —— 恰好落在镜像之外、帧分配器最先交出的帧（内核堆第 0 页）
> 上，随机破坏堆的链表元数据，表现为 `alloc` 失败或 `Bad free`（且随代码体积变化时有时无）。

### 用户空间固定布局（内核与用户程序约定，见 [kernel/src/main.rs](../../kernel/src/main.rs) / [user/src/main.rs](../../user/src/main.rs)）

| 区域 | 地址 | 说明 |
| --- | --- | --- |
| 程序镜像 | `USER_BASE` 起 | 扁平二进制，**随代码增长**（当前约 28 页 / 111 KiB） |
| 文件服务缓冲页 | `USER_BASE + 0x10_0000` 起 | fat32 的 BPB/目录/FAT/文件缓冲（`..+0x10_4000`）、app 的 `RESULT_BUF`/`WRITE_BUF` 与 shell 的 `SHELL_RESULT_BUF`/`SHELL_WRITE_BUF`（`..+0x10_8000`）、mfs 的 4 个块缓冲（`..+0x10_C000`）、ext2 块缓冲（`..+0x10_10000`） |
| mfs 元数据缓冲 | `USER_BASE + 0x11_0000` 起 | GC 遍历 / inode 表块缓存 / 索引块 scratch / GC 表块（`..+0x11_4000`，M5b 新增后三页） |
| exFAT 缓冲 | `USER_BASE + 0x11_4000` 起 | 集群缓冲（按簇大小最多 64 页，`..+0x15_4000`）+ 位图窗口 `+0x15_4000` + upcase 窗口 `+0x15_5000` + 单页暂存 `+0x15_6000`（M6c 起集群缓冲动态分配） |
| block_srv 私有页 | `USER_BASE + 0x16_0000` 起 | 卷扫描页 `+0x16_0000` + PRP 表页 `+0x16_1000`（M6c；均不共享给任何域） |
| 用户栈 | `USER_BASE + 0x3F_9000 .. +0x40_1000` | **8 页（32 KiB），栈顶 `+0x40_1000` 向下增长**。单页不够：VFS 请求/回复在栈上构造 `Message`（96 B payload）并层层调用，app 在最早的几次 VFS 调用就会越过一页栈底，过去靠按需分页静默补页（不可靠） |
| 固定数据区 | `USER_BASE + 0x80_0000` 起 | +0x00 共享页（sender/receiver）、+0x1_0000 NVMe 配置、+0x2_0000 MMIO、+0x3_0000 DMA（5 页） |
| 按需分页测试地址 | `USER_BASE + 0x1_0000_0000` | sender 触发的缺页演示 |

> ⚠️ 程序镜像是**全部域共用**的同一镜像，新增代码会使其变大。所有固定映射地址必须留在
> 镜像增长范围之上（当前 ≥ 1 MiB），否则会在 `load_user_program` / `sys_alloc_page` 触发
> `map_user_page: PageAlreadyMapped` 内核 panic。

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
| 3 | `SYS_RECV` | `rdi=ptr`（可空） | 接收 IPC 消息（阻塞）；`ptr` 非空时把完整 `Message` 写回用户缓冲区，返回 `tag` |
| 4 | `SYS_PUTS` | `rdi=ptr, rsi=len` | 打印用户字符串 |
| 5 | `SYS_EXIT` | — | 终止当前用户任务（标记 `Terminated`） |
| 6 | `SYS_ALLOC_PAGE` | `rdi=vaddr` | 分配一物理帧映射到本域 `vaddr`，返回 1 成功 / 0 失败 |
| 7 | `SYS_SHARE_PAGE` | `rdi=vaddr, rsi=to` | 把本域 `vaddr` 的页映射进 `to` 域同地址；需 `Capability::MapInto(to)`，返回 1/0 |
| 8 | `SYS_UNMAP` | `rdi=vaddr` | 解除本域 `vaddr` 映射并递减引用计数，归零时释放物理帧，返回 1/0 |
| 9 | `SYS_MAP_ANON` | `rdi=domain, rsi=vaddr` | 分页器：给 `domain` 的 `vaddr` 映射匿名零帧；需 `Capability::MapInto(domain)`，返回 1/0 |
| 10 | `SYS_PAGE_FAULT_REPLY` | — | 分页器：唤醒最近一次 `SYS_RECV` 到的缺页域（回复目标由内核记录），返回 1/0 |
| 12 | `SYS_CALL` | `rdi=to, rsi=tag` | 同步调用：发送请求并阻塞等回复，返回回复 `tag`；需 `Capability::SendTo(to)`，失败返回 `u64::MAX` |
| 13 | `SYS_REPLY` | `rdi=tag` | 回复当前任务最近一次 `SYS_RECV` 到的调用者（回复目标由内核在 `receive` 时记录），返回 1/0 |
| 14 | `SYS_REGISTER_IRQ` | `rdi=irq` | 注册当前域接收 `irq`；需 `Capability::Irq(irq)`，返回 1/0 |
| 15 | `SYS_SCROLL_UP` | — | 控制台历史向上滚动一屏，返回 1 |
| 16 | `SYS_SCROLL_DOWN` | — | 控制台历史向下滚动一屏，返回 1 |
| 17 | `SYS_BACKSPACE` | — | 退格：删除输入行光标前一个字符，返回 1 |
| 18 | `SYS_TERM_PUT` | `rdi=ch` | 在输入行光标处插入字符 `ch`（`ch=0x0A` 提交当前行），返回 1 |
| 19 | `SYS_TERM_LEFT` | — | 输入行光标左移，返回 1 |
| 20 | `SYS_TERM_RIGHT` | — | 输入行光标右移，返回 1 |
| 21 | `SYS_MAP_MMIO` | `rdi=bar, rsi=vaddr` | 把物理 MMIO 页（`bar`，页对齐）映射到本域 `vaddr`（非缓存）；需 `Capability::Mmio(bar)`，返回 1/0 |
| 22 | `SYS_PORT_IN8` | `rdi=port` | 从 I/O 端口读 1 字节（用户态设备驱动用） |
| 23 | `SYS_PORT_IN16` | `rdi=port` | 从 I/O 端口读 2 字节 |
| 24 | `SYS_PORT_OUT8` | `rdi=port, rsi=val` | 向 I/O 端口写 1 字节 |
| 25 | `SYS_PORT_OUT16` | `rdi=port, rsi=val` | 向 I/O 端口写 2 字节 |
| 26 | `SYS_VIRT_TO_PHYS` | `rdi=vaddr` | 本域用户虚拟地址反查物理地址（供 NVMe PRP），失败返回 0 |
| 27 | `SYS_READLINE` | `rdi=buf, rsi=max` | 阻塞读取一行控制台输入到 `buf`（最多 `max` 字节，不含换行），返回长度，失败返回 `u64::MAX`；队列空时阻塞至键盘回车 |
| 28 | `SYS_CLEAR` | — | 清屏并复位终端状态（历史 / 输入行 / 光标 / 回滚），返回 1 |
| 29 | `SYS_CAP_ISSUE` | `rdi=obj` | 「能力即句柄」：为调用方域的不透明对象 `obj` 签发句柄，返回句柄索引（0 起），槽满返回 `u64::MAX` |
| 30 | `SYS_CAP_LOOKUP` | `rdi=handle` | 校验句柄是否有效，有效返回其对象标识，被撤销 / 非法返回 `u64::MAX` |
| 31 | `SYS_CAP_DROP` | `rdi=handle` | 撤销句柄（关闭打开对象时调用），返回 1/0 |
| 32 | `SYS_HANDLE_SEND` | `rdi=to, rsi=handle` | **能力随 IPC 传递（句柄移交）**：把本域 `handle` 槽里的不透明对象**移入** `to` 域，返回 `to` 域里的新句柄索引；**移动语义**（成功后本域该句柄立即失效）。需 `Capability::SendTo(to)`；源槽空 / 目标槽满返回 `u64::MAX` 且不改变任何状态 |
| 33 | `SYS_CAP_SEND` | `rdi=to, rsi=kind, rdx=arg` | **能力随 IPC 传递（能力委派）**：把本域**持有**的能力**复制**给 `to` 域，返回 1/0。需 `Capability::SendTo(to)`，且**不允许放大**（自己没持有的能力给不出去）；`to` 已持有该项时幂等成功、不占新槽。`kind` 取 `cap::CAP_KIND_*`：`0=SendTo / 1=MapInto / 2=Irq / 3=Mmio`，`arg` 为该能力的参数（目标域 id / IRQ 号 / 页对齐 MMIO 基址） |

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
- `term_put(c)` / `term_backspace()` / `term_left()` / `term_right()`（终端编辑：光标处插入 / 删光标前 / 左右移动光标）
- `scroll_view_up()` / `scroll_view_down()`（控制台行历史回滚）
- `unsafe input_read(out: *mut u8, max: usize) -> Option<usize>`（`SYS_READLINE` 用：从输入行队列取走一行，键盘回车时由 `term_put('\n')` 入队并唤醒等待者）
- `term_put` 记录本轮**用户输入起点** `INPUT_BASE`（首个按键时锁定为当时行尾），回车只提交该起点之后的内容，
  由此支持「行内提示符」：提示符可先 `print` 到输入行，`SYS_READLINE` 只回传用户键入部分；退格/左移不会越过该起点。
- `print_logo()`（打印启动 LOGO，整体水平居中；内容见 `logo.rs`，纯 ASCII，因内核字体仅含 0x20..=0x7E）
- 背景：清屏/重绘不再填纯色，而是调用 `bg_fill_rect` / `bg_fill_all`，按 `bg::color_for_row` 的**竖直渐变**
  逐行取色填充。颜色表 `bg.rs` 由 `resources/system/terminal/终端背景_1024x768.raw` 采样得到（64 级 ≈ 256 字节），
  已压暗偏蓝以保证白色文字可读；分辨率无关，重绘开销与原先纯色填充同量级。
- 帧缓冲格式：BGRA8888，颜色 `0x00RRGGBB`（[framebuffer.rs](../../kernel/src/video/framebuffer.rs)）；实际分辨率 1280x800（q35 + virtio + OVMF）。

### 物理帧分配（[kernel/src/memory/frame_allocator.rs](../../kernel/src/memory/frame_allocator.rs)）

- `init(info: &BootInfo)` / `print_stats()`
- `allocate_frame() -> Option<u64>`（返回物理地址）
- `free_frame(addr: u64)`
- `inc_ref(addr: u64)` / `dec_ref(addr: u64) -> bool`（共享帧引用计数；`dec_ref` 归零返回 `true`）
- `total_frames() / free_frames() / total_memory_bytes() / free_memory_bytes()`
- `FRAME_SIZE = 4096`
- 初始化末尾调用 `reserve_active_page_tables()`：把当前 `CR3` 页表层级引用的物理帧标记为占用。
  这些「引导器遗留页表」在 UEFI 内存图中可能为 CONVENTIONAL，若被当作空闲帧分配并清零，会摧毁
  正在生效的地址翻译，导致启动到 `paging::init` 即 #PF → #DF → triple fault（且随镜像大小变化时有时无）。
- 内核保留上界取 `_kernel_end` **向上对齐到 64 KiB**：链接符号与镜像实际占用末尾可能有少量出入，
  留余量可确保内核栈顶所在的帧不会被当作空闲帧分配（栈顶就在镜像末尾附近，被复用为页表会立刻被栈写坏）。

### 分页（[kernel/src/memory/paging.rs](../../kernel/src/memory/paging.rs)）

- `init()`
- `map_user_page(domain_id: u64, vaddr: u64, paddr: u64)`（USER 权限映射）
- `resolve_user_page(domain_id: u64, vaddr: u64) -> Option<u64>`（遍历页表把 vaddr 反查为物理地址）
- `unmap_user_page(domain_id: u64, vaddr: u64) -> Option<u64>`（解除映射并返回原物理地址）
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
- `set_current_reply_target(target: u64)` / `current_reply_target() -> u64`（`reply` 回复目标追踪；`u64::MAX` 表示无）
- `exit_current() -> !`（`SYS_EXIT` 调用的任务退出入口）
- `INPUT_WAIT`（伪域 id `u64::MAX-1`：表示等待控制台输入行；`SYS_READLINE` 用 `block_current(INPUT_WAIT)`，`video::term_put` 回车时 `wake_one(INPUT_WAIT)`）

任务表常量：`MAX_TASKS = 16`，内核栈 `STACK_SIZE = 4096 * 8`（32 KiB）。

### IPC（[kernel/src/ipc.rs](../../kernel/src/ipc.rs)）

- `init(domain_count: usize)`
- `send(to: u64, tag: u64, payload: &[u8]) -> bool`（非阻塞）
- `deliver(from: u64, to: u64, tag: u64, payload: &[u8]) -> bool`（内核内部投递，绕过能力检查，用于缺页等异常转发）
- `receive() -> Message`（阻塞，记录回复目标供 `reply` 使用）
- `call(to: u64, tag: u64, payload: &[u8]) -> Message`（同步调用：发送请求 + 阻塞等回复）
- `reply(tag: u64, payload: &[u8]) -> bool`（回复最近一次 `receive` 到的调用者）
- `PAYLOAD_LEN = 96`（VFS 请求要把绝对路径整条装进 payload），邮箱容量 `MAILBOX_CAP = 16`
- `Message { from, to, tag, payload }`（`#[repr(C)]`，与用户态同布局）

### 能力系统（[kernel/src/cap.rs](../../kernel/src/cap.rs)）

- `init(domain_count: usize)`
- `has(domain: u64, cap: Capability) -> bool`
- `grant(domain: u64, cap: Capability) -> bool`
- `revoke(domain: u64, cap: Capability) -> bool`
- `grant` / `revoke` 保存并恢复中断使能状态，避免 boot 期（IF=0）被提前开中断。
- `Capability::SendTo(u64)` / `Capability::MapInto(u64)` / `Capability::Irq(u8)`，每域 `CAP_SLOTS = 16`
- **「能力即句柄」句柄表**：`handle_issue(domain, obj) -> u64` / `handle_lookup(domain, handle) -> Option<u64>` / `handle_drop(domain, handle) -> bool`，每域 `HANDLE_SLOTS = 32`。槽内存放**不透明**对象标识（微内核不解释其含义，libvfs 传 `(服务域 << 32) | 服务内 fd`），由 `SYS_CAP_ISSUE`/`SYS_CAP_LOOKUP`/`SYS_CAP_DROP` 暴露给用户态。
- **能力随 IPC 传递**（`SYS_HANDLE_SEND`/`SYS_CAP_SEND`）两条路径，语义刻意不同：
  - `handle_move(from, to, handle) -> u64`：**移动**。先取出源槽对象、再在目标域找空槽；目标槽满则**回滚**（对象放回原槽），故失败时不会出现「两边都没有」。移走后源域该句柄立即失效 —— 「能力是唯一凭证」，同一份能力同一时刻只属于一个域。这是 fd 传递要的语义（交出 fd 后自己不再持有）。
  - `delegate(from, to, cap) -> bool`：**复制**。`from` 必须自己持有 `cap`（**不允许放大** —— 没有的能力给不出去，这是能力模型的根）；检查与写入在同一把锁内完成，避免「检查后被抢先」。`to` 已持有该能力时幂等返回成功且不占新槽（否则重复委派会把 16 个槽位耗光）。
  - 两者都要求调用方持有 `SendTo(to)`：`delegate` 上是双层校验（外层管「能不能给对方」，内层管「东西是不是我的」）；`handle_move` 上是防 DoS（否则任何域都能把对象灌进别的域的 32 个句柄槽）。
  - `decode(kind, arg) -> Option<Capability>`：把 syscall 的两个整数还原成 `Capability`，并对 `arg` 做与该能力使用点一致的校验（`Irq` 是 u8、`Mmio` 必须页对齐），否则可以造出永远匹配不上的能力，白占对方槽位。

### 分页器（[kernel/src/pager.rs](../../kernel/src/pager.rs)）

- `init(domain_count: usize, pager_domain: u64)`（每域统一登记 `pager_domain` 为其分页器）
- `of(domain: u64) -> u64`（查询某域的分页器域 id）
- `deliver_fault(pager: u64, info: PageFaultInfo)`（把缺页信息序列化进 IPC 消息 payload，经 `ipc::deliver` 投递并 `wake_one` 分页器）
- `PageFaultInfo { fault_domain, fault_addr, error_code }`（`#[repr(C)]`，24 字节，与用户态同布局）
- `FAULT_TAG`（缺页消息 tag 标记，区分普通 IPC）

缺页流程：`page_fault_handler` 读 CR2 → `deliver_fault`（投递 IPC 消息到分页器邮箱）→ `block_current(fault_domain)`；分页器经 `SYS_RECV` 取消息、从 payload 解出 `PageFaultInfo` → `SYS_MAP_ANON` 映射零帧 → `SYS_PAGE_FAULT_REPLY` 唤醒缺页域（回复目标由 `receive` 记录）。

### IRQ 转发（[kernel/src/irq.rs](../../kernel/src/irq.rs)）

- `register(irq: u8, domain: u64)`（登记某域为 `irq` 的驱动域；调用者须先通过 `SYS_REGISTER_IRQ` 校验 `Capability::Irq(irq)`）
- `dispatch(irq: u8, data: u64)`（把中断数据作为 IPC 消息 tag 转发给注册域；从 IRQ 处理器 IF=0 调用，非阻塞、不改变中断位）
- 最多支持 16 个 IRQ（PIC master 8 + slave 8）；`HANDLERS` 为 `[Option<u64>; 16]`。

「中断即 IPC」模型：硬件 IRQ 处理器读设备数据（如键盘 scancode）→ `irq::dispatch` 投递到驱动域邮箱 → 驱动域循环 `SYS_RECV` 接收并处理，再 `send_eoi`。

### 文件服务与挂载层（用户态）

文件系统全部位于用户态，经 libvfs 统一接入（见 [user/src/vfs.rs](../../user/src/vfs.rs)）。

- 域布局（[kernel/src/main.rs](../../kernel/src/main.rs)）：`5 block_srv / 6 fat32_srv / 7 app / 8 shell / 9 mount_srv / 10 tmpfs_srv / 11 mfs_srv / 12 ext2_srv / 13 exfat_srv`（共 14 个域）。
- **libvfs 路由**：每个路径操作先经 `mount_lookup(path)` 向 `mount_srv` 查询，回复打包为 `[63:40] 卷编码 | [39:32] 服务域 | [31:0] 挂载点前缀长度`（**M1b** 起含卷编码：0 = 该服务的默认卷，否则 = 卷号 + 1）；`route()` 去掉挂载前缀得到子路径，并把**卷编码写进请求 tag 的高 32 位**（tag 正文仍是 4 字节 ASCII，服务端用 `vfs::tag_body` 剥掉高位 —— 路径类请求因此天然带上目标卷，不必给每个请求结构体加字段）。
- **对外 fd 编码**：`[63:48] 能力句柄 | [47:32] 服务域 | [31:0] 服务内 fd`。`open`/`creat` 成功后向内核申请句柄（`SYS_CAP_ISSUE`）并编进高位；`read`/`write`/`readdir` 先经 `cap_guard`（`SYS_CAP_LOOKUP` 校验句柄有效且对象标识与 fd 一致）再下发；`close` 撤销句柄（`SYS_CAP_DROP`）。句柄被撤销后该 fd 上的任何 I/O 都失败——这就是「能力即句柄」的执行点。
- **mount_srv**：维护「挂载点前缀 → 服务域 + 卷编码」表，组件边界敏感的最长前缀匹配（`/tmpfoo` 不匹配 `/tmp`）。表是**运行时可变的**：`mount_main` 启动时写入引导默认项 `/ → fat32_srv(6)`、`/tmp → tmpfs_srv(10)`、`/mfs → mfs_srv(11)`、`/ext2 → ext2_srv(12)`、`/usb → exfat_srv(13)`（卷编码 0 = 各服务的默认卷），此后任何服务都可经 `MNTA`/`MNTD` 在运行时挂载/卸载。表容量 `MOUNT_MAX = 16`（**M1b 起**由 8 提到 16：引导默认项 5 个 + 每个文件服务上报的额外卷各占一个，插一块两分区的 U 盘就会把 8 个槽用满，届时连 `MNTA` 自动分配的 `/mnt<N>` 都拿不到槽位）。`MNTA` payload 为 `MountReq { domain u64, prefix [u8; 24] }`：前缀为空则 mount_srv 自动分配最小的空闲 `/mnt<N>`，回复挂载槽位号（1 起）；`MNTD` payload 为前缀，回复 1/`u64::MAX`，根 `/` 不可卸载。**M1b 额外卷**：新增 tag `MNTV`（`MountVolReq { domain u64, vol u64 }`）—— 文件服务把它**自己那类**的额外卷上报给 mount_srv，后者自动挂到 `/usb<卷号>`（卷号 = block_srv 卷表里的 id），并把 `enc_of_vol(vol) = vol + 1` 记进该项；重复上报是幂等的（同域同卷返回既有槽位号）。启动期会打印 `mount-dbg: /usb4 domain=12 slot=6` 这类诊断行。
- **fat32_srv**：NVMe（回退 IDE PIO）块设备之上的 FAT32 服务，挂载于 `/`。支持**VFAT 长名（LFN）**：`readdir` 拼接 LFN 项（32 字节/项、逻辑逆序、校验和存于 LFN 项偏移 13）并转成 UTF-8 存入 `DirEntry.long`；`open` 先按 8.3 短名精确匹配，失败再按长名（ASCII 大小写不敏感）回退。**只读长名，不生成 LFN 项**（写入仍只写 8.3 短名）。**M1b 大簇 + 多卷**：簇缓冲由「2 页（4 KiB 簇上限）」改为**固定 16 页整簇缓冲**（`FAT32_CLU_VADDR = USER_BASE + 0x20_0000`，目录缓冲与文件缓冲都别名到它），故 `SecPerClus` 最大 128 扇区 = 64 KiB 簇可挂载（`fat_load_bpb` 在挂载期校验 `bytes_per_sector == 512 && 0 < cluster_bytes <= 64 KiB`，不符即拒绝）；服务循环从 tag 高位取卷编码 → 若与 `FAT_BPB_VOL` 不同则**重新载入该卷的 BPB** 并切 `FAT_CUR_VOL`，之后所有 `block_read/write` 都带上当前卷号 —— 即「一个 fd 绑定一个卷，切卷时重解析几何」。**簇分配游标**：`find_free_cluster` 从 `FAT_ALLOC_HINT` 起向后扫描（找到后推进、扫到表尾回绕、换卷时复位到 2），避免每次分配都从簇 2 线性扫整张 FAT —— 否则写大文件是 O(n²) 次 FAT 读。**写路径**支持 `CREAT/WRITE/MKDIR/UNLINK/RMDIR`（**无 truncate**），大文件跨簇写入由 `write_file_range` 逐簇读-改-写 + 扩展时分配新簇挂链（单文件簇链上限 `MAX_CHAIN = 256`）。FS-18 自测覆盖「写 100000 字节跨簇 → 逐簇读回 → UNLINK 释放」。
- **tmpfs_srv**：纯内存文件系统，挂载于 `/tmp`；平铺节点表（绝对路径 → 节点）+ 32 KiB 字节区，名称限定 8.3 短名并转大写。与 fat32 共存，经挂载层拼成统一目录树。
- **mfs_srv**：原创文件系统 MorionFS（**MFS8 格式**），挂载于 `/mfs`，后端为卷层认领的 MFS 卷（默认 `build/mfs.img`）。4 KiB 块 + 8 字节块头（magic + CRC32）；超级块 A/B 双副本；payload `+256` 起是保留区，其中头 8 字节为**主卷序号**（`MFS_SB_PRIMARY`，u64，0 = 非主卷 —— 见下方「主卷切换」）。**S3a 起空闲位图移出超级块**：块 2/3 是**位图头块**（magic `MFBH`：`gen u64` + `total_blocks u32` + `data_blocks u32` = bb + 各数据块 CRC32 数组），块 `4..4+bb` / `4+bb..4+2bb` 是两份**裸 4096 字节位图数据副本**（无块头），`bb = ceil(total/32768)`，`MFS_DATA_START = 4+2*bb` 之前的块一律强制占用；容量上限因此从内联位图的 30656 块（≈119 MiB）提到 `MFS_MAX_BLOCKS = 1018 × 32768 ≈ 127.25 GiB`。提交出口统一为 **`mfs_bmp_flush()`**（取代 `mfs_write_super`）：gen+1 → 重算全部数据块 CRC → 对两份副本各写**脏区间**位图数据块 + 头块 + 超级块；脏位由 `mfs_bmp_set/clear` 按 `blk/32768` 置位，故只写变动过的区间。**挂载时逐块读位图数据块算 CRC32 与头块数组比对**（要求头块 `gen/total/data_blocks` 与超级块一致；两份都不可用但 SB+itab 有效则 `mfs_rebuild_bitmap()` 全置占用后交给 `mfs_gc` 重建，**不格式化**）。**MFS6 起引入 inode 号间接层**：目录项的 4 字节子字段存 **inode 号**（不再是对象块号），号到块的映射由一棵独立的 COW 树给出 —— 索引块（`MFIX`，1022 槽 → 表块）+ 表块（`MFIT`，1022 槽 → 对象块号）；`ino 0` 为无效/空闲，**根目录恒为 `ino 1`**（根永不改名/删除，故超级块只存 `itab_root`、`ino_count`、`ino_hint`）。索引块内容在内存留一份镜像（查表不读索引块），表块走单条目缓存。**写路径统一走 `mfs_commit_object(ino, buf, magic)`**：COW 对象块 → 更新表槽（COW 表块）→ COW 索引块 → 写超级块。因为父目录条目存的是 ino 且在对象更新时**不变**，MFS5 的「沿祖先链逐级回写」被整体删除 —— **写代价与目录深度无关**。**硬链接**（`LINK`）因此成立：多个目录项指向同一 ino，改文件只动它那一个表槽，所有名字自动看到新内容；`unlink` 是「摘名字」，`nlink` 减到 0 才释放 ino。**内建快照**因此记录 `{gen, itab_root, ino_hint, alloc_next}`（24 字节/条，上限 8，**环形**：满时淘汰最旧一条再写入）—— 必须连**它自己那版 inode 表**一起记，否则回滚后 ino 会翻译到回滚后的对象上；**空间回收**为 mark & sweep：`mfs_gc` 对「当前 + 每个快照」各自用**它自己的表**把 ino 翻成块号并遍历，同时**标记索引块与全部已用表块**（元数据漏标会被回收后重新分配出去），故快照引用的历史块不会被回收（见 `MFS_GC_TAG`）；回收只在请求分派前（低水位 `total/16`）或显式调用时执行，分配失败路径不做回收（避免误判同一次 COW 中已在建、尚未挂到根上的块）。**文件节点**（`MFFL`）：`size`(u64，**MFS8 起**) / `nblocks` + **1005 个直接块指针** + 一级/二级/**三级**间接指针（间接块 magic `MFIN`/`MFI2`/**`MFI3`**，各 1022 个槽位；直接区由 1008 缩到 1005 是为 u64 `size` 与 `ind3` 腾位，`MFS_FILE_RESERVED_OFF` 仍为 4056），单文件上限 = 整卷可用块数（MFS7 起 ≈127.25 GiB；MFS8 起四段映射，块上限 ≈1.07e9，故 4 GiB 不再是天花板），小文件不产生额外 I/O；写路径对「活动间接块」做单次调用内的读-改-COW 缓存（**三级只服务 >4 GiB 文件，走无缓存链路 `mfs_ind_peek3`/`mfs_ind_link3`**）；**空洞按 0 读**（稀疏区段不会读成短读）。**目录**（`MFDI`）用 **ext2 风格变长目录项**：`block(u32)/type(u8)/name_len(u8)/rec_len(u16)/name`，4 字节对齐、按 `rec_len` 串联、删除时把空出长度并给前一条目；名字 ≤255 字节、大小写敏感按字节精确匹配（`mfs_normalize` 只做 `.`/`..`/重复 `/` 规整，不做 8.3 大写化）；节点块（payload +0 为 `ext` 指针、+8 起 40 字节元数据、+48 起为条目区）放不下时挂一个 **`MFXI` 扩展索引块**（1022 个槽位全指向扩展目录块），目录最多 = 1 + 1022 个块。**节点元数据（MFS5）**：40 字节，`mode`(u16) / `owner`(u16，创建者域 id) / `nlink`(u32) / `mtime` / `ctime` / `atime`(u64，Unix 秒 UTC)；文件节点放在 inode 尾部保留区，目录节点紧跟 `ext` 之后（`MFS_DIR_HDR` 8 → 48）。时间由用户态直接读 **CMOS RTC**（端口 0x70/0x71，经 `SYS_PORT_IN8/OUT8`）得到；`atime` 不随读更新（否则读路径退化成写路径）。`mode` **只存储与显示，不做访问判定**（其高 4 位编码**节点类型**，与 ext2 `i_mode` 的 `S_IFMT` 同构，见 `vfs::MODE_FTYPE_*`：`ls -l` 靠它显示 `d`/`l`/`-`；低 12 位才是权限位，`chmod` 只改低 12 位、类型位随节点固定；非 MFS 服务不填类型位，客户端按 `is_dir` 回退）。另有 `TRNC`（truncate：截短释放尾部块、扩展为稀疏）、`RENM`（rename：跨目录，两条路径走调用方共享页，拒绝目录移入自身子孙）、`CHMD`（chmod）、`LINK`（硬链接：同 ino 加一个名字，仅限文件与同一文件服务）、`SYML`（**软链接，M5c**：新节点类型 `MFSL` + 目录项 type 3；目标**内联**在节点 payload 里（`+0 size` = 目标串长度、`+8 起` = 目标字节，元数据仍落在文件布局的尾部保留区）—— 沿用文件布局就是为了让所有元数据函数按 `is_dir = false` 直接复用，不必再加一种类型分支；解析走 `mfs_resolve_ex(canon, follow_leaf)`，**跟随**时把链接分量就地展开成「目标 + 剩余分量」、重新规范化、整条重走（不接着展开点走，因为目标里的 `..` 可能吃掉展开点之前的目录），限深 `MFS_SYMLINK_MAX_DEPTH = 16` 防环、展开超长直接失败；`unlink`/`rmdir`/`rename` 用 `follow_leaf = false`（作用于链接自身），而**中间分量**上的链接两种模式都跟随。**绝对目标是服务命名空间内的路径**：服务只看得见自己那棵子树，客户端 `vfs::symlink_into` 会剥掉同挂载点前缀（`/mfs/a` -> `/a`），**目标不在同一挂载点则直接拒绝创建**（原样存下只会得到一条用户无从分辨的悬空链接）。配套 `RDLK`（`readlink`：不跟随解析到链接自身，把内联的目标串写回调用方共享页，客户端 `vfs::readlink_into` 按挂载前缀**加回**去，故界面看到的是用户输入的原始路径）与 `LSTA`（`lstat`：同 `stat`，但不跟随末段）。GC 里 `MFSL` 按**叶子**处理（目标内联、不引用其它块）—— 漏了这一支会让「只要卷上有软链接，整次回收就放弃」）。旧格式（`MFS1`…`MFS5`/未知 magic/版本不符）首次挂载自动重新格式化。**M6c 安全护栏**：自动格式化**仅**允许「卷类型为 `UNKNOWN`（空白待格式化）或 `MFS`」—— 若该卷已被识别为 FAT/ext2/exFAT（非 MFS），mfs_srv 直接放弃挂载并打印 `mfs: refuse to format non-MFS volume`，避免接真机盘时误格式化既有分区。**M7 按卷几何格式化**：首次格式化的总块数由 `mfs_format_total_blocks()` 给出 = `认领到的卷容量 / 8 扇区每块`，夹在 `[MFS_MIN_TOTAL_BLOCKS = 64, MFS_MAX_BLOCKS]` 之间（下限保证放得下两份超级块 + 根目录 + inode 表），卷容量未知（`MFS_VOL_SECTORS == 0`，如 IDE PIO 回退）时退回 `MFS_DEFAULT_TOTAL_BLOCKS` —— **此前无论卷多大都写死 4096 块 = 16 MiB**，整块新盘也只会格出 16 MiB。挂载时另有一条校验：超级块记的总块数若超过该卷实际容量（换镜像 / 卷号认领错 / 卷被缩小过），该副本判为不可用 → 两份都不可用就走格式化（此前只有「不超过 `MFS_MAX_BLOCKS`」这条上界，盘上记着比卷更大的尺寸时会一路读到盘外）。上界由 `MFS_MAX_BLOCKS`（MFS7 起 ≈127.25 GiB = 1018 个位图数据块 × 32768 块）给出；位图已外置到独立数据块，不再受超级块 payload 大小限制。卷表查询收敛为 `vol_find_desc(scratch, vol) -> Option<VolumeDesc>`，`vol_kind_of` / `vol_sectors` 均基于它。**S2 多卷与显式格式化**：mfs_srv 参与额外卷挂载 —— 真盘上可有多块 MFS 卷，`mount_extra_volumes(…, VOL_KIND_MFS, …)` 把非主卷挂到 `/usb<卷号>`。与别的服务不同，MFS 的**内存态只有一份**（位图**页窗口** / `MFS_ITAB_MEM` / 快照表 / 各游标对应**一个**卷），故它按请求切卷：请求 tag 高位的卷编码（fd 类请求则由 `MfsFd.vol` 决定）与 `MFS_CUR_VOL` 不同时先 `mfs_switch_vol` —— 设 `MFS_CUR_VOL`/`MFS_CUR_SECTORS` 后 `mfs_load_state()` 把新卷的超级块（含位图）载回内存。**能这样切的前提是每次改动都随即落盘**（`mfs_itab_set` → `mfs_itab_flush` → `mfs_bmp_flush`），故请求边界上盘上状态总是自洽的。`mfs_load_state` 由 `mfs_mount_or_format` 拆出，**只载入、不格式化**（切卷时若格式化，会把一块暂时读不出的盘直接抹掉，那可能是用户唯一的副本）；`mfs_mount_or_format` = 载得动就载、载不动才格式化。新增 VFS tag `MKFS`（`vfs::mfs_mkfs(vol)`，payload = 卷号，**按卷号而非路径寻址**故不经挂载路由直接发给 mfs_srv）：`mfs_mkfs_volume(vol)` 的护栏是**只接受 `VOL_KIND_MFS` 或 `VOL_KIND_UNKNOWN`**，FAT/exFAT/ext2 分区与不存在的卷号一律拒绝；格式化时临时把 `MFS_CUR_VOL`/`MFS_CUR_SECTORS` 指向目标卷（尺寸按它的真实容量算，复用 M7 的几何路径），结束后切回原卷并 `mfs_load_state()` 重建内存态，成功则把非主卷经 `MNTV` 挂到 `/usb<卷号>`。**主卷切换（S2 补齐）**：`MKFS` 成功时还把该卷标记为**主卷** —— 超级块 payload `+256` 的 `MFS_SB_PRIMARY`（u64，0 = 非主卷）记「现有所有 MFS 卷与它自身的最大序号 + 1」（`mfs_next_primary_serial`，按卷号直读超级块、与已冻结的卷表无关），并随这次格式化提交落盘。认领端由 `mfs_vol_claim` 取代通用 `vol_claim`：**序号最大且 > 0** 的 MFS 卷优先 → 否则卷表里第一个 MFS 卷（老卷/序号全为 0，保持旧行为）→ 都没有则回退约定卷号 1。序号只增、不回写别的卷，单主卷由「最大者胜出」保证，于是「最近一次显式格式化过的卷」稳定地就是**下次启动**的 `/mfs`（本次运行不换挂载点）。标记只由显式的 `mkfs.mfs` / `mfs.primary` 设置 —— 首次挂载的自动格式化**不**认领主卷。`MKFS` 的成功回复由裸 `1` 改为**从盘上回读**的序号（>0；失败仍 `u64::MAX`），回读即标记已落盘的证据（FS-24 据此判定，另断言「重复格式化序号严格变大」与「普通写提交后标记不丢」）。**只改标记入口（`MFS_SETPRIMARY_TAG` / `vfs::mfs_set_primary(vol)`）**：`mkfs.mfs` 换主卷会**擦除**目标卷的文件，故另给一条**不动数据**的路 —— `mfs_set_primary_volume(vol)` 先用 `mfs_sb_probe` 确认目标卷**确实是 MFS**（把 `mfs_primary_of_vol` 拆出 `Option<u64>` 以区分「不是 MFS」与「是 MFS 但序号为 0」；「没有格式化兜底」），再走同一个 `mfs_next_primary_serial` 取号，临时切卷 → `mfs_load_state` → 置 `MFS_PRIMARY_SERIAL` → `mfs_bmp_flush` 提交（位图无脏块，实际只写头块 + 两份超级块）→ 切回原卷并重载。shell 命令 `mfs.primary <卷号>`；新增 FS-25（序号严格大于 mkfs 基线 + 文件逐字节一致 + 非 MFS 卷被拒）。`mfs_build_super` 每次提交都会回写该字段，`mfs_load_state` 每次载入都会读回内存态 —— 少任一处，一次普通写盘就会把标记抹成 0。详情见 [roadmap-fs.md](roadmap-fs.md) 阶段 D「M7」「M8」「S3a」「S3b」与「S2 补齐」。
- **ext2_srv（只读）**：ext2 只读兼容，挂载于 `/ext2`，后端为 NVMe 第三 namespace（`build/ext2.img`，宿主 `mke2fs` 预格式化）。**不写盘、也不自动格式化**——超级块无效即挂载失败（定位是「读既有 Linux 分区」）。解析超级块（@1024，magic `0xEF53`）→ 块大小 / 每组块数与 inode 数 / inode 大小；块组描述符表缓存每组 inode 表起始块；inode `block[15]` 的直接 / 一级 / 二级间接块映射（三级不实现）；目录项 `inode/rec_len/name_len/file_type/name` 顺序遍历。只服务 `OPEN/READ/READDIR/STAT/CLOSE`，写类 tag 一律回 `u64::MAX`。名字按 ext2 语义大小写敏感，精确匹配失败后再做一次 ASCII 大小写不敏感回退。**M1b 多卷**：`Ext2Fd` 记 `vol`；服务循环按 tag 高位卷编码切 `EXT2_CUR_VOL`，与 `EXT2_GEO_VOL` 不同则**重新挂载该卷**（重跑 `ext2_mount`：超级块 → 块组描述符 → inode 表缓存）；同时 `EXT2_MAX_GROUPS` 由 16 提到 **4096**（原值只够 16 MiB 镜像，4096 组 × 8192 块 × 4 KiB 覆盖约 32 GiB 卷），故真机 Linux 分区可挂。详情见 [roadmap-fs.md](roadmap-fs.md) 阶段 C3。
- **exfat_srv**：exFAT 兼容（**M6a 只读 + M6b 读写 + M6c 大容量**），挂载于 `/usb`，后端为 NVMe 第五 namespace（`build/exfat.img`，宿主 `mkfs.exfat` 预格式化）。**不自动格式化**，签名 / boot checksum 不符即挂载失败（`exfat: mount FAILED vol=… stage=…`）。解析主引导区（sector 0，备份在 sector 12，各 12 扇区；`EXFAT   ` 签名 + `0x55AA`）→ `SectorsPerClusterShift`/`FatOffset`/`FatLength`/`ClusterHeapOffset`/`ClusterCount`/`FirstClusterOfRootDirectory` → **boot checksum**（sector 11 低 4 字节，逐字节 `sum = rot(sum) + (sum>>1) + byte`，跳过偏移 106/107/112）→ 系统项 `0x81` 分配位图（1 = 占用，位 `cluster-2`）与 `0x82` upcase 表（`exfat_scan_system_entries` 只校验位图**覆盖面** `bitmap_len*8 >= ClusterCount`，内容**按需读扇区**，不整体载入） → 根目录所在簇必须被位图标为占用（兼作位图解析校验）。FAT 链每簇一个 u32（`0` 空闲 / `>= 0xFFFFFFF8` 链尾），`NoFatChain` 置位时按 `first + i` 连续取簇。目录 `entry set` = `0x85` File + `0xC0` Stream Extension + N×`0xC1` File Name（每条 32 字节、每 `0xC1` 承载 15 个 UTF-16 码元，`0x00` = 本簇余下未使用），set checksum 校验完整性（跳过首项字节 2/3），名字 UTF-16 → UTF-8，时间戳换算为 Unix 秒。**写路径（M6b）**：`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`；位图分配/释放（**按需 512 B 扇区窗口**：定位 `cluster-2` 所在扇区，切窗时先回写脏窗）、FAT 表项读-改-写（双 FAT 时镜像）、簇链扩展（连续文件先补齐 FAT 链接转成链式）、entry set 构造含 **NameHash**（用 upcase 表：`hash = hash.rotate_right(1) + upcase(c)`，末尾再转一次）与 SetChecksum；名字 UTF-8 → UTF-16（仅 BMP）。**顺序保证无悬空引用**：创建 = 先备簇与数据、最后写目录项；删除 = 先摘目录项、再释放簇。exFAT 无稀疏文件，`TRUNCATE` 扩展会真实分配并清零。⚠️ **扩容时须把链尾簇的 `0x00` 空位填成非 0**（`0x20`）—— exFAT 要求非 0 项不得出现在 `0x00` 之后，否则宿主 `fsck.exfat` 判卷损坏。**M6c 去上限**：集群缓冲依簇大小动态分配（`exfat_bufs_init(spc_pages)`，簇 ≤ 256 KiB = 64 页，占 `+0x11_4000..0x15_3FFF`）；分配位图与 upcase 表改为**按需 512 B 扇区窗口**（不再整体载入内存）；boot checksum 改为逐扇区累加（不依赖连续两页）。由此 32 KiB/128 KiB 大簇、位图 > 4 KiB 的大容量卷（真机 U 盘）均可挂载。**M6c 边界**：只支持 512 B 扇区（`BytesPerSectorShift == 9`）、簇 ≤ 256 KiB（`SectorsPerClusterShift ≤ 9`）；`rename`/`chmod`/`link` 不支持。**M1b 多卷**：`ExfatFd` 记 `vol`；服务循环按 tag 高位卷编码切 `EXFAT_CUR_VOL`，与 `EXFAT_GEO_VOL` 不同则**重新挂载**（重跑 `exfat_mount`，并把 `EXFAT_ALLOC_HINT` 复位为 2）；集群缓冲经 `exfat_bufs_init(spc_pages)` **只补足差额页**（已分配页数记在 `EXFAT_BUFS_PAGES`，换到簇更小的卷时不必回收也不会重复 `sys_alloc_page`）。详情见 [roadmap-fs.md](roadmap-fs.md) 阶段 D「M6a / M6b / M6c」。
- **block_srv 卷层（分区解析）**：`BlockReq.op = (volume << 8) | opcode`（payload 恰好 32 字节、无空位，故把卷号并进 `op`）。opcode：`0` 读 / `1` 写 / `2` **查询卷表**。启动时扫描各 namespace：有 MBR/GPT 分区表则每个非空分区各成一个卷，否则**整个 namespace 视为一个卷**（向后兼容三张整盘镜像）；再按卷首签名探测类型（`EXFAT   ` / MFS magic `MFS0..MFS9`（低字节为版本号） / ext2 `0xEF53` / FAT `0x55AA`）。**卷容量（M7）**：初始化阶段对每个 namespace 发一次 `Identify Namespace`(CNS=0) 取 **NSZE**（返回数据偏移 0 的 u64 = 扇区数），缓存在 `NVME_NS_SECTORS`（走 Admin 队列，只在 init 发一次）。整盘卷因此能填出**真实 `sectors`** —— 此前该栏恒为 0（「整盘其余部分，未知」），MFS 首次格式化只能退回写死的默认尺寸。`sectors == 0` 仍表示**容量未知**（仅当连盘也问不出容量时），需要容量的上层必须按默认值兜底，**不能把 0 当成零长度卷**。**⚠️ VBR 与 MBR 的区分（接真机盘后补）**：本函数也用于**本身就是分区**的卷（`-drive file=/dev/sdXN`），此时扇区 0 是该分区的 VBR；真实 FAT32 的引导代码正好落在 446..509（MBR 分区表的位置），只判「type != 0 且 count != 0」会把它误读成 4 条主分区项 —— 结果是**真正的文件系统卷永远登记不上**，只登记出 4 个指向非法 LBA 的假卷（读它们直接 `LBA Out of Range`）。故主分区项额外要求 `boot ∈ {0x00, 0x80}` 且 `lba_start != 0`（合法项才可能如此，扇区 0 永远是 MBR 自己）；不满足即当作「无分区表」走整盘卷路径。`mkfs.fat` 造的小镜像该区域恰好全 0，因此模拟镜像测不出这个问题。实际 I/O 用 `(vol.nsid, vol.start_lba + lba)`。**M6c 块层多页 DMA**：单条 NVMe 命令最多 256 扇区（128 KiB）；1 页只用 PRP1、2 页 PRP2 直指第 2 页、> 2 页 PRP2 指向 block_srv 私有 **PRP 表页**（表项为第 2..N 页物理地址，逐页 `SYS_VIRT_TO_PHYS` 反查）；更大的请求由 block_srv 按 256 扇区切段（切段落在页边界，段缓冲天然页对齐）。namespace 列表由 **Identify Controller 的 `NN`（偏移 516）** 推导为 `1..=NN`（不用 Identify CNS=2：实测部分 QEMU 版本返回不完整）。IDE PIO 路径仅登记「整盘一个卷」，其容量由 **ATA IDENTIFY DEVICE**（命令 `0xEC`）现问现取：优先 LBA48（word 100-103，需 word 83 bit10 支持位），否则 LBA28（word 60-61），两者都夹在 **28 位 LBA 上限**（`0x0FFF_FFFF` 扇区 = 128 GiB）内 —— 本驱动的读写命令只发 28 位 LBA，报出更大容量会让上层往读不到的区域写；`IDENTIFY` 失败（无盘 / ABRT / 超时）才算容量未知。此前该栏恒为 0。**卷表打印（S2）**：扫描完（以及 IDE 回退路径）立刻逐卷打印 `vol: <卷号> nsid=… lba=… sectors=… kind=…`（`vol_kind_name` 给 kind 可读名，`unknown` = 无文件系统）—— 卷号由扫描顺序决定，不打出来 `mkfs.mfs <卷号>` 就只能靠猜。
- **卷「认领」与额外卷上报（M1b）**：文件服务启动时查卷表认领自己的卷号 —— fat32 → 第一个 FAT 卷；ext2 → 第一个 ext2 卷；exfat → 第一个 `EXFAT   ` 卷；mfs → **主卷序号最大的 MFS 卷**（`mfs_vol_claim`，序号 = 超级块 `MFS_SB_PRIMARY`；都没有则第一个 MFS 卷，再没有则回退约定卷号 1 —— 空白盘无 magic）。因此现有 `nvme.img`/`mfs.img`/`ext2.img` 的卷号恒为 0/1/2，行为与引入卷层前一致。认领完默认卷后调 `mount_extra_volumes(scratch, kind, primary, domain)`：遍历卷表，把**同类且非默认**的卷经 `MNTV` 上报给 mount_srv（自动挂 `/usb<卷号>`），于是「插入第二块 FAT32/ext2/exFAT 盘」无需改任何代码即可访问。**MFS 自 S2 起也参与**（真盘上可有多块 MFS 卷），但**不做自动格式化**：只挂卷层已探测为 MFS 的额外卷，空白卷必须显式 `mkfs.mfs`。⚠️ 四个服务（fat32 / ext2 / exFAT / **mfs**）都需要 `Capability::SendTo(mount_srv)` —— 缺这条授权时 `ipc::call` 被内核**静默拒绝**（返回 `u64::MAX`，不报错），额外卷会挂不上且无任何日志。
- **readdir 条目协议 `DirEntry`**（[user/src/vfs.rs](../../user/src/vfs.rs)，168 字节）：`name [u8;11]`（8.3 短名，无短名概念的文件系统也填截断等价形式供回退）+ `long_len u8` + `long [u8;128]`（长名，0 = 无长名）+ `size u32` + `is_dir u32` + `mode u16` / `owner u16` / `nlink u32` / `mtime u64`（**MFS5 起的节点元数据**，MFS6 的 `readdir` 会对每个条目先把它存的 ino 经 inode 表翻成块号再读元数据；非 MFS 服务填默认值：权限 0755/0644、属主 0、链接数 1、时间 0）。`ls -l` 因此只需一次 `readdir`。结果页只有一页，故按 `RESULT_MAX_ENTRIES`（页大小 / 条目大小 = 24）截断，避免越界写。
- **stat 协议 `Stat`**（[user/src/vfs.rs](../../user/src/vfs.rs)，40 字节）：`size u32` / `is_dir u32` / `mode u16` / `owner u16` / `nlink u32` / `mtime u64` / `ctime u64` / `atime u64`。`mode` 的高 4 位是**节点类型**（`vfs::MODE_FTYPE_*`，与 ext2 `i_mode` 的 `S_IFMT` 同构，`ls -l`/`stat` 靠它显示 `d`/`l`/`-`），低 12 位是权限位；非 MFS 服务不填类型位，客户端按 `is_dir` 回退。请求用 `PathReq { aux, buf }`：**路径写在调用方共享页**里（payload 装不下"路径 + 结果页地址"），结果也写入同一页 —— 这样 shell 与 app 各自的结果页都能用（此前 `STAT` 硬编码写 `RESULT_BUF`，只有 app 能用）。`RENM` 用 `TwoPathReq { a_len, b_len, buf }`，页内布局 `src\0dst`；`SYML` 也用 `TwoPathReq`，页内布局 `目标\0链接自身`。**`LSTA`（lstat）** 与 `STAT` 共用同一个 `PathReq` 与结果结构，唯一区别是用不跟随式解析 —— 因此悬空链接的 `lstat` 仍能返回 `mode = 0xA000`（链接）与 `size` = 目标串长度。**`RDLK`（readlink）** 同样用 `PathReq`，但结果不是 `Stat` 而是一段**目标字节串**（无 NUL、写回 `PathReq.buf`），回复值为字节数；作用在非链接上时返回 `u64::MAX`。
- 每个客户端把结果/写缓冲页用 `SYS_SHARE_PAGE` 共享给**所有**它可能访问的文件服务域：app 的 `RESULT_BUF`/`WRITE_BUF`、shell 的 `SHELL_RESULT_BUF` 均共享给域 6 / 域 10 / 域 11 / 域 12 / 域 13。
- **共享缓冲地址约定**：共享页必须位于程序镜像之外的固定虚拟地址（同地址共享，目标域自身的镜像会占住同地址）。已用区间：fat32 `+0x10_0000..0x10_4000`、app/shell 共享缓冲 `+0x10_4000..0x10_8000`、mfs 块缓冲 `+0x10_8000..0x10_C000`、ext2 块缓冲 `+0x10_C000..0x10_10000`、**mfs 的 GC 遍历 / inode 表块缓存 / 索引块 scratch / GC 表块缓冲 `+0x11_0000..0x11_4000`**（M5b 新增后三页）、**exfat 集群缓冲 `+0x11_4000..0x15_3FFF`（按簇大小最多 64 页）+ 位图窗口 `+0x15_4000` + upcase 窗口 `+0x15_5000` + 单页暂存 `+0x15_6000`**、**block_srv 自有卷扫描页 `+0x16_0000` + PRP 表页 `+0x16_1000`**（不共享给任何域）、**fat32 整簇缓冲 `+0x20_0000..+0x21_0000`（M1b：16 页 = 64 KiB，`dir_buf`/`file_buf` 都别名到它）**。**易错点**：新增块缓冲页时必须同时 `sys_alloc_page` + `sys_share_page(.., BLOCK_DOMAIN)` —— 漏了共享，block_srv 拿到的是目标域里未映射的地址，NVMe 会直接回「非法字段」而写入静默失败。**另一个易错点**：固定地址分区不可重叠 —— 同一地址对同一域重复 `sys_share_page`（或撞上别人已占的区间）会触发内核 `KERNEL PANIC: map_user_page: PageAlreadyMapped`，故新缓冲区必须从上述空闲区间里挑、并确认没有第二个域声明同址。

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
| `make run-nvme` | **文件系统验证主用**：q35 + NVMe 单控制器六 namespace（nsid1 `nvme.img` FAT32 / nsid2 `mfs.img` MorionFS / nsid3 `ext2.img` ext2 只读 / nsid4 `parts.img` MBR 分区测试盘 / nsid5 `exfat.img` exFAT 读写 / nsid6 `spare.img` 空白盘供 `mkfs.mfs` 自测） |
| `make run-ide` | IDE PIO 运行（回退验证路径） |
| `make debug` | QEMU + GDB（`-s -S`） |
| `make clean` / `check` / `clippy` | 清理 / 检查 / 静态检查 |

内核目标：`x86_64-unknown-none`；引导器目标：`x86_64-unknown-uefi`。

### 用户态程序构建（[user/](../../user/)）

- 自定义 target：`user/x86_64-morion-user.json`（`code-model=large` + `rustc-abi=softfloat` + `relocation-model=static`），解决用户基址 `0x8000_0000_0000` 超出 32 位重定位范围的问题。
- 链接脚本：`user/linker.ld`，`ENTRY(_start)`，链接到 `0x8000000000`，`_start` 置于镜像最前端。
- 入口 `_start(domain_id: u64)` 接收内核经 `rdi` 传入的域 id，据此分流角色：0=sender、1=receiver、2=pager、3=echo、4=kbd（键盘驱动）。
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
| 13 | 内存管理完善（`SYS_UNMAP` + 共享帧引用计数 `inc_ref`/`dec_ref`） | ✅ |
| 14 | 按需分页（外部页管理器模型）：缺页捕获转发 + 匿名零帧映射 + 分页器域 | ✅ |
| 15 | 同步 IPC（`call`/`reply`）：回复目标追踪 + echo 服务演示 + 匿名零帧清零 | ✅ |
| 16 | 用户态中断/驱动框架（`Irq` 能力 + `irq::dispatch` + 键盘驱动迁到用户态） | ✅ |
| 17 | 控制台阻塞读行（`SYS_READLINE` + 输入行队列 + shell 域 8 骨架） | ✅ |
| 18 | Shell 基础命令（`help/echo/ls/cat/clear` + `sys_clear`；VFS 请求携带客户端缓冲地址） | ✅ |
| 19 | Shell 路径与写命令（`cd/pwd/mkdir/rm/touch` + cwd 归一化 + 相对路径解析） | ✅ |
| 20 | 行内提示符（输入起点追踪 + `flush()`；提示符含 cwd）+ 保留活动页表帧修复 | ✅ |
| 21 | 启动 ASCII LOGO（`video::logo`，61x25，按几何编排；整块居中 + 单次重绘） | ✅ |
| 22 | 渐变终端背景（`video::bg`，由 `.raw` 采样竖直色表）+ 启动页表改入 `.bss` | ✅ |
| 23 | 挂载层（`mount_srv` 域 9：挂载点前缀 → 服务域；libvfs 按挂载表路由 + fd 编码服务域） | ✅ |
| 24 | tmpfs 内存文件系统（`tmpfs_srv` 域 10）：挂载 `/tmp`，与 fat32 共存构成统一目录树 | ✅ |
| 25 | MorionFS（`mfs_srv` 域 11）：块设备后端（NVMe nsid2）+ COW + CRC32 + 超级块 A/B + 内建快照 + 自动格式化，挂载 `/mfs`；**MFS2 起空闲位图 + mark & sweep 空间回收**（`MSGC`/`MSST`）、**MFS3 起文件一/二级间接块**（突破 ≈4 MiB）、**MFS4 起变长目录项 + 扩展目录块 + 名字 ≤255 字节**、**MFS5 起节点元数据（时间戳/权限/owner）+ `truncate`/`rename`/`chmod`**、**MFS6 起 inode 号间接层（索引块 + 表块）+ 硬链接 `LINK`**（顺带去掉沿祖先链的逐级回写，写代价与目录深度无关） | ✅ |
| 26 | 运行时挂载（`MNTA`/`MNTD` + 自动分配空闲 `/mnt<N>`）+ 能力即句柄（`SYS_CAP_ISSUE`/`LOOKUP`/`DROP`，libvfs `cap_guard`） | ✅ |
| 27 | 帧缓冲渲染性能（32 位写 + 输入行局部重绘）与终端输出批量化 | ✅ |
| 28 | VFAT 长名读取（`readdir` 拼接 LFN + 校验和 + UTF-8；`open` 按长名回退；`DirEntry.long` 协议扩展） | ✅ |
| 29 | ext2 只读兼容（`ext2_srv` 域 12：超级块 / 块组描述符 / inode 块映射 / 目录遍历，挂载 `/ext2`，宿主 `mke2fs` 预格式化 + `debugfs` 预置文件） | ✅ |
| 30 | exFAT 兼容（`exfat_srv` 域 13）：**M6a 只读** = 引导区 + boot checksum / FAT 链 / entry set + set checksum / 分配位图 / upcase 表，挂载 `/usb`，宿主 `mkfs.exfat` 预格式化；**M6b 读写** = 位图分配/释放、FAT 链扩展、entry set 增删（含 NameHash / SetChecksum 生成），`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`；结果由宿主 `fsck.exfat` 交叉校验 | ✅ |
| 31 | 大容量卷（**M6c**）：NVMe 单命令支持多页 DMA（PRP 表页；单命令 ≤ 256 扇区 = 128 KiB，更大的请求由 block_srv 切段）；exFAT 去掉 4 KiB 簇 / 4 KiB 位图 / 8 KiB upcase 三处硬上限（集群缓冲依簇大小动态分配 ≤ 256 KiB、位图与 upcase 改按需扇区窗口）；**MFS 非 MFS 卷拒绝格式化护栏**（接真机盘不误格式化） | ✅ |
| 32 | 多卷挂载（**M1b**）：请求 tag 高 32 位携带卷编码（`enc_of_vol` = 卷号 + 1，0 = 默认卷）+ `mount_lookup` 回复扩为 `[63:40] 卷编码`；一次打开绑定一个卷（`fd → vol`），服务按卷切换时**重新解析该卷几何**（fat32 重载 BPB / ext2、exFAT 重挂载）；新增 `MNTV` tag，各文件服务把自己那类的额外卷自动上报，mount_srv 挂到 `/usb<卷号>`（补 `SendTo(mount_srv)` 能力，MFS 不参与 —— 第 34 行起改为参与）；fat32 簇缓冲 2 页 → 16 页整簇（64 KiB 簇上限，`SecPerClus ≤ 128`）、ext2 块组上限 16 → 4096 | ✅ |
| 33 | MFS 按卷几何格式化（**M7**）：block_srv 补 `Identify Namespace`(CNS=0) 的 **NSZE**（整盘卷 `sectors` 不再恒为 0）；`mfs_format` 按该卷真实容量定总块数（此前写死 4096 块 = 16 MiB），挂载时校验盘上总块数不超过卷容量；`mfs-dbg` 增 `volsec=`；卷表查询收敛为 `vol_find_desc`；测试卷默认 16 → **64 MiB**，FS-21 断言 `total × 8 == 卷 sectors` | ✅ |
| 34 | MFS 显式格式化 + 多卷（**M8 / S2**）：`MSST` 同级的 `MKFS` tag（`vfs::mfs_mkfs(vol)`，按卷号寻址、不经挂载路由）+ `mfs_mkfs_volume` 护栏（**只接受 `MFS` / `UNKNOWN` 卷**，绝不吞别人的分区）；`mfs_load_state` 从 `mfs_mount_or_format` 拆出（只载入不格式化）；mfs_srv 按请求切卷（`MFS_CUR_VOL`/`MFS_CUR_SECTORS` + `MfsFd.vol`，切卷重载内存态）并参与额外卷挂载 `/usb<卷号>`（补 `SendTo(mount_srv)`）；block_srv 启动打印卷表；shell 加 `mkfs.mfs <卷号>`；新增空白测试盘（nsid 6）+ FS-22 | ✅ |
| 35 | MFS 主卷切换（**S2 补齐**）：超级块 payload `+256` 存**主卷序号**（`MFS_SB_PRIMARY`，不升 magic，老卷该处为 0 = 非主卷）；`mkfs.mfs` 置「现有最大 + 1」并随提交落盘（`mfs_next_primary_serial`）；认领改走 `mfs_vol_claim`（序号最大且 >0 者胜出 → 第一个 MFS 卷 → 回退约定卷号 1），于是「最近一次显式格式化过的卷」就是**下次启动**的 `/mfs`；`MKFS` 成功回复由 `1` 改为**盘上回读**的序号；新增 FS-24（序号递增 + 普通提交后标记不丢 + 主卷不受扰动） | ✅ |
| 36 | MFS 只改标记换主卷（**S2 补齐**）：新 tag `MFS_SETPRIMARY_TAG`（`vfs::mfs_set_primary(vol)`）+ `mfs_set_primary_volume` —— **不动数据**地给一块已有 MFS 卷换主卷（`mkfs.mfs` 会擦除）；`mfs_sb_probe` 拆出以区分「不是 MFS」与「是 MFS 但序号 0」，**无格式化兜底**、非 MFS/不存在卷号一律拒；与 mkfs 共用 `mfs_next_primary_serial`；shell 加 `mfs.primary <卷号>`（含 `help`）；新增 FS-25 | ✅ |
| 37 | IDE PIO 容量探测（**M7 遗留收口**）：IDE 回退路径的整盘卷不再恒报 `sectors = 0` —— 新增 `ide_identify`/`ide_capacity_sectors`，用 **ATA IDENTIFY DEVICE**（`0xEC`）现问容量（优先 LBA48 word 100-103，需 word 83 bit10 支持位；否则 LBA28 word 60-61），**夹在 28 位 LBA 上限**（`0x0FFF_FFFF` 扇区 = 128 GiB）内；`BLOCK_OP_LIST_VOLUMES` 改从卷表取真实值；问不出（无盘 / ABRT / 超时）才是容量未知。实测 1024 MiB 盘 → `sectors=2097152` | ✅ |

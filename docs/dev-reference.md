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
| 文件服务缓冲页 | `USER_BASE + 0x10_0000` 起 | fat32 的 BPB/目录/FAT/文件缓冲（`..+0x10_4000`）、app 的 `RESULT_BUF`/`WRITE_BUF` 与 shell 的 `SHELL_RESULT_BUF`/`SHELL_WRITE_BUF`（`..+0x10_8000`）、mfs 的 4 个块缓冲（`..+0x10_C000`） |
| 用户栈 | `USER_BASE + 0x40_0000` | 1 页，向下增长 |
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
- `PAYLOAD_LEN = 32`，邮箱容量 `MAILBOX_CAP = 16`
- `Message { from, to, tag, payload }`（`#[repr(C)]`，与用户态同布局）

### 能力系统（[kernel/src/cap.rs](../../kernel/src/cap.rs)）

- `init(domain_count: usize)`
- `has(domain: u64, cap: Capability) -> bool`
- `grant(domain: u64, cap: Capability) -> bool`
- `revoke(domain: u64, cap: Capability) -> bool`
- `grant` / `revoke` 保存并恢复中断使能状态，避免 boot 期（IF=0）被提前开中断。
- `Capability::SendTo(u64)` / `Capability::MapInto(u64)` / `Capability::Irq(u8)`，每域 `CAP_SLOTS = 16`
- **「能力即句柄」句柄表**：`handle_issue(domain, obj) -> u64` / `handle_lookup(domain, handle) -> Option<u64>` / `handle_drop(domain, handle) -> bool`，每域 `HANDLE_SLOTS = 32`。槽内存放**不透明**对象标识（微内核不解释其含义，libvfs 传 `(服务域 << 32) | 服务内 fd`），由 `SYS_CAP_ISSUE`/`SYS_CAP_LOOKUP`/`SYS_CAP_DROP` 暴露给用户态。

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

- 域布局（[kernel/src/main.rs](../../kernel/src/main.rs)）：`5 block_srv / 6 fat32_srv / 7 app / 8 shell / 9 mount_srv / 10 tmpfs_srv / 11 mfs_srv`（共 12 个域）。
- **libvfs 路由**：每个路径操作先经 `mount_lookup(path)` 向 `mount_srv` 查询，回复打包为 `(服务域 << 32) | 挂载点前缀长度`；`route()` 去掉挂载前缀得到子路径再下发目标服务。
- **对外 fd 编码**：`[63:48] 能力句柄 | [47:32] 服务域 | [31:0] 服务内 fd`。`open`/`creat` 成功后向内核申请句柄（`SYS_CAP_ISSUE`）并编进高位；`read`/`write`/`readdir` 先经 `cap_guard`（`SYS_CAP_LOOKUP` 校验句柄有效且对象标识与 fd 一致）再下发；`close` 撤销句柄（`SYS_CAP_DROP`）。句柄被撤销后该 fd 上的任何 I/O 都失败——这就是「能力即句柄」的执行点。
- **mount_srv**：维护「挂载点前缀 → 服务域」表，组件边界敏感的最长前缀匹配（`/tmpfoo` 不匹配 `/tmp`）。表是**运行时可变的**（`MOUNT_MAX = 8` 槽）：`mount_main` 启动时写入引导默认项 `/ → fat32_srv(6)`、`/tmp → tmpfs_srv(10)`、`/mfs → mfs_srv(11)`，此后任何服务都可经 `MNTA`/`MNTD` 在运行时挂载/卸载。`MNTA` payload 为 `MountReq { domain u64, prefix [u8; 24] }`：前缀为空则 mount_srv 自动分配最小的空闲 `/mnt<N>`，回复挂载槽位号（1 起）；`MNTD` payload 为前缀，回复 1/`u64::MAX`，根 `/` 不可卸载。
- **fat32_srv**：NVMe（回退 IDE PIO）块设备之上的 FAT32 服务，挂载于 `/`。
- **tmpfs_srv**：纯内存文件系统，挂载于 `/tmp`；平铺节点表（绝对路径 → 节点）+ 32 KiB 字节区，名称限定 8.3 短名并转大写。与 fat32 共存，经挂载层拼成统一目录树。
- **mfs_srv**：原创文件系统 MorionFS，挂载于 `/mfs`，后端为 NVMe 第二 namespace（独立 `build/mfs.img`）。4 KiB 块 + 8 字节块头（magic + CRC32）；超级块 A/B 双副本交替写；**只增不回收的 COW** 写时复制（叶子 → 逐级上溯父目录 → 根），因此**内建快照**（`{gen, root_block, alloc_next}`）天然可用；快照表（上限 8）随超级块持久化且为**环形**，满时淘汰最旧一条再写入（旧块仍由 COW 保留，仅丢弃快照记录，索引随之整体前移）；空白盘首次挂载自动格式化。详情见 [roadmap-fs.md](roadmap-fs.md) 阶段 C3。
- **block_srv 多设备**：`BlockReq.op = (device << 8) | opcode`（payload 恰好 32 字节、无空位，故把设备号并进 `op`）。NVMe 取 `nsid = device + 1`：nsid1 = FAT32 盘 `nvme.img`、nsid2 = MFS 盘 `mfs.img`（QEMU 单控制器双 `nvme-ns`）；IDE PIO 仅支持 `device == 0`。
- 每个客户端把结果/写缓冲页用 `SYS_SHARE_PAGE` 共享给**所有**它可能访问的文件服务域：app 的 `RESULT_BUF`/`WRITE_BUF`、shell 的 `SHELL_RESULT_BUF` 均共享给域 6 / 域 10 / 域 11。
- **共享缓冲地址约定**：共享页必须位于程序镜像之外的固定虚拟地址（同地址共享，目标域自身的镜像会占住同地址）。已用区间：fat32 `+0x10_0000..0x10_4000`、app/shell 共享缓冲 `+0x10_4000..0x10_8000`、mfs 块缓冲 `+0x10_8000..0x10_C000`。

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
| `make run-nvme` | **文件系统验证主用**：q35 + NVMe 单控制器双 namespace（nsid1 `nvme.img` FAT32 / nsid2 `mfs.img` MorionFS） |
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
| 25 | MorionFS（`mfs_srv` 域 11）：块设备后端（NVMe nsid2）+ COW + CRC32 + 超级块 A/B + 内建快照 + 自动格式化，挂载 `/mfs` | ✅ |
| 26 | 运行时挂载（`MNTA`/`MNTD` + 自动分配空闲 `/mnt<N>`）+ 能力即句柄（`SYS_CAP_ISSUE`/`LOOKUP`/`DROP`，libvfs `cap_guard`） | ✅ |
| 27 | 帧缓冲渲染性能（32 位写 + 输入行局部重绘）与终端输出批量化 | ✅ |

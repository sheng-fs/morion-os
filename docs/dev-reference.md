# Morion OS 开发速查手册

> 用途：集中记录各模块的 API、常量、配置与文件位置，作为后续开发/复盘的快速索引。
> 本文档与代码同步维护；若某处签名/常量发生变化，请同步更新本文件。

## 1. 项目结构

| 路径 | 说明 |
| --- | --- |
| `boot/` | UEFI 引导器 (crate: `morion-boot`)，加载内核 ELF 并跳转 |
| `kernel/` | 微内核 (crate: `morion-kernel`)，`x86_64-unknown-none` |
| `user/srv/` | 用户态系统服务 (crate: `morion-srv`)：16 个服务各一个 `[[bin]]` → 各一份**独立 ELF**，内核引导期逐个载入各自固定域 (E2b)。含监督者 `init` (E3c) 与图形服务 `gfx_srv` (G1) |
| `user/libmorion/` | 用户态运行库 (crate: `morion`)：syscall / 打印 / libvfs / 入口样板 |
| `user/hello/` | 可执行文件加载的演示程序 (独立 ELF，运行时经 `SYS_SPAWN_ELF` 载入) |
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

`BootInfo` 字段：`magic, version, fb_addr, fb_width, fb_height, fb_stride, fb_bpp, mmap_addr, mmap_entry_count, mmap_entry_size, svc_addr, svc_count, svc_entry_size`。

**服务模块表（E3b）**：`svc_addr` 指向一张 `ServiceModule { domain, addr, len }` 数组（引导器放在 `LOADER_DATA` 页里），内核经 `BootInfo::service_modules()` 取用 —— `svc_entry_size` 是布局校验（两个独立编译的产物，字段不一致就判为不可用）。这是 16 个引导期服务的镜像来源（不再是内核 `include_bytes!`）。

### 分页 / 地址空间（[kernel/src/memory/paging.rs](../../kernel/src/memory/paging.rs)）

| 名称 | 值 | 说明 |
| --- | --- | --- |
| `PHYS_OFFSET` | `0xFFFF_8000_0000_0000` | 物理内存 offset 映射（P4[256]） |
| `USER_SPACE_BASE` | `0x0000_0080_0000_0000` | 用户空间基址（P4[1]） |
| `USER_SPACE_END` | `USER_SPACE_BASE + 512 GiB` | 用户空间上界（**开区间**，P4[1] 之外就是内核/未映射；ELF 段与入口都要落在里面） |
| `USER_STACK_TOP` / `USER_STACK_PAGES` | `USER_BASE + 0x40_1000` / 8 | 用户栈顶与页数（32 KiB）。**唯一来源**：引导期 `exec::spawn_elf_at` 与运行时 `exec::spawn_elf` 必须给出同一布局 —— 所有程序共用一套链接地址 |
| `HEAP_START` | `0x4444_4444_0000` | 内核堆起始虚拟地址 |
| `HEAP_SIZE` | `4 MiB` | 内核堆大小（每任务 32 KiB 内核栈 × `MAX_TASKS = 32` ≈ 1 MiB，故留 4 MiB） |
| `MANAGED_MEMORY` | `4 GiB` | 管理的物理内存上限 |

> **启动页表放在 `.bss`**（`BOOT_PML4` / `BOOT_PDPT` / `BOOT_PDS`，共 24 KiB，4 KiB 对齐），
> 不再向帧分配器申请。历史上启动页表取自「镜像尾部相邻帧」，一旦镜像变大使该帧与内核栈顶或仍在
> 使用的引导器页表重合，就会在 `paging::init` 处 triple fault（且随镜像大小变化时有时无）。

> **ELF 入口必须是纯汇编桩**：`_start` 由 `global_asm!` 定义（`lea rsp, [rip + _stack_end]` + `cli` +
> `jmp kernel_main`），而不是普通 Rust 函数。若把「设置 rsp」写进 `extern "C" fn _start`，LLVM 会在
> 函数入口处先按**引导器**的 rsp 分配栈帧，随后该 asm 把 rsp 重置到 `_stack_end`，整个栈帧就被平移
> 到 `[_stack_end, _stack_end + frame_size)` —— 恰好落在镜像之外、帧分配器最先交出的帧（内核堆第 0 页）
> 上，随机破坏堆的链表元数据，表现为 `alloc` 失败或 `Bad free`（且随代码体积变化时有时无）。

### 用户空间固定布局（内核与用户程序约定，见 [kernel/src/main.rs](../../kernel/src/main.rs) / [user/srv/src/common.rs](../../user/srv/src/common.rs)）

| 区域 | 地址 | 说明 |
| --- | --- | --- |
| 程序镜像 | `USER_BASE` 起 | 每个程序自己的 ELF 镜像，**随代码增长**；硬上限是 `+0x10_0000` 之前那段 = 1 MiB |
| 文件服务缓冲页 | `USER_BASE + 0x10_0000` 起 | fat32 的 BPB/目录/FAT/文件缓冲（`..+0x10_4000`）、app 的 `RESULT_BUF`/`WRITE_BUF` 与 shell 的 `SHELL_RESULT_BUF`/`SHELL_WRITE_BUF`（`..+0x10_8000`）、mfs 的 4 个块缓冲（`..+0x10_C000`）、ext2 块缓冲（`..+0x10_10000`） |
| mfs 元数据缓冲 | `USER_BASE + 0x11_0000` 起 | GC 遍历 / inode 表块缓存 / 索引块 scratch / GC 表块（`..+0x11_4000`，M5b 新增后三页） |
| exFAT 缓冲 | `USER_BASE + 0x11_4000` 起 | 集群缓冲（按簇大小最多 64 页，`..+0x15_4000`）+ 位图窗口 `+0x15_4000` + upcase 窗口 `+0x15_5000` + 单页暂存 `+0x15_6000`（M6c 起集群缓冲动态分配） |
| block_srv 私有页 | `USER_BASE + 0x16_0000` 起 | 卷扫描页 `+0x16_0000` + PRP 表页 `+0x16_1000`（M6c；均不共享给任何域） |
| fat32 整簇缓冲 | `USER_BASE + 0x20_0000` 起 | 16 页 = 64 KiB（`FAT32_CLU_VADDR`/`FAT32_CLU_PAGES`，M1b 大簇支持），`dir_buf`/`file_buf` 都别名到它 |
| ELF 加载中转页 + 暂存区 | `USER_BASE + 0x1F_F000`（中转 1 页）/ `+0x20_0000` 起（暂存，最多 256 页 = 1 MiB） | 运行时可执行文件加载（E1/E2，`morion::exec`）：中转页要**共享给读取的文件服务**（同地址），暂存区只在本域内、**不共享**。⚠️ `+0x20_0000` 与 fat32 的整簇缓冲同址但**不同域**（一个是 fat32 自己的、一个是 shell/app 的），不冲突；也正因如此 `spawn_file` 的调用方不能是文件服务 |
| 用户栈 | `USER_BASE + 0x3F_9000 .. +0x40_1000` | **8 页（32 KiB），栈顶 `+0x40_1000` 向下增长**。单页不够：VFS 请求/回复在栈上构造 `Message`（96 B payload）并层层调用，app 在最早的几次 VFS 调用就会越过一页栈底，过去靠按需分页静默补页（不可靠） |
| 固定数据区 | `USER_BASE + 0x80_0000` 起 | +0x00 共享页（sender/receiver）、+0x1_0000 **设备授权描述页**（D1 起通用，原为 NVMe 专属配置结构）、+0x2_0000 BAR 窗口、+0x3_0000 DMA 块（NVMe 驱动自行排 7 页：admin 的 ASQ/ACQ + 两条 I/O 队列的 SQ/CQ + data 页 —— **布局由驱动决定**，内核只给一个连续 DMA 块） |
| 按需分页测试地址 | `USER_BASE + 0x1_0000_0000` | sender 触发的缺页演示 |

> ⚠️ 每个程序自己的镜像都会随代码增长。所有固定映射地址必须留在**所有**镜像的增长范围
> 之上（当前 ≥ 1 MiB），否则会在 `exec::spawn_elf_at` / `sys_alloc_page` 触发
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
| 5 | `SYS_EXIT` | — | 终止当前用户任务（标记 `Terminated`）；若它是所属域的**最后一个**任务且该域非引导域，则登记该域由内核**退出即回收**（见域小节 `request_destroy`） |
| 6 | `SYS_ALLOC_PAGE` | `rdi=vaddr` | 分配一物理帧映射到本域 `vaddr`，返回 1 成功 / 0 失败 |
| 7 | `SYS_SHARE_PAGE` | `rdi=vaddr, rsi=to` | 把本域 `vaddr` 的页映射进 `to` 域同地址；需 `Capability::MapInto(to)`，返回 1/0 |
| 8 | `SYS_UNMAP` | `rdi=vaddr` | 解除本域 `vaddr` 映射并递减引用计数，归零时释放物理帧，返回 1/0 |
| 9 | `SYS_MAP_ANON` | `rdi=domain, rsi=vaddr` | 分页器：给 `domain` 的 `vaddr` 映射匿名零帧；需 `Capability::MapInto(domain)`，返回 1/0 |
| 10 | `SYS_PAGE_FAULT_REPLY` | — | 分页器：唤醒最近一次 `SYS_RECV` 到的缺页域（回复目标由内核记录），返回 1/0 |
| 12 | `SYS_CALL` | `rdi=to, rsi=tag` | 同步调用：发送请求并阻塞等回复，返回回复 `tag`；需 `Capability::SendTo(to)`，失败返回 `u64::MAX`。**G6 起**：目标域若**无存活任务**（服务崩了/正被监督者重启）立即失败返回；等待期间目标域死掉也会在超时轮询（`CALL_POLL_MS = 200`）后失败返回 —— 不再永久挂起 |
| 13 | `SYS_REPLY` | `rdi=tag` | 回复当前任务最近一次 `SYS_RECV` 到的调用者（回复目标由内核在 `receive` 时记录），返回 1/0 |
| 14 | `SYS_REGISTER_IRQ` | `rdi=irq` | 注册当前域接收 `irq`；需 `Capability::Irq(irq)`，返回 1/0 |
| 15..20 | ~~`SYS_SCROLL_UP/DOWN`、`SYS_BACKSPACE`、`SYS_TERM_PUT`、`SYS_TERM_LEFT/RIGHT`~~ | — | **已随 G4 退役**：这六个号曾用于「内核终端输入行」（历史滚动 / 退格 / 逐键编辑 / 回车提交）。输入搬进用户态后**不再分配**，键字节改走 48 / 49 |
| 21 | `SYS_MAP_MMIO` | `rdi=bar, rsi=vaddr` | 把物理 MMIO 页（`bar`，页对齐）映射到本域 `vaddr`（非缓存）；需 `Capability::Mmio(bar)`，返回 1/0 |
| 22 | `SYS_PORT_IN8` | `rdi=port` | 从 I/O 端口读 1 字节（用户态设备驱动用） |
| 23 | `SYS_PORT_IN16` | `rdi=port` | 从 I/O 端口读 2 字节 |
| 24 | `SYS_PORT_OUT8` | `rdi=port, rsi=val` | 向 I/O 端口写 1 字节 |
| 25 | `SYS_PORT_OUT16` | `rdi=port, rsi=val` | 向 I/O 端口写 2 字节 |
| 26 | `SYS_VIRT_TO_PHYS` | `rdi=vaddr` | 本域用户虚拟地址反查物理地址（供 NVMe PRP），失败返回 0 |
| 27 | ~~`SYS_READLINE`~~ | — | **已随 G4 退役**：内核不再提供「读一行」—— 行编辑在用户态 [`morion::console::readline`](../../user/libmorion/src/console.rs)（键字节走 48 / 49） |
| 28 | `SYS_CLEAR` | — | 清屏并复位内核终端状态（历史 / 当前行），返回 1 |
| 29 | `SYS_CAP_ISSUE` | `rdi=obj` | 「能力即句柄」：为调用方域的不透明对象 `obj` 签发句柄，返回句柄索引（0 起），槽满返回 `u64::MAX` |
| 30 | `SYS_CAP_LOOKUP` | `rdi=handle` | 校验句柄是否有效，有效返回其对象标识，被撤销 / 非法返回 `u64::MAX` |
| 31 | `SYS_CAP_DROP` | `rdi=handle` | 撤销句柄（关闭打开对象时调用），返回 1/0 |
| 32 | `SYS_HANDLE_SEND` | `rdi=to, rsi=handle` | **能力随 IPC 传递（句柄移交）**：把本域 `handle` 槽里的不透明对象**移入** `to` 域，返回 `to` 域里的新句柄索引；**移动语义**（成功后本域该句柄立即失效）。需 `Capability::SendTo(to)`；源槽空 / 目标槽满返回 `u64::MAX` 且不改变任何状态 |
| 33 | `SYS_CAP_SEND` | `rdi=to, rsi=kind, rdx=arg` | **能力随 IPC 传递（能力委派）**：把本域**持有**的能力**复制**给 `to` 域，返回 1/0。需 `Capability::SendTo(to)`，且**不允许放大**（自己没持有的能力给不出去）；`to` 已持有该项时幂等成功、不占新槽。`kind` 取 `cap::CAP_KIND_*`：`0=SendTo / 1=MapInto / 2=Irq / 3=Mmio / 4=Spawn / 5=Fb`，`arg` 为该能力的参数（目标域 id / IRQ 号 / 页对齐 MMIO 基址；`Spawn`/`Fb` 无参、`arg` 忽略） |
| 34 | `SYS_IRQ_POLL` | `rdi=mask` | 非阻塞取走**掩码 `mask` 覆盖的 MSI/MSI-X 向量**中任意一个的「待处理」标志，命中返回**该向量号**，无 / 非法返回 0。位 `i` ↔ 向量 `idt::MSI_VECTOR_BASE + i`；掩码里每个位都须满足 `Capability::Irq(vector)` 且是该向量的注册者，否则整体非法。中断不投 IPC（见「IRQ 转发」），驱动用它走「中断已到」的快路径，未命中再 `SYS_IRQ_WAIT` 阻塞 |
| 35 | `SYS_MSIX_ENABLE` | — | 打开**被授权设备**的 MSI-X（置 Enable、清 Function Mask）；由该设备的驱动域调用且只成功一次，**PCI 配置空间写因此留在内核**。D1 起转调 `device::enable_msix()`（原为 `nvme::enable_msix`）。返回 1/0 |
| 36 | `SYS_IRQ_WAIT` | `rdi=mask, rsi=timeout_ms` | **阻塞等待掩码里任意一条向量**的中断（`wait_any`）：命中返回该向量号，超时返回 0（调用方据此回退轮询）。阻塞期间本域让出 CPU（不空转），由中断处理器唤醒；超时由 `tick()` 兜底。校验同 `SYS_IRQ_POLL`。syscall 入口已用 SFMASK 清 IF，故「查标志 → 登记掩码 → 阻塞」之间不会插进中断处理，不丢唤醒 |
| 37 | `SYS_SPAWN_ELF` | `rdi=ptr, rsi=len` | **加载可执行文件并启动**（E1）：把本域内存里的 ELF64 `ET_EXEC` 镜像校验后载入**新域**并起一个 Ring 3 任务，成功返回**新域 id**，失败 `u64::MAX`。需 `Capability::Spawn`。解析与映射全在核内（`elf::parse` 全量校验 + `exec::spawn_elf`），新域**零能力**、其分页器登记为调用者。**失败时会把半成品域销毁掉**（地址空间与各全局表行都不留） |
| 38 | `SYS_DOMAIN_DESTROY` | `rdi=domain` | **销毁一个域并回收它的全部资源**（E2b 地基）：地址空间（逐页按引用计数归还 + 页表帧 + PML4）、能力与句柄、邮箱、分页器登记、中断注册、任务与内核栈；域 id 槽位归还以便复用。返回 1/0。门禁是**两条一起**：`Capability::Spawn` **且** `pager::of(domain) == 调用者`（"谁加载谁负责"）—— 因此**自我销毁不可达**（拆自己正在用的页表/内核栈会当场崩） |
| 39 | `SYS_DOMAIN_COUNT` | — | 当前**存活域数**（`domain::alive_count`），自测取证用：销毁之后应回到基线 |
| 40 | `SYS_FRAME_FREE` | — | 当前**空闲物理帧数**（`frame_allocator::free_frames`），自测取证用：反复加载/销毁后不应下降 |
| 41 | `SYS_SPAWN_ELF_AT` | `rdi=域 id, rsi=ptr, rdx=len` | **在指定域里加载并启动**（E3c，监督者重启服务）：不建新域，目标域须已存在且**无存活任务**；内核先清空它的用户地址空间（`domain::reset`）、**丢弃它邮箱里未处理的请求**（`ipc::remove_domain`，G6：这些请求常引用客户端共享过来的页，而 `reset` 已把映射清掉）、摘掉已终止任务（`scheduler::reap_terminated`），再映射新镜像 → **域号不变**。需 `Capability::Spawn` |
| 42 | `SYS_DOMAIN_ALIVE` | `rdi=域 id` | 该域**是否还有存活任务**（1/0）—— 监督者巡检原语。问的是"任务在不在"而不是"域槽位在不在"（引导域的任务退出后槽位仍在）。无能力门禁 |
| 43 | `SYS_SPAWN_ELF_MODULE` | `rdi=域 id` | **用引导模块内存镜像在指定域原地重启**（E3c 后续）：与 41 共用同一套「验镜像 → 目标域无存活任务 → `domain::reset` → `reap_terminated` → 起任务」流程，区别只在镜像来源 —— 内核按域号去 `bootinfo::get().service_modules()`（E3b 交来的 `LOADER_DATA` 镜像）里取，过 `is_identity_mapped` 后映射，**不依赖磁盘**。返回域 id / `u64::MAX`。需 `Capability::Spawn` |
| 44 | `SYS_FB_INFO` | `rdi=用户缓冲指针` | **取帧缓冲几何**（G1）：把 `FbInfo { addr, width, height, stride, bpp }`（24 字节）写回用户缓冲，成功 1 / 失败 0。需 `Capability::Fb` |
| 45 | `SYS_FB_MAP` | `rdi=页对齐用户虚拟地址` | **把整块帧缓冲映射进本域**（G1）：按 4 KiB 页逐页映射（非缓存，与 `SYS_MAP_MMIO` 同口径）；若目标区间**已有映射**则整体拒绝（不半途映射，也不撞 `PageAlreadyMapped`）。成功 1 / 失败 0。需 `Capability::Fb` |
| 46 | `SYS_FB_TAKEOVER` | — | **宣告本域接管显示**（G1）：置内核 `video::FB_TAKEN_OVER`，此后内核终端跳过帧缓冲（`print` 仍写 COM1），屏幕归用户态。成功 1。需 `Capability::Fb`，幂等 |
| 47 | `SYS_CONSOLE_READY` | — | 显示**是否已交用户态**（`FB_TAKEN_OVER` 曾置位，1/0）。无能力门禁。给「要把输出镜像到屏幕控制台的客户端」用 —— 接管前镜像只会白等一次 `SYS_CALL`（G3b） |
| 48 | `SYS_KEY_PUSH` | `rdi=字节` | **推进一个按键字节**（G4）：用户态键盘域（`kbd_srv`）调用，内核只搬运、**不解释**（可打印字符 / 退格 / 回车一视同仁）。队列（64 字节环形）满则丢弃该字节，返回 1 |
| 49 | `SYS_KEY_READ` | — | **阻塞取一个按键字节**（G4）：队列空则睡眠，由 `SYS_KEY_PUSH` 唤醒（`wake_one(KEY_WAIT)`）；返回字节值。行编辑 / 回显 / 行历史全在用户态（[`morion::console`](../../user/libmorion/src/console.rs)） |
| 50 | `SYS_DEVICE_CONFIG_READ` | `rdi=offset` | **读本域被授权设备的 PCI 配置空间 dword**（N2）：只放行"读**自己那台**设备"（内核按 `domain → bus/dev/func` 绑定表校验），没有绑定设备返回 `u64::MAX`。驱动靠它**自行解析**能力链表（PCI 通用能力 / 厂商能力，如 virtio 各 BAR 区域偏移），内核因此不必懂设备协议 |

### MSR 配置（`syscall::init()`）

| MSR | 配置 | 说明 |
| --- | --- | --- |
| `EFER` | 置位 `SYSTEM_CALL_EXTENSIONS` | 启用 syscall/sysret |
| `EFER.NXE` | 置位 `NO_EXECUTE_ENABLE` —— 在 **`paging::init`** 里，不在这里 | 让页表项 NX（不可执行）位生效，用户页 **W^X** 的前提；CPU 无 NX（`CPUID.8000_0001H:EDX[20]=0`）时打印告警并跳过 |
| `STAR` | sysret CS=4 / SS=3 (RPL3)，syscall CS=1 / SS=2 (RPL0) | 段基址 |
| `LSTAR` | `syscall_entry` | syscall 入口 |
| `SFMASK` | `INTERRUPT_FLAG` | 进入时清 IF |

### Ring3 切入与上下文（`syscall.rs`）

- `switch_to_user(entry: u64, stack_top: u64, arg: u64) -> !`：构造 iret 帧首次切入 Ring 3；`arg` 经 `rdi` 传入用户 `_start(domain_id)`（传递所属域 id）。
- `syscall_entry` 用 `r10` 暂存用户 `rsp` 并保留 `rbx`（`r10` 为 caller-saved 且不在 syscall ABI 中；`rbx` 为 callee-saved，用户态跨 syscall 复用）。
- 用户态 syscall 封装（[user/libmorion/src/syscall.rs](../../user/libmorion/src/syscall.rs)）声明 `rcx/r11/r8/r9/r10` clobber，且参数寄存器 `rdi/rsi/rdx` 用 `inout(..) => _`（内核 `syscall_entry` 会改写它们，仅 `in` 会让编译器误以为跨 syscall 不变）。

## 6. 模块 API 索引

### 视频（[kernel/src/video/mod.rs](../../kernel/src/video/mod.rs)）

- `init(info: &BootInfo)` / `ready() -> bool`
- `print(s: &str)` / `println(s: &str)`
- `print_hex(v: u64)` / `print_u64(v: u64)`
- `clear(color: u32)` / `clear_screen()`
- `width() -> u32` / `height() -> u32`
- `print_logo()`（打印启动 LOGO，整体水平居中；内容见 `logo.rs`，纯 ASCII）
- **内核终端只管输出**（G4）：输入行编辑 / 行历史导航 / 光标**都不在内核**了 —— 键盘字节经
  [`crate::key`](../../kernel/src/key.rs) 交给用户态（`SYS_KEY_PUSH` / `SYS_KEY_READ`），
  行编辑与回显在 [`morion::console`](../../user/libmorion/src/console.rs)。内核终端因此只服务
  **引导期日志**（`gfx_srv` 接管前）与 **panic 屏**；接管后 `print` 只写 COM1。
- 屏幕模型：`HISTORY`（512 行环形缓冲）+ 一行「当前未提交行」`LINE`。`redraw()` 先铺背景渐变，
  再画历史里最后 `hist_visible()` 行与当前行；`\n` 提交当前行，行满（按**显示列**）自动提交。
  原先「历史区 + 底部固定输入行 + 光标下划线」两区布局、以及配套的输入/输出隔离
  （`input_detach`/`input_reattach`、`INPUT_BASE`、`IN_ACCUM` 跨行累积）**已随 G4 一并删除**。
- **文本绘制**（[kernel/src/video/unicode.rs](../../kernel/src/video/unicode.rs)，G3c 后只剩最小 ASCII）：
  - ASCII（0x20..=0x7E）用 `font.rs` 的 **8x16** 字模；**其余字符一律画空心豆腐块**。
    汉字/全角点阵（`cjk.bin`，≈276 KB）与「按字形宽度排版」的能力已随 G3a/G3c 搬到用户态
    [`user/srv/src/gfx/`](../../user/srv/src/gfx/)（`glyphs.rs` + `cjk.bin` + `term.rs`），
    内核不再携带任何汉字点阵 —— 内核 ELF `347376 → 70504` 字节。
  - 宽度改由 `east_asian_wide()` 粗判：ASCII 1 格、汉字/全角类 2 格、控制字符 0 格 ——
    与用户态 `gfx_srv` 的口径一致，接管前后同一段文字的列数不会突变。
  - `decode(bytes, i) -> (码点, 字节数)`（非法序列按 U+FFFD 且只前进 1 字节，不能让一个坏字节卡住整行）
    / `next_index` / `prev_index`（按字符移动光标与退格，绝不切进多字节字符中间）**保持原样** ——
    内核仍要认 UTF-8 的**边界**，只是不再有字形。
  - **串口不受影响**：`print` 把 UTF-8 字节直接写 COM1，不经过本模块，故 headless 回归与判据不变；
    屏上的中文只在 `gfx_srv` 接管**之前**那几秒（以及 panic 屏）退化成豆腐块。
  - `cjk.bin` 由 [scripts/gen-cjk-font.py](../../scripts/gen-cjk-font.py) 生成：GNU Unifont（OFL-1.1），
    字符集 = **GB2312 全集** ∪ 仓库源码/文档里出现过的所有非 ASCII 字符（约 7500 字 / 276 KB，
    定长 37 字节记录按码点升序）。产物**随仓库提交**，构建不依赖网络与 Python，`--out` 默认就指向
    `user/srv/src/gfx/cjk.bin`。
  - 单测：`video/unicode.rs` 里 4 条 host 测试锁住宽度口径、混排列数、字符边界与截断序列处理。
- **终端行模型（显示列而非字节）**：行缓冲存的是 **UTF-8 字节**，`CUR_POS` / `CUR_COL` 是字节下标，
  但排版与换行按 `unicode::str_width` 折算的**显示列**（`append_cp` 用列数判满行、渲染时逐字符推进
  `8/16 px`、光标下划线宽度取该字符宽度）。退格 / ←/→ 都走 `prev_index` / `next_index`，
  所以行里有汉字时编辑不会把它的 UTF-8 字节切开。
- 背景：清屏/重绘不再填纯色，而是调用 `bg_fill_rect` / `bg_fill_all`，按 `bg::color_for_row` 的**竖直渐变**
  逐行取色填充。颜色表 `bg.rs` 由 `resources/system/terminal/终端背景_1024x768.raw` 采样得到（64 级 ≈ 256 字节），
  已压暗偏蓝以保证白色文字可读；分辨率无关，重绘开销与原先纯色填充同量级。
- 帧缓冲格式：BGRA8888，颜色 `0x00RRGGBB`（[framebuffer.rs](../../kernel/src/video/framebuffer.rs)）；实际分辨率 1280x800（q35 + virtio + OVMF）。

### 物理帧分配（[kernel/src/memory/frame_allocator.rs](../../kernel/src/memory/frame_allocator.rs)）

- `init(info: &BootInfo)` / `print_stats()`
- `allocate_frame() -> Option<u64>`（返回物理地址）
- `free_frame(addr: u64)`
- `inc_ref(addr: u64)` / `dec_ref(addr: u64) -> bool`（共享帧引用计数；`dec_ref` 归零返回 `true`）
- `is_tracked(addr: u64) -> bool` / `release_user_frame(addr: u64)`（**域销毁时的逐页归还规则**：登记过引用计数的帧按计数递减、归零才 `free_frame`；**未登记**的帧 —— 镜像页 / 用户栈帧 / 页表帧 —— 视为该域独占，直接 `free_frame`。引用计数表只有 64 槽，故只有「用户显式申请/共享」的帧登记在内）
- `total_frames() / free_frames() / total_memory_bytes() / free_memory_bytes()`
- `FRAME_SIZE = 4096`
- 初始化末尾调用 `reserve_active_page_tables()`：把当前 `CR3` 页表层级引用的物理帧标记为占用。
  这些「引导器遗留页表」在 UEFI 内存图中可能为 CONVENTIONAL，若被当作空闲帧分配并清零，会摧毁
  正在生效的地址翻译，导致启动到 `paging::init` 即 #PF → #DF → triple fault（且随镜像大小变化时有时无）。
- 内核保留上界取 `_kernel_end` **向上对齐到 64 KiB**：链接符号与镜像实际占用末尾可能有少量出入，
  留余量可确保内核栈顶所在的帧不会被当作空闲帧分配（栈顶就在镜像末尾附近，被复用为页表会立刻被栈写坏）。
- **内核保留区间 `pin_range(start, end)`**（G6）：登记「任何路径都不得释放」的物理区间，`free_frame` 对其**空操作**。目前只登记**帧缓冲** —— 它是被 `SYS_FB_MAP`（`map_mmio`，**不走引用计数**）映射进 `gfx_srv` 的，若不挡，「同域重启」清地址空间时 `free_user_space` 会把它当本域独占帧 `free_frame`，大内存配置下等于把显存交回分配器。与 `reserve_frame`（初始化时占位）互补：那个管分配，这个管释放。

### 分页（[kernel/src/memory/paging.rs](../../kernel/src/memory/paging.rs)）

- `init()`（先 `enable_nx()` 开 `EFER.NXE`，再建页表并载入 CR3）
- `UserPagePerm { ReadOnly, ReadWrite, ReadExecute }`（**W^X 的唯一落点**：只有 `ReadExecute` 可执行，它必然不可写；另两种一律 `NO_EXECUTE`。`ReadWrite` 是数据/栈/共享缓冲/匿名页的默认权限）
- `map_user_page(domain_id: u64, vaddr: u64, paddr: u64, perm: UserPagePerm)`（USER 权限映射，权限由调用方给出）
- `resolve_user_page(domain_id: u64, vaddr: u64) -> Option<u64>`（遍历页表把 vaddr 反查为物理地址）
- `unmap_user_page(domain_id: u64, vaddr: u64) -> Option<u64>`（解除映射并返回原物理地址）
- `free_user_space(pml4_phys: u64)`（**域销毁 / 同域重启时调用**：只遍历 P4[1]——其余 PML4 条目是所有域共享的内核映射，不能动——逐页 `release_user_frame` 归还物理帧，再回收 PT/PD/PDPT 页表帧，最后清掉 P4[1] 条目。兼容 2 MiB 大页。调用者必须是**别的域**，不能拆自己正在用的页表。**G6**：内核保留区间（帧缓冲）由 `free_frame` 内部挡下，不会被归还）
- `heap_start() / heap_size()`

### 域（[kernel/src/domain.rs](../../kernel/src/domain.rs)）

- `create() -> u64`（返回域 id；域表是 `Vec<Option<Domain>>`，**运行时也能建**）
- `destroy(id: u64) -> bool`（**销毁域**：摘除域表槽位 → 释放用户地址空间 → 清能力/句柄、邮箱、分页器、中断注册 → 摘除并终止它的任务、唤醒等它的任务。槽位归还以便复用；**不允许自我销毁**，门禁在 `SYS_DOMAIN_DESTROY`）
- `request_destroy(id)` / `reclaim_pending()`（**退出即回收**的延迟机制：`SYS_EXIT` 时任务仍跑在自己的内核栈与页表上，不能就地销毁，故 `request_destroy` 只登记，由 `reclaim_pending` 在**别的任务**上下文（时钟 `tick`）真正销毁）
- `is_boot(id)` / `BOOT_DOMAINS = 17`（**白名单**：引导期服务域 `0..16` 退出时不自动销毁；它们的槽位始终被占用，故「id < 17 即引导域」是稳定不变量。**N0** 由 16 扩到 17，给 `net_srv`(16) 腾号）
- `pml4_of(id: u64) -> u64`（返回该域 PML4 物理地址）
- `is_alive(id: u64) -> bool` / `alive_count() -> usize`（自测取证用）
- **域 id 必须复用**（`slot_for` 优先取第一个空槽）：域 id 是各全局表的下标（`cap`/`ipc`/`pager` 是 `Vec`，`irq::ANY_MASK` 是 `[u64; 64]`），单调增长会让反复"加载→销毁"迟早越界
- 建域时复制当前 PML4 的**内核空间**条目（代码 / 恒等 / offset / 内核堆），**显式跳过 P4[1]（用户空间）**：引导期它是空的所以看不出来，但 `SYS_SPAWN_ELF` 是在调用者的 syscall 里建域（CR3 = 调用者的 PML4），照抄过去会让新域与调用者**共用同一棵用户空间页表** —— 既没有隔离，映射新程序还会撞上调用者自己的镜像（`PageAlreadyMapped`）。新域的用户空间必须从零由加载器建立。

### 可执行文件加载（[kernel/src/elf.rs](../../kernel/src/elf.rs) + [kernel/src/exec.rs](../../kernel/src/exec.rs)）

- `elf::parse(bytes) -> Option<Image>`（**信任边界**：镜像字节完全由用户态提供，故每个字段先校验再用 —— magic / `ELFCLASS64` / `ELFDATA2LSB` / `ET_EXEC` / `EM_X86_64` / `e_phentsize == 56` / `e_phnum ≤ 32`；每段 `p_filesz ≤ p_memsz`、文件内容不越界、段整体落在 `[USER_SPACE_BASE, USER_SPACE_END)`；**入口必须落在某个已载入段内**。全程不分配资源、不 panic，非法即 `None`）
- `exec::spawn_elf(image, loader) -> Option<u64>`（**运行时**：解析 → `domain::create()` + `cap/ipc/pager::add_domain(domain[, loader])` → 逐段映射 → 映射用户栈 → `try_spawn_user`；返回新域 id）
- `exec::spawn_elf_at(domain, image) -> bool`（**引导期**：不建域、不登记全局表 —— 16 个服务域由内核按固定域号先建好并授权，这里只"解析 → 映射镜像与栈 → 起任务"。镜像由**引导器**从 ESP 的 `EFI/morion/services/<name>.elf` 读入 `LOADER_DATA` 页，经 `BootInfo` 的模块表交来（见 `bootinfo::ServiceModule`；E3b 起内核不再 `include_bytes!`），见 [kernel/src/main.rs](../../kernel/src/main.rs)。镜像以**物理地址**给出，内核靠恒等映射读它 —— 故载入前用 `paging::is_identity_mapped(addr, len)` 判可达性）
- 失败清理（E2b 地基）：`spawn_elf` 映射或起任务任一步失败 → `domain::destroy(domain)` 把半成品域拆干净（地址空间 + 各全局表行）后返回 `None`，不再漏域漏页。
- `elf::parse` 解析 `p_flags`（`PF_X`/`PF_W`）并**拒绝 `PF_W|PF_X` 的段**（W^X 的镜像侧前提）。
- 映射要点：**分两遍** —— 先按「**页权限并集**」逐页建映射，再拷内容。这样相邻段共享边界页时，页权限在第一次映射时就已经是并集，不需要事后改页表项（`Mapper` 没有改标志的入口）。一页只映射一次，重复 map 会 panic；新页先**清零**再拷入文件内容（分配器不保证零，`.bss` 与段尾填充都依赖这一点）；物理地址 < 4 GiB 在恒等映射内，故可直接当指针写。
- **W^X（E3a）**：段权限 → 页权限（`PF_X` → `ReadExecute`，`PF_W` → `ReadWrite`，其余 `ReadOnly`）；并集同时含 W 与 X 的页**拒绝加载**（不静默降级成 RWX）。用户栈与所有 syscall 映射的页（`SYS_ALLOC_PAGE`/`SYS_SHARE_PAGE`/`SYS_MAP_ANON`、NVMe 配置页与 DMA 页）一律 `ReadWrite`（RW + NX），MMIO 映射为 RW + NOCACHE + NX。
- **链接脚本要求**：`user/linker.ld` 在 `.data` 前 `ALIGN(4096)` —— 否则 `.text/.rodata`(RX) 与 `.data/.bss`(RW) 会落在**同一页**（小镜像很常见），页级 W^X 无法满足、加载直接失败。
- **保护违例不转发分页器**：用户态缺页带 `P=1`（页已映射、但这次访问的类型不被允许：写只读页 / 执行 NX 页）时按需分页补不了 —— 转给分页器只会让它去映射一个**已映射**的页，撞内核 `PageAlreadyMapped` panic。故 `#PF` 处理器对 `PROTECTION_VIOLATION` 直接**终止该任务**（走 E2b 的退出即回收），内核继续跑。
- 不做的事（见 roadmap「E1 未完成」）：**不登记共享帧引用计数**（镜像页/栈帧都是该域独占，销毁时按「未登记即独占」直接归还，见物理帧分配小节）、**动态链接 / 重定位**（只吃 `ET_EXEC`）。域与帧的回收已在 E2b 地基落地（`SYS_DOMAIN_DESTROY` / 退出即回收）。

### 调度器（[kernel/src/scheduler/mod.rs](../../kernel/src/scheduler/mod.rs)）

- `init()`
- `spawn(entry: extern "C" fn(), domain: u64)`（内核任务）
- `spawn_user(entry: u64, user_stack: u64, domain: u64)`（Ring 3 用户任务；引导期用，吃满任务表即 panic）
- `try_spawn_user(entry: u64, user_stack: u64, domain: u64) -> bool`（**运行时**用，供 `SYS_SPAWN_ELF`；任务表满返回 false 而**不 panic** —— 那是用户可触发的路径）
- `run() -> !`
- `tick()` / `yield_now()` / `sleep(ms: u64)`（`tick` 开头还会 `domain::reclaim_pending()` 执行**退出即回收**的挂起销毁 —— 此刻跑在被中断任务的上下文里，与挂起域必不相同）
- `block_current(on_domain: u64)` / `wake_one(domain: u64)`
- `block_current_timeout_ms(on: u64, ms: u64)`（**带超时的阻塞**：TCB 记 `wake_deadline`，由 `tick()` 在到期时置回 `Ready`；`SYS_IRQ_WAIT` 用它等设备中断，超时即返回让调用方回退。`tick()` 的到期唤醒因此覆盖 `Sleeping` 与**带超时 `Blocked`** 两态）
- `current_domain() -> u64`
- `set_current_reply_target(target: u64)` / `current_reply_target() -> u64`（`reply` 回复目标追踪；`u64::MAX` 表示无）
- `exit_current() -> !`（`SYS_EXIT` 调用的任务退出入口；若这是所属域的**最后一个**任务，会 `domain::request_destroy` 登记该域 —— **延迟**到 `tick` 真正销毁，因为此刻仍跑在本域的栈与页表上，见域小节）
- `remove_domain(domain: u64)`（**域销毁时调用**：把该域的任务槽位置 `None`（drop 归还内核栈），并唤醒所有 `wait_on == domain` 的阻塞任务 —— 否则等它的域会永远睡下去。`SCHEDULER` 尚未初始化时直接返回）
- `KEY_WAIT`（伪域 id `u64::MAX-1`：表示等待**一个按键字节**；`SYS_KEY_READ` 用 `block_current(KEY_WAIT)`，`key::push` 推键后 `wake_one(KEY_WAIT)`。G4 前这名号叫 `INPUT_WAIT`，语义是「等一行输入」）
- `IRQ_WAIT_MARK` / `irq_wait_token(domain: u64)`（伪等待键 `u64::MAX-0x300-domain`，落点 `[u64::MAX-0x3FF, u64::MAX-0x300]`：`SYS_IRQ_WAIT` 以它阻塞、`irq::set_pending` 按掩码命中后以它唤醒。**按域取键**而非按向量 —— 一个域同时只可能有一个任务在等中断，掩码等待天然属于「域」；与真实域 id、`KEY_WAIT` 都不重叠，故中断唤醒不会误撞 IPC 的唤醒）
- **空闲任务** `task_idle`（[kernel/src/main.rs](../../kernel/src/main.rs)）循环 `hlt(); yield_now();` —— `hlt` 交出 CPU（KVM 里 vCPU 因此退出客户机，宿主设备模型才有机会 post 完成并投中断），返回后立即让出，使**刚被中断唤醒的域马上接手**而不必再等一个时钟 tick。

任务表常量：`MAX_TASKS = 32`（含运行时 `SYS_SPAWN_ELF` 建的域，故留足余量），内核栈 `STACK_SIZE = 4096 * 8`（32 KiB）。**每任务 32 KiB 内核栈来自内核堆**，所以任务表上限与 `paging::HEAP_SIZE` 是绑在一起的（32 × 32 KiB ≈ 1 MiB，堆因此为 4 MiB）。

### IPC（[kernel/src/ipc.rs](../../kernel/src/ipc.rs)）

- `init(domain_count: usize)` / `add_domain(id: u64)`（运行时建新域时按 id 补一个空邮箱）/ `remove_domain(id: u64)`（域销毁时清掉它的邮箱行，槽位随域 id 复用）
- `send(to: u64, tag: u64, payload: &[u8]) -> bool`（非阻塞）
- `deliver(from: u64, to: u64, tag: u64, payload: &[u8]) -> bool`（内核内部投递，绕过能力检查，用于缺页等异常转发）
- `receive() -> Message`（阻塞，记录回复目标供 `reply` 使用）
- `call(to: u64, tag: u64, payload: &[u8]) -> Message`（同步调用：发送请求 + 阻塞等回复）
- `wake_one(key: u64)`（把第一个 `wait_on == key` 的阻塞任务置回就绪；**域销毁**与**按键到达**都靠它）
- `reply(tag: u64, payload: &[u8]) -> bool`（回复最近一次 `receive` 到的调用者）
- `PAYLOAD_LEN = 96`（VFS 请求要把绝对路径整条装进 payload），邮箱容量 `MAILBOX_CAP = 16`
- `Message { from, to, tag, payload }`（`#[repr(C)]`，与用户态同布局）

### 能力系统（[kernel/src/cap.rs](../../kernel/src/cap.rs)）

- `init(domain_count: usize)`
- `has(domain: u64, cap: Capability) -> bool`
- `grant(domain: u64, cap: Capability) -> bool`
- `revoke(domain: u64, cap: Capability) -> bool`
- `grant` / `revoke` 保存并恢复中断使能状态，避免 boot 期（IF=0）被提前开中断。
- `Capability::SendTo(u64)` / `Capability::MapInto(u64)` / `Capability::Irq(u8)` / `Capability::Mmio(u64)` / `Capability::Spawn` / `Capability::Fb`，每域 `CAP_SLOTS = 32`（自测域 app 要把 fat32/mount/tmpfs/mfs/ext2/exfat/block/echo/gfx 的 `SendTo`+`MapInto` 全授一遍，16 个槽不够 —— 槽满时 `grant` 会打印 `[WARN] capability table full: grant dropped`）
- `Spawn`（E1）无参数 —— 它就是「可以造进程」这张凭证：`SYS_SPAWN_ELF` 建新域 + 载入镜像 + 起任务全靠它。默认不授予，引导期只给自测域（将来给 shell / init）。`SYS_CAP_SEND` 的 `kind` 相应加 `4=CAP_KIND_SPAWN`（`arg` 忽略）。
- `Fb`（G1 图形子系统）无参数 —— 「可访问帧缓冲」这张凭证：`SYS_FB_INFO` / `SYS_FB_MAP` / `SYS_FB_TAKEOVER` 全靠它。`Mmio` 按页对齐基址**逐页**匹配（设备 BAR 一页一条），而帧缓冲是整块（可达上千页），逐页授权塞不下有限的能力槽（`CAP_SLOTS`），故单列一类。引导期只给 `gfx_srv`（域 15）；`kind` 加 `5=CAP_KIND_FB`（`arg` 忽略）。
- `init(domain_count)` / **`add_domain(id: u64)`**（运行时经 `SYS_SPAWN_ELF` 建新域时按 id 补一行空槽；新域**零能力**，与 `init` 同理按域 id 索引，故必须在 `domain::create()` 之后调用）/ **`remove_domain(id: u64)`**（域销毁时清掉该域的能力行与句柄行，槽位随域 id 复用）
- **「能力即句柄」句柄表**：`handle_issue(domain, obj) -> u64` / `handle_lookup(domain, handle) -> Option<u64>` / `handle_drop(domain, handle) -> bool`，每域 `HANDLE_SLOTS = 32`。槽内存放**不透明**对象标识（微内核不解释其含义，libvfs 传 `(服务域 << 32) | 服务内 fd`），由 `SYS_CAP_ISSUE`/`SYS_CAP_LOOKUP`/`SYS_CAP_DROP` 暴露给用户态。
- **能力随 IPC 传递**（`SYS_HANDLE_SEND`/`SYS_CAP_SEND`）两条路径，语义刻意不同：
  - `handle_move(from, to, handle) -> u64`：**移动**。先取出源槽对象、再在目标域找空槽；目标槽满则**回滚**（对象放回原槽），故失败时不会出现「两边都没有」。移走后源域该句柄立即失效 —— 「能力是唯一凭证」，同一份能力同一时刻只属于一个域。这是 fd 传递要的语义（交出 fd 后自己不再持有）。
  - `delegate(from, to, cap) -> bool`：**复制**。`from` 必须自己持有 `cap`（**不允许放大** —— 没有的能力给不出去，这是能力模型的根）；检查与写入在同一把锁内完成，避免「检查后被抢先」。`to` 已持有该能力时幂等返回成功且不占新槽（否则重复委派会把 16 个槽位耗光）。
  - 两者都要求调用方持有 `SendTo(to)`：`delegate` 上是双层校验（外层管「能不能给对方」，内层管「东西是不是我的」）；`handle_move` 上是防 DoS（否则任何域都能把对象灌进别的域的 32 个句柄槽）。
  - `decode(kind, arg) -> Option<Capability>`：把 syscall 的两个整数还原成 `Capability`，并对 `arg` 做与该能力使用点一致的校验（`Irq` 是 u8、`Mmio` 必须页对齐），否则可以造出永远匹配不上的能力，白占对方槽位。

### 分页器（[kernel/src/pager.rs](../../kernel/src/pager.rs)）

- `init(domain_count: usize, pager_domain: u64)`（每域统一登记 `pager_domain` 为其分页器；**运行时**建的新域用 `add_domain(id, pager_domain)` 按 id 补一行 —— 新域的分页器 = 加载它的那个域，loader 自然是该程序的缺页后端）
- `add_domain(id, pager_domain)` / `remove_domain(id)`（域销毁时摘掉登记，`PAGERS` 表项置 `None`）
- `of(domain: u64) -> Option<u64>`（查询某域的分页器域 id；该域不存在 / 已销毁 / 无分页器时为 `None` —— 缺页处理器据此**停下来**而不能无限重试）
- `deliver_fault(pager: u64, info: PageFaultInfo)`（把缺页信息序列化进 IPC 消息 payload，经 `ipc::deliver` 投递并 `wake_one` 分页器）
- `PageFaultInfo { fault_domain, fault_addr, error_code }`（`#[repr(C)]`，24 字节，与用户态同布局）
- `FAULT_TAG`（缺页消息 tag 标记，区分普通 IPC）

缺页流程：`page_fault_handler` 读 CR2 → `deliver_fault`（投递 IPC 消息到分页器邮箱）→ `block_current(fault_domain)`；分页器经 `SYS_RECV` 取消息、从 payload 解出 `PageFaultInfo` → `SYS_MAP_ANON` 映射零帧 → `SYS_PAGE_FAULT_REPLY` 唤醒缺页域（回复目标由 `receive` 记录）。

### IRQ 转发（[kernel/src/irq.rs](../../kernel/src/irq.rs)）

- `register(irq: u8, domain: u64)`（登记某域为 PIC `irq` 的驱动域；调用者须先通过 `SYS_REGISTER_IRQ` 校验 `Capability::Irq(irq)`）
- `dispatch(irq: u8, data: u64)`（把中断数据作为 IPC 消息 tag 转发给注册域；从 IRQ 处理器 IF=0 调用，非阻塞、不改变中断位）
- 最多支持 16 个 PIC IRQ（master 8 + slave 8）；`HANDLERS` 为 `[Option<u64>; 16]`。
- `remove_domain(domain)`（**域销毁时调用**：把「PIC IRQ 注册 + MSI 向量注册 + 等待掩码」三处一起摘干净，槽位随域 id 复用，新域不会顶着旧注册）
- **MSI/MSI-X 向量**（阶段 39/40/41）走另一条路：`register_vector(vector, domain)` / `is_registered_by(vector, domain)` / `set_pending(vector)` / `set_any_mask(domain, mask)` / `clear_any_mask(domain)` / `take_pending_any(mask, domain) -> Option<u8>`。
  - 向量处理器置一个「待处理位」并**唤醒**等在**掩码**上的域，**不投 IPC**：那个邮箱同时也是驱动收请求的邮箱，拉取即消费，还会改写内核记录的回复目标，`reply` 会投错域。驱动改用 `SYS_IRQ_POLL` 取位 / `SYS_IRQ_WAIT` 阻塞等。
  - `VECTORS`/`PENDING` 均为 `[…; 256]`，以向量号为下标；`take_pending_any` 遍历掩码里的位、**只取自己注册的那个**并同时校验注册者，别的域读不到别人的中断。`ANY_MASK` 是 `[u64; ANY_MAX_DOMAINS]`（`ANY_MAX_DOMAINS = 64`），按**域**存「正在等的向量掩码」—— 一个域同时只有一个等待者，故域即等待身份。
  - `set_pending` 置位后在短持 `ANY_MASK` 内算出「掩码含该向量」的域，**放锁后再**逐个 `wake_one(irq_wait_token(domain))`（先 `VECTORS`、再 `PENDING`、再 `ANY_MASK`、最后调度器）：调度器的锁在关中断下被多处持有，不能与 IRQ 的锁形成嵌套。

「中断即 IPC」模型：硬件 IRQ 处理器读设备数据（如键盘 scancode）→ `irq::dispatch` 投递到驱动域邮箱 → 驱动域循环 `SYS_RECV` 接收并处理，再 `send_eoi`。

### LAPIC 最小支撑（[kernel/src/arch/apic.rs](../../kernel/src/arch/apic.rs)）

MSI/MSI-X 的物理形式是**设备向 LAPIC 的「中断消息」地址写一条消息**（`0xFEE0_0000 | (apic_id << 12)`，数据里带向量），没有 LAPIC 就永远收不到。`apic::init()` 只做必需的最小集：

- `IA32_APIC_BASE`(MSR `0x1B`)：取基址并确保 bit11（EN）置位；`SVR`(base+0xF0) 置软使能 + 伪中断向量 `0xFF`；`TPR`(base+0x80) 置 0。
- `LVT0`(base+0x350) 必须是「投递模式 ExtINT + 不屏蔽」：LAPIC 一旦使能，8259A 的 PIC 中断改由 LINT0 以 ExtINT 透传，KVM 正是据此判断「PIC 中断还要不要投」（`kvm_apic_accept_pic_intr`），LVT0 被屏蔽则时钟/键盘立刻失效。故只在它不满足时改写，从不覆盖固件已设好的值。
- `eoi()`：MSI 向量处理器结束时写 `base+0xB0`；ExtINT 透传来的中断不经 LAPIC 的 ISR，仍由 `pic::send_eoi()` 收尾。

### 用户态运行库 libmorion（[user/libmorion](../../user/libmorion)）

所有用户程序共用的"运行时"（crate 名 `morion`），相当于 crt0 + libc 的最小子集：

- `syscall`：syscall 封装（`sys_*`）+ 终端打印（`print` / `println` / `print_u64` / `print_hex` / `flush`）
  + `domain_id()`（`_start` 记下的本域 id）。
- `vfs`：libvfs（fd / 挂载路由 / 能力句柄守卫）—— 原 `user/src/vfs.rs`，除路径外无改动。
- `exec`：`spawn_file(path) -> Option<u64>`（见下）。
- **入口与 panic**：`_start(domain_id)`（放 `.text._start`，链接脚本排在镜像最前端，`ENTRY(_start)`）
  调程序定义的 `morion_main(domain_id)`，返回即 `sys_exit()`；`#[panic_handler]` 打印一行后退出。
  程序**不要再自己定义**这两个（重复定义会链接冲突）。
- 库**不传链接参数**：`-T user/linker.ld` 与 `-nostdlib` 由每个程序的 `build.rs` 声明。

`exec::spawn_file(path)` 的链路：`vfs::open` → 分块读进本域内存 → `SYS_SPAWN_ELF`。
两个实现细节值得记：

1. **经一页"中转页"而不是直接读进暂存区**：文件服务是把数据写进调用方指定的那一页（同地址共享），
   若让每个暂存页都共享出去，一个几百 KB 的程序就要占几十个内核共享帧槽位（`frame_allocator`
   只有 64 个）；这里只用一页反复中转 + 本域内 `memcpy`，共享帧表只占 1 个槽位。
2. **对同一 (页, 域) 只能 `share_page` 一次**（重复映射会撞内核 `map_user_page` 的
   `PageAlreadyMapped` panic），故用一张按域 id 置位的位图记住已共享过谁；`sys_alloc_page`
   同理要用 `sys_virt_to_phys != 0` 先判断"已映射"，否则第二次 `run` 会 panic。

### 文件服务与挂载层（用户态）

文件系统全部位于用户态，经 libvfs 统一接入（见 [user/libmorion/src/vfs.rs](../../user/libmorion/src/vfs.rs)）。

- 域布局（[kernel/src/main.rs](../../kernel/src/main.rs)）：`5 block_srv / 6 fat32_srv / 7 app / 8 shell / 9 mount_srv / 10 tmpfs_srv / 11 mfs_srv / 12 ext2_srv / 13 exfat_srv / 14 init / 15 gfx_srv / 16 net_srv`（共 17 个域；`ipc::init`/`cap::init`/`pager::init` 一律按 `domain::BOOT_DOMAINS` 取数，避免"建域数 ≠ 表长度"导致按下标访问越界）。

### 服务监督者 init（E3c）

`user/srv/src/init.rs`（域 14）补上"服务实例退出后没人管"这一环：

- **巡检**：每 40 ms 对一批长期驻留的服务域问一次 `SYS_DOMAIN_ALIVE`（不设内核回调 —— 内核只需要机制，不需要认识"服务"这个用户态概念）。
- **重启（两个镜像来源）**：发现某个域没有存活任务，**先试引导模块内存镜像**（`SYS_SPAWN_ELF_MODULE(43)`，内核按域号从 E3b 的模块表取，**不依赖磁盘**）；不可用时再回退到 FAT32 根卷的 `/system/services/<name>.elf`（`SYS_SPAWN_ELF_AT(41)`）。两条路都**原地**拉起 —— 域号不变，故 libvfs 里写死的 `FAT32_DOMAIN=6` 那类 ABI 全部照旧；日志会打印来源（`, from memory)` / `, from disk)`）。
- **监督范围**：`pager / echo / kbd / fat32_srv / mount_srv / tmpfs_srv / mfs_srv / ext2_srv / exfat_srv / gfx_srv`（10 个）。内存镜像这条路让**文件服务本身**（fat32/mfs）也可被重启，解掉"读盘要靠文件服务、文件服务死了没法自救"的鸡生蛋问题。**gfx_srv(15) 于 G6 纳入**（原与 block_srv 同理被排除）：前提是 ① 帧缓冲已登记为内核保留区间（`frame_allocator::pin_range`），`domain::reset` 不再误放显存；② 客户端在服务重启后**重建共享会话**（`morion::gfx` 重发 `SYS_SHARE_PAGE`）且 `ipc::call` 不再永久挂起 —— 见本文件「图形服务 gfx_srv」的 G6 段与 [roadmap-gfx.md](roadmap-gfx.md)。仍刻意不含：**block_srv(5)**（内核为它映射了 NVMe 配置页与 DMA 帧，`domain::reset` 会把这些物理帧还给帧分配器 —— 帧缓冲那类保留帧现在挡住了，但 NVMe 的**内核侧映射**还未登记，纳入前仍要先做这件事）与**按设计会正常退出**的 sender / receiver / app / shell（监督它们等于无休止重启）。
- **自测 FS-29**（[user/srv/src/app.rs](../../user/srv/src/app.rs)）：app 用 `sys_send(3, ECHO_QUIT_TAG)` 让 echo 自己 `SYS_EXIT` → 断言域 3 一度"没有存活任务" → 等 init 拉起来 → 断言**域号仍是 3**、能正常回显（`call` 得 `tag+1`）、存活域数不变。
- **注意**：`SYS_SPAWN_ELF_AT` / `SYS_SPAWN_ELF_MODULE` 只允许"重启"（目标无存活任务），不允许"抢占"；重启用的是 `domain::reset`（清用户地址空间、保留域与它的分页器/能力注册），不是 `domain::destroy`（那会把域号一起交还）。
- **libvfs 路由**：每个路径操作先经 `mount_lookup(path)` 向 `mount_srv` 查询，回复打包为 `[63:40] 卷编码 | [39:32] 服务域 | [31:0] 挂载点前缀长度`（**M1b** 起含卷编码：0 = 该服务的默认卷，否则 = 卷号 + 1）；`route()` 去掉挂载前缀得到子路径，并把**卷编码写进请求 tag 的高 32 位**（tag 正文仍是 4 字节 ASCII，服务端用 `vfs::tag_body` 剥掉高位 —— 路径类请求因此天然带上目标卷，不必给每个请求结构体加字段）。
- **对外 fd 编码**：`[63:48] 能力句柄 | [47:32] 服务域 | [31:0] 服务内 fd`。`open`/`creat` 成功后向内核申请句柄（`SYS_CAP_ISSUE`）并编进高位；`read`/`write`/`readdir` 先经 `cap_guard`（`SYS_CAP_LOOKUP` 校验句柄有效且对象标识与 fd 一致）再下发；`close` 撤销句柄（`SYS_CAP_DROP`）。句柄被撤销后该 fd 上的任何 I/O 都失败——这就是「能力即句柄」的执行点。
- **mount_srv**：维护「挂载点前缀 → 服务域 + 卷编码」表，组件边界敏感的最长前缀匹配（`/tmpfoo` 不匹配 `/tmp`）。表是**运行时可变的**：`mount_main` 启动时写入引导默认项 `/ → fat32_srv(6)`、`/tmp → tmpfs_srv(10)`、`/mfs → mfs_srv(11)`、`/ext2 → ext2_srv(12)`、`/usb → exfat_srv(13)`（卷编码 0 = 各服务的默认卷），此后任何服务都可经 `MNTA`/`MNTD` 在运行时挂载/卸载。表容量 `MOUNT_MAX = 16`（**M1b 起**由 8 提到 16：引导默认项 5 个 + 每个文件服务上报的额外卷各占一个，插一块两分区的 U 盘就会把 8 个槽用满，届时连 `MNTA` 自动分配的 `/mnt<N>` 都拿不到槽位）。`MNTA` payload 为 `MountReq { domain u64, prefix [u8; 24] }`：前缀为空则 mount_srv 自动分配最小的空闲 `/mnt<N>`，回复挂载槽位号（1 起）；`MNTD` payload 为前缀，回复 1/`u64::MAX`，根 `/` 不可卸载。**M1b 额外卷**：新增 tag `MNTV`（`MountVolReq { domain u64, vol u64 }`）—— 文件服务把它**自己那类**的额外卷上报给 mount_srv，后者自动挂到 `/usb<卷号>`（卷号 = block_srv 卷表里的 id），并把 `enc_of_vol(vol) = vol + 1` 记进该项；重复上报是幂等的（同域同卷返回既有槽位号）。启动期会打印 `mount-dbg: /usb4 domain=12 slot=6` 这类诊断行。
- **fat32_srv**：NVMe（回退 IDE PIO）块设备之上的 FAT32 服务，挂载于 `/`。支持**VFAT 长名（LFN）**：`readdir` 拼接 LFN 项（32 字节/项、逻辑逆序、校验和存于 LFN 项偏移 13）并转成 UTF-8 存入 `DirEntry.long`；`open` 先按 8.3 短名精确匹配，失败再按长名（ASCII 大小写不敏感）回退。**只读长名，不生成 LFN 项**（写入仍只写 8.3 短名）。**M1b 大簇 + 多卷**：簇缓冲由「2 页（4 KiB 簇上限）」改为**固定 16 页整簇缓冲**（`FAT32_CLU_VADDR = USER_BASE + 0x20_0000`，目录缓冲与文件缓冲都别名到它），故 `SecPerClus` 最大 128 扇区 = 64 KiB 簇可挂载（`fat_load_bpb` 在挂载期校验 `bytes_per_sector == 512 && 0 < cluster_bytes <= 64 KiB`，不符即拒绝）；服务循环从 tag 高位取卷编码 → 若与 `FAT_BPB_VOL` 不同则**重新载入该卷的 BPB** 并切 `FAT_CUR_VOL`，之后所有 `block_read/write` 都带上当前卷号 —— 即「一个 fd 绑定一个卷，切卷时重解析几何」。**簇分配游标**：`find_free_cluster` 从 `FAT_ALLOC_HINT` 起向后扫描（找到后推进、扫到表尾回绕、换卷时复位到 2），避免每次分配都从簇 2 线性扫整张 FAT —— 否则写大文件是 O(n²) 次 FAT 读。**写路径**支持 `CREAT/WRITE/MKDIR/UNLINK/RMDIR`（**无 truncate**），大文件跨簇写入由 `write_file_range` 逐簇读-改-写 + 扩展时分配新簇挂链（单文件簇链上限 `MAX_CHAIN = 256`）。FS-18 自测覆盖「写 100000 字节跨簇 → 逐簇读回 → UNLINK 释放」。
- **tmpfs_srv**：纯内存文件系统，挂载于 `/tmp`；平铺节点表（绝对路径 → 节点）+ 32 KiB 字节区，名称限定 8.3 短名并转大写。与 fat32 共存，经挂载层拼成统一目录树。
- **mfs_srv**：原创文件系统 MorionFS（**MFS8 格式**），挂载于 `/mfs`，后端为卷层认领的 MFS 卷（默认 `build/mfs.img`）。4 KiB 块 + 8 字节块头（magic + CRC32）；超级块 A/B 双副本；payload `+256` 起是保留区，其中头 8 字节为**主卷序号**（`MFS_SB_PRIMARY`，u64，0 = 非主卷 —— 见下方「主卷切换」）。**S3a 起空闲位图移出超级块**：块 2/3 是**位图头块**（magic `MFBH`：`gen u64` + `total_blocks u32` + `data_blocks u32` = bb + 各数据块 CRC32 数组），块 `4..4+bb` / `4+bb..4+2bb` 是两份**裸 4096 字节位图数据副本**（无块头），`bb = ceil(total/32768)`，`MFS_DATA_START = 4+2*bb` 之前的块一律强制占用；容量上限因此从内联位图的 30656 块（≈119 MiB）提到 `MFS_MAX_BLOCKS = 1018 × 32768 ≈ 127.25 GiB`。提交出口统一为 **`mfs_bmp_flush()`**（取代 `mfs_write_super`）：gen+1 → 重算全部数据块 CRC → 对两份副本各写**脏区间**位图数据块 + 头块 + 超级块；脏位由 `mfs_bmp_set/clear` 按 `blk/32768` 置位，故只写变动过的区间。**挂载时逐块读位图数据块算 CRC32 与头块数组比对**（要求头块 `gen/total/data_blocks` 与超级块一致；两份都不可用但 SB+itab 有效则 `mfs_rebuild_bitmap()` 全置占用后交给 `mfs_gc` 重建，**不格式化**）。**MFS6 起引入 inode 号间接层**：目录项的 4 字节子字段存 **inode 号**（不再是对象块号），号到块的映射由一棵独立的 COW 树给出 —— 索引块（`MFIX`，1022 槽 → 表块）+ 表块（`MFIT`，1022 槽 → 对象块号）；`ino 0` 为无效/空闲，**根目录恒为 `ino 1`**（根永不改名/删除，故超级块只存 `itab_root`、`ino_count`、`ino_hint`）。索引块内容在内存留一份镜像（查表不读索引块），表块走单条目缓存。**写路径统一走 `mfs_commit_object(ino, buf, magic)`**：COW 对象块 → 更新表槽（COW 表块）→ COW 索引块 → 写超级块。因为父目录条目存的是 ino 且在对象更新时**不变**，MFS5 的「沿祖先链逐级回写」被整体删除 —— **写代价与目录深度无关**。**硬链接**（`LINK`）因此成立：多个目录项指向同一 ino，改文件只动它那一个表槽，所有名字自动看到新内容；`unlink` 是「摘名字」，`nlink` 减到 0 才释放 ino。**内建快照**因此记录 `{gen, itab_root, ino_hint, alloc_next}`（24 字节/条，上限 8，**环形**：满时淘汰最旧一条再写入）—— 必须连**它自己那版 inode 表**一起记，否则回滚后 ino 会翻译到回滚后的对象上；**空间回收**为 mark & sweep：`mfs_gc` 对「当前 + 每个快照」各自用**它自己的表**把 ino 翻成块号并遍历，同时**标记索引块与全部已用表块**（元数据漏标会被回收后重新分配出去），故快照引用的历史块不会被回收（见 `MFS_GC_TAG`）；回收只在请求分派前（低水位 `total/16`）或显式调用时执行，分配失败路径不做回收（避免误判同一次 COW 中已在建、尚未挂到根上的块）。**文件节点**（`MFFL`）：`size`(u64，**MFS8 起**) / `nblocks` + **1005 个直接块指针** + 一级/二级/**三级**间接指针（间接块 magic `MFIN`/`MFI2`/**`MFI3`**，各 1022 个槽位；直接区由 1008 缩到 1005 是为 u64 `size` 与 `ind3` 腾位，`MFS_FILE_RESERVED_OFF` 仍为 4056），单文件上限 = 整卷可用块数（MFS7 起 ≈127.25 GiB；MFS8 起四段映射，块上限 ≈1.07e9，故 4 GiB 不再是天花板），小文件不产生额外 I/O；写路径对「活动间接块」做单次调用内的读-改-COW 缓存（**三级只服务 >4 GiB 文件，走无缓存链路 `mfs_ind_peek3`/`mfs_ind_link3`**）；**空洞按 0 读**（稀疏区段不会读成短读）。**目录**（`MFDI`）用 **ext2 风格变长目录项**：`block(u32)/type(u8)/name_len(u8)/rec_len(u16)/name`，4 字节对齐、按 `rec_len` 串联、删除时把空出长度并给前一条目；名字 ≤255 字节、大小写敏感按字节精确匹配（`mfs_normalize` 只做 `.`/`..`/重复 `/` 规整，不做 8.3 大写化）；节点块（payload +0 为 `ext` 指针、+8 起 40 字节元数据、+48 起为条目区）放不下时挂一个 **`MFXI` 扩展索引块**（1022 个槽位全指向扩展目录块），目录最多 = 1 + 1022 个块。**节点元数据（MFS5）**：40 字节，`mode`(u16) / `owner`(u16，创建者域 id) / `nlink`(u32) / `mtime` / `ctime` / `atime`(u64，Unix 秒 UTC)；文件节点放在 inode 尾部保留区，目录节点紧跟 `ext` 之后（`MFS_DIR_HDR` 8 → 48）。时间由用户态直接读 **CMOS RTC**（端口 0x70/0x71，经 `SYS_PORT_IN8/OUT8`）得到；`atime` 不随读更新（否则读路径退化成写路径）。`mode` **只存储与显示，不做访问判定**（其高 4 位编码**节点类型**，与 ext2 `i_mode` 的 `S_IFMT` 同构，见 `vfs::MODE_FTYPE_*`：`ls -l` 靠它显示 `d`/`l`/`-`；低 12 位才是权限位，`chmod` 只改低 12 位、类型位随节点固定；非 MFS 服务不填类型位，客户端按 `is_dir` 回退）。另有 `TRNC`（truncate：截短释放尾部块、扩展为稀疏）、`RENM`（rename：跨目录，两条路径走调用方共享页，拒绝目录移入自身子孙）、`CHMD`（chmod）、`LINK`（硬链接：同 ino 加一个名字，仅限文件与同一文件服务）、`SYML`（**软链接，M5c**：新节点类型 `MFSL` + 目录项 type 3；目标**内联**在节点 payload 里（`+0 size` = 目标串长度、`+8 起` = 目标字节，元数据仍落在文件布局的尾部保留区）—— 沿用文件布局就是为了让所有元数据函数按 `is_dir = false` 直接复用，不必再加一种类型分支；解析走 `mfs_resolve_ex(canon, follow_leaf)`，**跟随**时把链接分量就地展开成「目标 + 剩余分量」、重新规范化、整条重走（不接着展开点走，因为目标里的 `..` 可能吃掉展开点之前的目录），限深 `MFS_SYMLINK_MAX_DEPTH = 16` 防环、展开超长直接失败；`unlink`/`rmdir`/`rename` 用 `follow_leaf = false`（作用于链接自身），而**中间分量**上的链接两种模式都跟随。**绝对目标是服务命名空间内的路径**：服务只看得见自己那棵子树，客户端 `vfs::symlink_into` 会剥掉同挂载点前缀（`/mfs/a` -> `/a`），**目标不在同一挂载点则直接拒绝创建**（原样存下只会得到一条用户无从分辨的悬空链接）。配套 `RDLK`（`readlink`：不跟随解析到链接自身，把内联的目标串写回调用方共享页，客户端 `vfs::readlink_into` 按挂载前缀**加回**去，故界面看到的是用户输入的原始路径）与 `LSTA`（`lstat`：同 `stat`，但不跟随末段）。GC 里 `MFSL` 按**叶子**处理（目标内联、不引用其它块）—— 漏了这一支会让「只要卷上有软链接，整次回收就放弃」）。旧格式（`MFS1`…`MFS5`/未知 magic/版本不符）首次挂载自动重新格式化。**M6c 安全护栏**：自动格式化**仅**允许「卷类型为 `UNKNOWN`（空白待格式化）或 `MFS`」—— 若该卷已被识别为 FAT/ext2/exFAT（非 MFS），mfs_srv 直接放弃挂载并打印 `mfs: refuse to format non-MFS volume`，避免接真机盘时误格式化既有分区。**M7 按卷几何格式化**：首次格式化的总块数由 `mfs_format_total_blocks()` 给出 = `认领到的卷容量 / 8 扇区每块`，夹在 `[MFS_MIN_TOTAL_BLOCKS = 64, MFS_MAX_BLOCKS]` 之间（下限保证放得下两份超级块 + 根目录 + inode 表），卷容量未知（`MFS_VOL_SECTORS == 0`，如 IDE PIO 回退）时退回 `MFS_DEFAULT_TOTAL_BLOCKS` —— **此前无论卷多大都写死 4096 块 = 16 MiB**，整块新盘也只会格出 16 MiB。挂载时另有一条校验：超级块记的总块数若超过该卷实际容量（换镜像 / 卷号认领错 / 卷被缩小过），该副本判为不可用 → 两份都不可用就走格式化（此前只有「不超过 `MFS_MAX_BLOCKS`」这条上界，盘上记着比卷更大的尺寸时会一路读到盘外）。上界由 `MFS_MAX_BLOCKS`（MFS7 起 ≈127.25 GiB = 1018 个位图数据块 × 32768 块）给出；位图已外置到独立数据块，不再受超级块 payload 大小限制。卷表查询收敛为 `vol_find_desc(scratch, vol) -> Option<VolumeDesc>`，`vol_kind_of` / `vol_sectors` 均基于它。**S2 多卷与显式格式化**：mfs_srv 参与额外卷挂载 —— 真盘上可有多块 MFS 卷，`mount_extra_volumes(…, VOL_KIND_MFS, …)` 把非主卷挂到 `/usb<卷号>`。与别的服务不同，MFS 的**内存态只有一份**（位图**页窗口** / `MFS_ITAB_MEM` / 快照表 / 各游标对应**一个**卷），故它按请求切卷：请求 tag 高位的卷编码（fd 类请求则由 `MfsFd.vol` 决定）与 `MFS_CUR_VOL` 不同时先 `mfs_switch_vol` —— 设 `MFS_CUR_VOL`/`MFS_CUR_SECTORS` 后 `mfs_load_state()` 把新卷的超级块（含位图）载回内存。**能这样切的前提是每次改动都随即落盘**（`mfs_itab_set` → `mfs_itab_flush` → `mfs_bmp_flush`），故请求边界上盘上状态总是自洽的。`mfs_load_state` 由 `mfs_mount_or_format` 拆出，**只载入、不格式化**（切卷时若格式化，会把一块暂时读不出的盘直接抹掉，那可能是用户唯一的副本）；`mfs_mount_or_format` = 载得动就载、载不动才格式化。新增 VFS tag `MKFS`（`vfs::mfs_mkfs(vol)`，payload = 卷号，**按卷号而非路径寻址**故不经挂载路由直接发给 mfs_srv）：`mfs_mkfs_volume(vol)` 的护栏是**只接受 `VOL_KIND_MFS` 或 `VOL_KIND_UNKNOWN`**，FAT/exFAT/ext2 分区与不存在的卷号一律拒绝；格式化时临时把 `MFS_CUR_VOL`/`MFS_CUR_SECTORS` 指向目标卷（尺寸按它的真实容量算，复用 M7 的几何路径），结束后切回原卷并 `mfs_load_state()` 重建内存态，成功则把非主卷经 `MNTV` 挂到 `/usb<卷号>`。**主卷切换（S2 补齐）**：`MKFS` 成功时还把该卷标记为**主卷** —— 超级块 payload `+256` 的 `MFS_SB_PRIMARY`（u64，0 = 非主卷）记「现有所有 MFS 卷与它自身的最大序号 + 1」（`mfs_next_primary_serial`，按卷号直读超级块、与已冻结的卷表无关），并随这次格式化提交落盘。认领端由 `mfs_vol_claim` 取代通用 `vol_claim`：**序号最大且 > 0** 的 MFS 卷优先 → 否则卷表里第一个 MFS 卷（老卷/序号全为 0，保持旧行为）→ 都没有则回退约定卷号 1。序号只增、不回写别的卷，单主卷由「最大者胜出」保证，于是「最近一次显式格式化过的卷」稳定地就是**下次启动**的 `/mfs`（本次运行不换挂载点）。标记只由显式的 `mkfs.mfs` / `mfs.primary` 设置 —— 首次挂载的自动格式化**不**认领主卷。`MKFS` 的成功回复由裸 `1` 改为**从盘上回读**的序号（>0；失败仍 `u64::MAX`），回读即标记已落盘的证据（FS-24 据此判定，另断言「重复格式化序号严格变大」与「普通写提交后标记不丢」）。**只改标记入口（`MFS_SETPRIMARY_TAG` / `vfs::mfs_set_primary(vol)`）**：`mkfs.mfs` 换主卷会**擦除**目标卷的文件，故另给一条**不动数据**的路 —— `mfs_set_primary_volume(vol)` 先用 `mfs_sb_probe` 确认目标卷**确实是 MFS**（把 `mfs_primary_of_vol` 拆出 `Option<u64>` 以区分「不是 MFS」与「是 MFS 但序号为 0」；「没有格式化兜底」），再走同一个 `mfs_next_primary_serial` 取号，临时切卷 → `mfs_load_state` → 置 `MFS_PRIMARY_SERIAL` → `mfs_bmp_flush` 提交（位图无脏块，实际只写头块 + 两份超级块）→ 切回原卷并重载。shell 命令 `mfs.primary <卷号>`；新增 FS-25（序号严格大于 mkfs 基线 + 文件逐字节一致 + 非 MFS 卷被拒）。`mfs_build_super` 每次提交都会回写该字段，`mfs_load_state` 每次载入都会读回内存态 —— 少任一处，一次普通写盘就会把标记抹成 0。详情见 [roadmap-fs.md](roadmap-fs.md) 阶段 D「M7」「M8」「S3a」「S3b」与「S2 补齐」。
- **ext2_srv（只读）**：ext2 只读兼容，挂载于 `/ext2`，后端为 NVMe 第三 namespace（`build/ext2.img`，宿主 `mke2fs` 预格式化）。**不写盘、也不自动格式化**——超级块无效即挂载失败（定位是「读既有 Linux 分区」）。解析超级块（@1024，magic `0xEF53`）→ 块大小 / 每组块数与 inode 数 / inode 大小；块组描述符表缓存每组 inode 表起始块；inode `block[15]` 的直接 / 一级 / 二级间接块映射（三级不实现）；目录项 `inode/rec_len/name_len/file_type/name` 顺序遍历。只服务 `OPEN/READ/READDIR/STAT/CLOSE`，写类 tag 一律回 `u64::MAX`。名字按 ext2 语义大小写敏感，精确匹配失败后再做一次 ASCII 大小写不敏感回退。**M1b 多卷**：`Ext2Fd` 记 `vol`；服务循环按 tag 高位卷编码切 `EXT2_CUR_VOL`，与 `EXT2_GEO_VOL` 不同则**重新挂载该卷**（重跑 `ext2_mount`：超级块 → 块组描述符 → inode 表缓存）；同时 `EXT2_MAX_GROUPS` 由 16 提到 **4096**（原值只够 16 MiB 镜像，4096 组 × 8192 块 × 4 KiB 覆盖约 32 GiB 卷），故真机 Linux 分区可挂。详情见 [roadmap-fs.md](roadmap-fs.md) 阶段 C3。
- **exfat_srv**：exFAT 兼容（**M6a 只读 + M6b 读写 + M6c 大容量**），挂载于 `/usb`，后端为 NVMe 第五 namespace（`build/exfat.img`，宿主 `mkfs.exfat` 预格式化）。**不自动格式化**，签名 / boot checksum 不符即挂载失败（`exfat: mount FAILED vol=… stage=…`）。解析主引导区（sector 0，备份在 sector 12，各 12 扇区；`EXFAT   ` 签名 + `0x55AA`）→ `SectorsPerClusterShift`/`FatOffset`/`FatLength`/`ClusterHeapOffset`/`ClusterCount`/`FirstClusterOfRootDirectory` → **boot checksum**（sector 11 低 4 字节，逐字节 `sum = rot(sum) + (sum>>1) + byte`，跳过偏移 106/107/112）→ 系统项 `0x81` 分配位图（1 = 占用，位 `cluster-2`）与 `0x82` upcase 表（`exfat_scan_system_entries` 只校验位图**覆盖面** `bitmap_len*8 >= ClusterCount`，内容**按需读扇区**，不整体载入） → 根目录所在簇必须被位图标为占用（兼作位图解析校验）。FAT 链每簇一个 u32（`0` 空闲 / `>= 0xFFFFFFF8` 链尾），`NoFatChain` 置位时按 `first + i` 连续取簇。目录 `entry set` = `0x85` File + `0xC0` Stream Extension + N×`0xC1` File Name（每条 32 字节、每 `0xC1` 承载 15 个 UTF-16 码元，`0x00` = 本簇余下未使用），set checksum 校验完整性（跳过首项字节 2/3），名字 UTF-16 → UTF-8，时间戳换算为 Unix 秒。**写路径（M6b）**：`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`；位图分配/释放（**按需 512 B 扇区窗口**：定位 `cluster-2` 所在扇区，切窗时先回写脏窗）、FAT 表项读-改-写（双 FAT 时镜像）、簇链扩展（连续文件先补齐 FAT 链接转成链式）、entry set 构造含 **NameHash**（用 upcase 表：`hash = hash.rotate_right(1) + upcase(c)`，末尾再转一次）与 SetChecksum；名字 UTF-8 → UTF-16（仅 BMP）。**顺序保证无悬空引用**：创建 = 先备簇与数据、最后写目录项；删除 = 先摘目录项、再释放簇。exFAT 无稀疏文件，`TRUNCATE` 扩展会真实分配并清零。⚠️ **扩容时须把链尾簇的 `0x00` 空位填成非 0**（`0x20`）—— exFAT 要求非 0 项不得出现在 `0x00` 之后，否则宿主 `fsck.exfat` 判卷损坏。**M6c 去上限**：集群缓冲依簇大小动态分配（`exfat_bufs_init(spc_pages)`，簇 ≤ 256 KiB = 64 页，占 `+0x11_4000..0x15_3FFF`）；分配位图与 upcase 表改为**按需 512 B 扇区窗口**（不再整体载入内存）；boot checksum 改为逐扇区累加（不依赖连续两页）。由此 32 KiB/128 KiB 大簇、位图 > 4 KiB 的大容量卷（真机 U 盘）均可挂载。**M6c 边界**：只支持 512 B 扇区（`BytesPerSectorShift == 9`）、簇 ≤ 256 KiB（`SectorsPerClusterShift ≤ 9`）；`rename`/`chmod`/`link` 不支持。**M1b 多卷**：`ExfatFd` 记 `vol`；服务循环按 tag 高位卷编码切 `EXFAT_CUR_VOL`，与 `EXFAT_GEO_VOL` 不同则**重新挂载**（重跑 `exfat_mount`，并把 `EXFAT_ALLOC_HINT` 复位为 2）；集群缓冲经 `exfat_bufs_init(spc_pages)` **只补足差额页**（已分配页数记在 `EXFAT_BUFS_PAGES`，换到簇更小的卷时不必回收也不会重复 `sys_alloc_page`）。详情见 [roadmap-fs.md](roadmap-fs.md) 阶段 D「M6a / M6b / M6c」。
- **block_srv 卷层（分区解析）**：`BlockReq.op = (volume << 8) | opcode`（payload 恰好 32 字节、无空位，故把卷号并进 `op`）。opcode：`0` 读 / `1` 写 / `2` **查询卷表**。启动时扫描各 namespace：有 MBR/GPT 分区表则每个非空分区各成一个卷，否则**整个 namespace 视为一个卷**（向后兼容三张整盘镜像）；再按卷首签名探测类型（`EXFAT   ` / MFS magic `MFS0..MFS9`（低字节为版本号） / ext2 `0xEF53` / FAT `0x55AA`）。**卷容量（M7）**：初始化阶段对每个 namespace 发一次 `Identify Namespace`(CNS=0) 取 **NSZE**（返回数据偏移 0 的 u64 = 扇区数），缓存在 `NVME_NS_SECTORS`（走 Admin 队列，只在 init 发一次）。整盘卷因此能填出**真实 `sectors`** —— 此前该栏恒为 0（「整盘其余部分，未知」），MFS 首次格式化只能退回写死的默认尺寸。`sectors == 0` 仍表示**容量未知**（仅当连盘也问不出容量时），需要容量的上层必须按默认值兜底，**不能把 0 当成零长度卷**。**⚠️ VBR 与 MBR 的区分（接真机盘后补）**：本函数也用于**本身就是分区**的卷（`-drive file=/dev/sdXN`），此时扇区 0 是该分区的 VBR；真实 FAT32 的引导代码正好落在 446..509（MBR 分区表的位置），只判「type != 0 且 count != 0」会把它误读成 4 条主分区项 —— 结果是**真正的文件系统卷永远登记不上**，只登记出 4 个指向非法 LBA 的假卷（读它们直接 `LBA Out of Range`）。故主分区项额外要求 `boot ∈ {0x00, 0x80}` 且 `lba_start != 0`（合法项才可能如此，扇区 0 永远是 MBR 自己）；不满足即当作「无分区表」走整盘卷路径。`mkfs.fat` 造的小镜像该区域恰好全 0，因此模拟镜像测不出这个问题。实际 I/O 用 `(vol.nsid, vol.start_lba + lba)`。**M6c 块层多页 DMA**：单条 NVMe 命令最多 256 扇区（128 KiB）；1 页只用 PRP1、2 页 PRP2 直指第 2 页、> 2 页 PRP2 指向 block_srv 私有 **PRP 表页**（表项为第 2..N 页物理地址，逐页 `SYS_VIRT_TO_PHYS` 反查）；更大的请求由 block_srv 按 256 扇区切段（切段落在页边界，段缓冲天然页对齐）。namespace 列表由 **Identify Controller 的 `NN`（偏移 516）** 推导为 `1..=NN`（不用 Identify CNS=2：实测部分 QEMU 版本返回不完整）。IDE PIO 路径仅登记「整盘一个卷」，其容量由 **ATA IDENTIFY DEVICE**（命令 `0xEC`）现问现取：优先 LBA48（word 100-103，需 word 83 bit10 支持位），否则 LBA28（word 60-61），两者都夹在 **28 位 LBA 上限**（`0x0FFF_FFFF` 扇区 = 128 GiB）内 —— 本驱动的读写命令只发 28 位 LBA，报出更大容量会让上层往读不到的区域写；`IDENTIFY` 失败（无盘 / ABRT / 超时）才算容量未知。此前该栏恒为 0。**卷表打印（S2）**：扫描完（以及 IDE 回退路径）立刻逐卷打印 `vol: <卷号> nsid=… lba=… sectors=… kind=…`（`vol_kind_name` 给 kind 可读名，`unknown` = 无文件系统）—— 卷号由扫描顺序决定，不打出来 `mkfs.mfs <卷号>` 就只能靠猜。**分区表写入（S2 卷管理收口）**：`opcode` 增 `3` 建分区 / `4` 删分区 / `5` 清空分区表 / `6` 重读分区表 / `7` 裸读一扇区，这五个一律**按 `nsid` 寻址**（分区表属于整块盘，卷号只是它的产物），请求复用 `BlockReq` 的 32 字节 payload —— 新增 `PartReq { op, nsid, arg0, arg1 }`，与 `BlockReq` 同为 4 × u64 且字段一一对应，接收端按 opcode 决定读成哪个结构。**表风格自适应**：`part_probe_style` 读 LBA 0 判定「无表 / MBR / GPT」（判据与卷层解析一致：要 `0x55AA` **且**有合法主分区项；保护性 MBR `0xEE` 即 GPT），已有表就沿用其风格，**空白盘默认 GPT**；`flags` bit0 可强制 MBR，但仅限空白盘（已有 GPT 时强制转换会毁表，直接拒绝）。**GPT 写全两份**：保护性 MBR(LBA 0) + 主头(LBA 1) + 主项数组(LBA 2..33) + 备份项数组(盘尾 32 扇区) + 备份头(最后一扇区)；`first_usable = 34`、`last_usable = 容量 - 34`；头写好后**先清零 `header_crc32` 字段再对前 92 字节算 CRC32**（IEEE 多项式，复用 `crc32_update`）；项数组 16 KiB 超过一个共享页，故**按页(8 扇区 = 32 项)流式读-改-写**并边读边累加项数组 CRC32（`mfs_crc32` 已重构成 `!crc32_update(0xFFFF_FFFF, data)`，行为不变）。**分区起点对齐 1 MiB**(2048 扇区)；GPT 类型 GUID 取通用的「Linux 文件系统数据」、MBR 类型字节 `0x83`（建分区时还不知道要 format 成什么，卷层探测靠卷首签名）。**GUID 确定性派生**（`part_mix` 混淆「盘, 角色」）而非随机，回归才可复现。**只动表、不动数据**：删分区只清条目，删到最后一个时整张表清零（盘回到「无分区表」；`part.wipe` 同理清 GPT 头与两份项数组），一块盘因此能在 GPT / 空白 / MBR 之间来回折腾。**改动后立即重扫全部 namespace 重建卷表**并打印（`create` 还会把新分区翻成卷号回给调用方）；⚠️ 卷号由扫描顺序决定，动靠前的盘会让后面卷号整体后移，而已挂载服务仍记着启动时的旧卷号 —— 分区操作应限于最后一类盘，或改完重启。`BLOCK_OP_DISK_READ` 按 nsid 裸读一扇区是**诊断/自测**用的旁路（建完分区后 LBA 0/1 已不属于任何卷，自测得靠它独立校验写进去的字节，而不是只信同一份代码扫出来的卷表）。⚠️ **IDE 回退路径未实现分区写入**（它只有一块整盘、也没有 nsid 概念），这些 opcode 在那边一律回 0。shell 侧是 `part.create` / `part.del` / `part.wipe` / `part.reload`（需 `Capability::SendTo(block_srv)`，不需要共享页）。
- **卷「认领」与额外卷上报（M1b）**：文件服务启动时查卷表认领自己的卷号 —— fat32 → 第一个 FAT 卷；ext2 → 第一个 ext2 卷；exfat → 第一个 `EXFAT   ` 卷；mfs → **主卷序号最大的 MFS 卷**（`mfs_vol_claim`，序号 = 超级块 `MFS_SB_PRIMARY`；都没有则第一个 MFS 卷，再没有则回退约定卷号 1 —— 空白盘无 magic）。因此现有 `nvme.img`/`mfs.img`/`ext2.img` 的卷号恒为 0/1/2，行为与引入卷层前一致。认领完默认卷后调 `mount_extra_volumes(scratch, kind, primary, domain)`：遍历卷表，把**同类且非默认**的卷经 `MNTV` 上报给 mount_srv（自动挂 `/usb<卷号>`），于是「插入第二块 FAT32/ext2/exFAT 盘」无需改任何代码即可访问。**MFS 自 S2 起也参与**（真盘上可有多块 MFS 卷），但**不做自动格式化**：只挂卷层已探测为 MFS 的额外卷，空白卷必须显式 `mkfs.mfs`。⚠️ 四个服务（fat32 / ext2 / exFAT / **mfs**）都需要 `Capability::SendTo(mount_srv)` —— 缺这条授权时 `ipc::call` 被内核**静默拒绝**（返回 `u64::MAX`，不报错），额外卷会挂不上且无任何日志。
- **readdir 条目协议 `DirEntry`**（[user/libmorion/src/vfs.rs](../../user/libmorion/src/vfs.rs)，168 字节）：`name [u8;11]`（8.3 短名，无短名概念的文件系统也填截断等价形式供回退）+ `long_len u8` + `long [u8;128]`（长名，0 = 无长名）+ `size u32` + `is_dir u32` + `mode u16` / `owner u16` / `nlink u32` / `mtime u64`（**MFS5 起的节点元数据**，MFS6 的 `readdir` 会对每个条目先把它存的 ino 经 inode 表翻成块号再读元数据；非 MFS 服务填默认值：权限 0755/0644、属主 0、链接数 1、时间 0）。`ls -l` 因此只需一次 `readdir`。结果页只有一页，故按 `RESULT_MAX_ENTRIES`（页大小 / 条目大小 = 24）截断，避免越界写。
- **stat 协议 `Stat`**（[user/libmorion/src/vfs.rs](../../user/libmorion/src/vfs.rs)，40 字节）：`size u32` / `is_dir u32` / `mode u16` / `owner u16` / `nlink u32` / `mtime u64` / `ctime u64` / `atime u64`。`mode` 的高 4 位是**节点类型**（`vfs::MODE_FTYPE_*`，与 ext2 `i_mode` 的 `S_IFMT` 同构，`ls -l`/`stat` 靠它显示 `d`/`l`/`-`），低 12 位是权限位；非 MFS 服务不填类型位，客户端按 `is_dir` 回退。请求用 `PathReq { aux, buf }`：**路径写在调用方共享页**里（payload 装不下"路径 + 结果页地址"），结果也写入同一页 —— 这样 shell 与 app 各自的结果页都能用（此前 `STAT` 硬编码写 `RESULT_BUF`，只有 app 能用）。`RENM` 用 `TwoPathReq { a_len, b_len, buf }`，页内布局 `src\0dst`；`SYML` 也用 `TwoPathReq`，页内布局 `目标\0链接自身`。**`LSTA`（lstat）** 与 `STAT` 共用同一个 `PathReq` 与结果结构，唯一区别是用不跟随式解析 —— 因此悬空链接的 `lstat` 仍能返回 `mode = 0xA000`（链接）与 `size` = 目标串长度。**`RDLK`（readlink）** 同样用 `PathReq`，但结果不是 `Stat` 而是一段**目标字节串**（无 NUL、写回 `PathReq.buf`），回复值为字节数；作用在非链接上时返回 `u64::MAX`。
- 每个客户端把结果/写缓冲页用 `SYS_SHARE_PAGE` 共享给**所有**它可能访问的文件服务域：app 的 `RESULT_BUF`/`WRITE_BUF`、shell 的 `SHELL_RESULT_BUF` 均共享给域 6 / 域 10 / 域 11 / 域 12 / 域 13。
- **共享缓冲地址约定**：共享页必须位于程序镜像之外的固定虚拟地址（同地址共享，目标域自身的镜像会占住同地址）。已用区间：fat32 `+0x10_0000..0x10_4000`、app/shell 共享缓冲 `+0x10_4000..0x10_8000`、mfs 块缓冲 `+0x10_8000..0x10_C000`、ext2 块缓冲 `+0x10_C000..0x10_10000`、**mfs 的 GC 遍历 / inode 表块缓存 / 索引块 scratch / GC 表块缓冲 `+0x11_0000..0x11_4000`**（M5b 新增后三页）、**exfat 集群缓冲 `+0x11_4000..0x15_3FFF`（按簇大小最多 64 页）+ 位图窗口 `+0x15_4000` + upcase 窗口 `+0x15_5000` + 单页暂存 `+0x15_6000`**、**block_srv 自有卷扫描页 `+0x16_0000` + PRP 表页 `+0x16_1000`**（不共享给任何域）、**fat32 整簇缓冲 `+0x20_0000..+0x21_0000`（M1b：16 页 = 64 KiB，`dir_buf`/`file_buf` 都别名到它）**。**易错点**：新增块缓冲页时必须同时 `sys_alloc_page` + `sys_share_page(.., BLOCK_DOMAIN)` —— 漏了共享，block_srv 拿到的是目标域里未映射的地址，NVMe 会直接回「非法字段」而写入静默失败。**另一个易错点**：固定地址分区不可重叠 —— 同一地址对同一域重复 `sys_share_page`（或撞上别人已占的区间）会触发内核 `KERNEL PANIC: map_user_page: PageAlreadyMapped`，故新缓冲区必须从上述空闲区间里挑、并确认没有第二个域声明同址。

### 图形服务 gfx_srv（G1）

`user/srv/src/gfx_srv.rs`（域 15）把**屏幕**从内核搬到用户态：

- **内核侧交出屏幕**：新增无参能力 `Fb` + 三个 syscall —— `SYS_FB_INFO`（取几何）、`SYS_FB_MAP`（把整块帧缓冲映射进本域）、`SYS_FB_TAKEOVER`（宣告接管）。内核 `video` 加 `FB_TAKEN_OVER` 标志；置位后内核**不再画帧缓冲** —— `print` / `print_logo` / `clear_screen` 直接跳过，`redraw` 变成空操作，`print` 仍写 COM1。于是 headless 回归的串口日志不受影响，屏幕也不再被内核改写。（G4 前这里还提过「输入编辑照旧」，那一整套输入机件现已删除，见 G4。）
- **映射**：`SYS_FB_MAP` 复用 `paging::map_mmio` 的 4 KiB 非缓存页（D2：先用 `NO_CACHE` 跑通），映射前**先整段查重**（目标区间已映射则整体拒绝，不半途映射、也不撞 `PageAlreadyMapped`）。
- **gfx_srv 启动序**：`SYS_FB_INFO` → `SYS_FB_MAP`（映射到 `USER_SPACE_BASE + 1 GiB`，远离镜像/共享缓冲/用户栈）→ **`probe` 探测映射可写**（只写 `y = 0` 那一行并回读）→ `SYS_FB_TAKEOVER` → 独占后 `paint` 整屏（底色 + 居中色块）+ `verify` 回读校验 → `Term::clear` + 写横幅，打印 `gfx: 1280x800 text console ready (kernel console detached)`。
  ⚠️ **顺序不能倒**：接管前内核还在整幅重绘日志（它的文本区自 `y = MARGIN` 起），若先整屏绘制再校验，内核一次重绘就把被校验的像素擦成背景渐变 → **假失败**（实测踩到过：`gfx: framebuffer readback FAILED`，随后 shell 拿不到控制台）。故探测点只取内核不碰的顶部带，整屏校验放到接管之后（那时屏幕只有一个写者）。
- **后续**：绘制原语 / 共享面 / 文本渲染 / 输入外移见 [roadmap-gfx.md](roadmap-gfx.md) 的 G2 / G3 / G4；surface 合成见 G5。

**G2 — 绘制原语 + 共享表面**：

- 协议在客户端库 [`user/libmorion/src/gfx.rs`](../../user/libmorion/src/gfx.rs)：`GfxReq { op, x, y, w, h, color, buf, stride }`（8×u64，塞进 96 字节 payload）+ `GFX_TAG` + `GFX_OP_FILL / RECT / BLIT / PING`。服务端 [gfx_srv.rs](../../user/srv/src/gfx_srv.rs) 用 `sys_recv_msg` 收、`sys_reply` 回。
- 客户端表面：`SYS_ALLOC_PAGE` + `SYS_SHARE_PAGE` **同址**共享给 `gfx_srv`（域 15）。地址按域 id 错开 —— `SURFACE_BASE = USER_BASE + 64 MiB`、步长 `0x40_0000`（4 MiB）、单面上限 256 页（1 MiB）；否则多客户端会把表面共享到服务域同一个 VA 而撞 `PageAlreadyMapped`。
- **`GFX_OP_BLIT` 端到端校验**：服务拷完后**回读帧缓冲**抽 5 个点与表面比对，全等才回 `1`。这是无显示器环境下"真的画上去了"的判据。服务读表面前用 `SYS_VIRT_TO_PHYS` 确认首尾像素在本域已映射（客户端得先共享过来）。
- 能力：客户端需 `SendTo(gfx_srv)`（发请求）+ `MapInto(gfx_srv)`（共享表面）；引导期为 app / shell 各授予这两张。
- 自测 **GS-1**（app）：`fill_screen` + `screen_rect` → 160×120 表面画四条竖直色带 → 客户端回读 → `blit` 到 (16,16) → `app: GS1 gfx primitives + shared surface OK (blit verified on framebuffer)`。

**G3a — 服务内终端 + 文本渲染**：

- **字库归用户态**：`gfx/` 模块（[mod.rs](../../user/srv/src/gfx/mod.rs) 里的 `Fb` 帧缓冲视图、[font.rs](../../user/srv/src/gfx/font.rs) ASCII 8×16 字模表、[glyphs.rs](../../user/srv/src/gfx/glyphs.rs) UTF-8 解码 + 字形二分查找 + 显示宽度、[term.rs](../../user/srv/src/gfx/term.rs) 终端状态）加上 `cjk.bin`（≈276 KB，`git mv` 进服务）。内核 `unicode.rs` 暂时保留但 `include_bytes!` 已改指用户态那份（一份数据，无副本漂移），G3c 删除。
- **分工**：`glyphs::bit(cp, row, col)` 只回答「亮不亮」，`term` 按**显示列**排版（宽度来自字库记录；ASCII 1 格、汉字/全角 2 格）、管自动换行/滚动/清屏/`\r`/`\t`。
- **落笔即校验**：每画一个字符，整格逐像素「算颜色 → 写入 → 读回比对」，不一致即回 0。
- **协议**：`GFX_OP_TEXT`（4，`buf` 文本页 + `w` 字节数，上限 4096）、`GFX_OP_CLEAR`（5）、`GFX_OP_MOVE`（6，`x`=列 `y`=行，越界拒）、`GFX_OP_QUERY`（7，回 `行<<32 | 列`）。**文本走共享页**（本域窗口 +1 MiB 处一页；窗口 = `USER_BASE + 64 MiB` 起、按域 id 每域 4 MiB），不塞 IPC payload。
- 客户端 API：`morion::gfx::{print, clear_screen, move_cursor, cursor}`。
- 自测 **GT-1**（app）：`clear_screen` → `TEXT("GT-1 ")` 断言列 = 5 → `TEXT("汉字宽字符")` 断言列 = 15（5 汉字 × 2 列）→ `move_cursor(0,20)` 再写 → 越界 `move_cursor(9999,9999)` 必被拒 → `app: GT1 text console OK (layout cols verified, framebuffer pixel readback matched)`。

**G3b — shell 输出上屏**：

- 内核加 `SYS_CONSOLE_READY(47)`（无能力门禁，读 `video::FB_TAKEN_OVER`）：客户端不必用一次 `SYS_CALL` 去白等图形服务。
- `libmorion::syscall` 把打印出口收成 `sink()`（`SYS_PUTS` → 内核终端 + COM1，**可选**再镜像一份给 `gfx::print`），开关是 `screen_mirror_on()`（`static`，按进程 opt-in）。shell 打开它；app/其它服务不打开 —— 自测里成千上万条打印不该每条多一次 IPC 往返。
- shell 启动：有界等待屏幕控制台（`CONSOLE_WAIT_MS = 1000`，每 1 ms 查一次 `SYS_CONSOLE_READY`；到点就退回只写串口）→ 确认 `SYS_CONSOLE_READY` 且 `sys_domain_alive(15)`（镜像走 `SYS_CALL`，目标没有活任务会一直等回复）→ `screen_mirror_on()`。
- 盲测自证：开镜像后用 `GFX_OP_QUERY` 问光标，`row > 0` 打 `shell: screen console mirror OK (gfx_srv cursor advanced)`（走 `sys_puts`，只进串口，不占屏幕）。

**G4 — 输入搬出内核（行编辑外移）**：

- **内核只剩搬运**：新增 `SYS_KEY_PUSH(48)` / `SYS_KEY_READ(49)` 与 [`kernel/src/key.rs`](../../kernel/src/key.rs)（64 字节环形队列；满则丢新键，绝不阻塞内核）。`kbd_srv` 只把 scancode 译成字节推进去（可打印字符 / 退格 `0x08` / 回车 `'\n'`，方向键丢弃），**不再有任何编辑语义**。队列非空时 `key::push` 会 `wake_one(KEY_WAIT)`（原名 `INPUT_WAIT`）。
- **删除的输入机件**：`term_put` / `term_backspace` / `term_left` / `term_right` / `scroll_view_up/down` / `input_read` / 输入行队列 / `INPUT_BASE`（行内提示符）/ 输入输出隔离（`input_detach`/`input_reattach`/`IN_SAVE_*`）/ 跨行累积 `IN_ACCUM` / 光标 `CURSOR_X/Y` 与 `set_cursor` / 历史区光标导航（`CUR_ROW`/`CUR_COL`/`SCROLL_OFFSET`）—— 连带 15..20 与 27 号 syscall 一起退役。`video` 从此只有「历史 + 一行当前输出」。
- **行编辑在客户端库**：[`user/libmorion/src/console.rs`](../../user/libmorion/src/console.rs) 的 `readline(buf)` —— 阻塞取键、可打印字符追加并**立刻回显**（走 `print` ⇒ 串口 + 可选屏幕镜像，所以打字看得见了）、退格发 `\b`、回车成行。
  ⚠️ **为什么不做成服务端功能**：`gfx_srv` 是单线程服务，一旦阻塞在「等按键」就出不了请求循环，别的客户端（app 自测的 `GFX_OP_FILL`/`GFX_OP_TEXT`）会被饿到有人按键为止。`SYS_KEY_READ` 在**客户端**阻塞则完全免费（内核把任务睡下，有键再唤醒）。
- **屏幕侧配合**：`Term::write` 支持 `\b`（光标左移一列 + 把该格涂成背景色，走同一条逐像素回读路径），行编辑器的退格因此能擦屏。键盘输入目前全是 ASCII，故一次退格 = 一列。
- shell：`morion::console::readline` 取代 `sys_readline`；`clear` 命令改清**屏幕控制台**（`gfx::clear_screen`）。控制台不可用时 shell 如实报错退出（没有回显通道 = 没有输入源）。
- **运行期证据**（QEMU monitor `sendkey` 无头实测）：注入 `h e l p ⏎` → 串口出现 `help` 回显 + 完整命令列表；注入 `echo hiz` + 退格 + `⏎` → 串口出现 `echo hiz\x08` + `hi`，证明「取键 → 回显 → 退格删缓冲并擦屏 → 回车提交」整条链路。

**G6 — 图形服务自愈（监督重启 + 客户端会话重建）**：

- **问题**：`gfx_srv` 原不在 init 监督集里 —— 它一崩，屏幕永久死掉；更糟的是 `ipc::call` 的调用方把请求塞进它邮箱后就**无限等回复**，服务没了就**永久挂死**（shell 的打印镜像首当其冲）。
- **内核侧两处**：① `ipc::call` 改为**带超时轮询**（`CALL_POLL_MS = 200`）并在目标域**无存活任务**时立即失败返回（`u64::MAX`）—— 客户端不再永久挂起；② 帧缓冲登记为**内核保留区间**（`frame_allocator::pin_range`），`domain::reset` 清地址空间时不会把它当普通帧释放。另外「同域重启」现在会**丢弃目标域邮箱里未处理的请求**（`restart_in_place` 里加 `ipc::remove_domain`），否则新实例会去处理那些引用已失效共享页的旧请求而再次崩。
- **客户端会话重建**（[`user/libmorion/src/gfx.rs`](../../user/libmorion/src/gfx.rs)）：服务重启会清空它域内的页表，之前 `SYS_SHARE_PAGE` 共享过去的文本页/表面**随之消失**。客户端库因此：`call` 收到 `u64::MAX` 时把会话标记失效；`ensure_shared` 重建时**只重发 `share`、不再 `alloc`**（页仍在本域，用 `SYS_VIRT_TO_PHYS` 判断），并在重发前**有界等待服务活过来**（必须等 `reset` 之后重发才安全，否则撞 `map_user_page` panic）；若服务端对悬空共享页回 `GFX_REPLY_NO_SESSION(2)`（发生在"重启落在两次调用之间、本域还没察觉"时，即 shell 的常态），`print`/`blit` 会**重建共享再试一次**。
- **gfx_srv 侧**：`GFX_OP_EXIT(8)` 自测钩子（**先回复再退出**，否则请求方等不到回复）；`text`/`blit` 在发现共享页不在本域映射时回 `GFX_REPLY_NO_SESSION`。
- **init**：把 `gfx_srv(15)` 纳入 `SUPERVISED`（见上）。
- **自测 GS-2**（app，跑在 GS-1/GT-1 之后）：杀 `gfx_srv` 两轮 —— ① 空窗期不调用：靠 `NO_SESSION` + 重建共享恢复；② 空窗期调用：`ipc::call` 必须快速失败（不挂起），重启后又能用 → `app: GS2 gfx_srv restart + client session rebuild OK (screen recovered)`。
- **GT-1 的并发修正**：屏幕是**多客户端共享**的（shell 也镜像打印、共用一条光标），GT-1 的"清屏 → 写 → 问光标"可能被 shell 的启动输出插队，测得列数偏大。改为**重试到干净窗口**（shell 打完启动输出就阻塞等输入），不引入新协议。

### 架构（[kernel/src/arch/](../../kernel/src/arch/)）

- `gdt::init()` / `gdt::set_rsp0(stack_top: u64)`
- `idt::init()`（异常 0..31 + PIC 时钟 32 / 键盘 33 + **MSI 向量段 0x50..0x5F**）
- `pic::init()` / `pic::send_eoi()`
- `pit::init()`（100 Hz 定时器）
- `keyboard::read_scancode()`
- `apic::init() -> Option<u32>` / `apic::msi_address() -> u32` / `apic::eoi()`（LAPIC 最小支撑，见下）
- `pci::enumerate()` / `pci::find_nvme()` / `pci::find_net()` / `pci::read_bar(index)` / `pci::read_bar0()` / `pci::find_msix()` / `pci::disable_intx()` / `pci::enable_msix()`
- `device::grant(GrantRequest) -> bool` / `device::grant_empty(domain)` / `device::enable_msix()` / `device::config_read(offset) -> Option<u32>`（**D1 通用设备授权**：BAR 映射 + DMA 块分配 + MSI-X 向量段分配 + `Mmio`/`Irq` 能力签发 + 写 `DeviceGrant` 描述；内核**不含**任何设备专属逻辑 —— 原 `nvme::setup` 已并入。`config_read` 是 N2 加的窄接口：把"域→设备"绑定后只放行读自己那台设备的配置空间）

## 7. 构建 / 测试命令（Makefile）

| 命令 | 说明 |
| --- | --- |
| `make kernel` | 仅构建微内核 |
| `make user` | 仅构建用户态服务 → `build/user/srv/*.elf`（15 份） |
| `make hello` | 仅构建可执行文件加载演示程序 → `build/user/hello.elf` |
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
- **E2b 起每个服务是独立程序**：`user/srv`（crate `morion-srv`）里一个服务一个 `[[bin]]`，各模块用 `#[cfg(feature = "svc-<name>")]` 门控 —— 一个 bin 只编自己的服务模块 + `common`。入口 `_start(domain_id)`（libmorion 提供）调各 bin 的 `morion_main`，后者打印 `[up] <name> (domain N)` 后进 `morion_srv::<mod>::run()`。
- 构建链：`cargo build --target user/x86_64-morion-user.json --package morion-srv --release -Z json-target-spec` → `build/user/srv/<name>.elf`（15 份，各自一份 ELF）。
- 服务 ELF 的**载体**（E3b 起）：引导器从**自己所在的 ESP**（`efiboot.img`）的 `\EFI\morion\services\<name>.elf` 读入内存 —— 故 Makefile 在生成 ESP 时 `mcopy` 这 15 份进去；内核镜像里**不再有服务副本**（内核体积因此从 ~686 KiB 降到 ~337 KiB）。
- **同一批 ELF 还要进 FAT32 根盘**（E3c）：`nvme.img` 的 `/system/services/*.elf` 是监督者 `init` 的**盘上重启源**（E3c 后续起，重启**优先**走引导模块内存镜像，失败才回退它）。两个载体同源（都取自 `build/user/srv/`），任何一处落后都会让"回退重启"拿到旧镜像。
- 读取方式：优先 `BootServices::get_image_file_system`（"本映像所在的卷"）；El Torito 光盘引导下这条链若解析不出来，退化为枚举所有 `SimpleFileSystem` 卷、用"能否读出第一个服务 ELF"判定。读文件走**裸 `SimpleFileSystem` 协议**（不用 `uefi::fs::FileSystem`：其 `read` 是 `vec![0; file_size]`，异种卷返回离谱大小时会 `capacity overflow` panic），并设 8 MiB 上限。
- ⚠️ **引导器的全局分配器必须先 `uefi::allocator::init(&mut st)`**：uefi 的 `Allocator` 靠内部静态 `SYSTEM_TABLE` 找 BootServices，`#[entry]` 不代为登记；不初始化就在第一次 `Vec`/`String` 分配时崩（症状是 `#UD`，且引导器默认 panic 处理器只 spin、看不到任何输出）。`exit_boot_services` 前调 `uefi::allocator::exit_boot_services()`。

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
| 38 | **block_srv 写分区表（S2 卷管理收口）**：opcode `3..7` = 建/删/清空/重读分区表 + 裸读一扇区，一律**按 nsid 寻址**（新增 `PartReq`，与 `BlockReq` 同尺寸）；表风格按盘自适应（空白盘默认 **GPT**，可 `mbr` 强制且仅限空白盘）；GPT 写全「保护性 MBR + 主头/主项数组 + 备份项数组/备份头」并把头与项数组 CRC32 算对（项数组 16 KiB 按页流式处理）；起点 1 MiB 对齐、GUID 确定性派生；**只动表不动数据**，删到最后一个就整表清空；改动后立即重扫重建卷表。shell 加 `part.create/del/wipe/reload`（shell → block_srv `SendTo`）；新增 `build/pt.img`（nsid 7，64 MiB）+ **FS-26**（含宿主 `sgdisk -v` 跨实现校验） | ✅ |
| 39 | **NVMe 中断化（MSI/MSI-X）**：内核新增 LAPIC 最小支撑（`arch/apic.rs`：`IA32_APIC_BASE`/`SVR`/`TPR`/`LVT0`-ExtINT 透传/`EOI`）、PCI 能力链表遍历与 MSI-X 定位（`pci::find_msix`/`disable_intx`/`enable_msix`）、MSI 向量段 `0x50..0x5F` 的 IDT 处理器（`eoi` + 置待处理位）、向量注册与 `SYS_IRQ_POLL`/`SYS_MSIX_ENABLE`；分工 = 内核管中断配置（LAPIC + PCI 配置空间 + 向量段），驱动写 MSI-X 表（该 BAR 由固件分配在 4 GiB 以上，内核到不了，且本就非缓存映射给驱动）。`nvme::setup` 在内核侧准备向量 + 授权 `Irq`，驱动写表项 0 → 请内核开 MSI-X → 注册向量 → `submit_wait` 改「先等中断再查 CQE」（`create_iocq` 补 **IEN=1**，否则 I/O CQ 根本不投中断），等不到则**粘性回退轮询**；启动打 `MSI-X prepared/enabled` 与 `after volume scan cmds=/irq_cmds=/poll_cmds=/irqs=` 两路证据，运行期每 4096 条命令再打一行 | ✅ |
| 40 | **阻塞等中断（等待原语）**：调度器加带超时阻塞（TCB `wake_deadline` + `block_current_timeout_ms`，`tick()` 到期唤醒 `Sleeping` 与带超时 `Blocked` 两态）与伪等待键 `irq_wait_token`（当时按向量取键，**S5 起改为按域取键**，见第 41 行）；`irq::set_pending` 置位后 `wake_one` 唤醒等待该向量的域（取完锁再进调度器，不形成锁嵌套）；新 syscall `SYS_IRQ_WAIT(36)`（阻塞等向量中断，超时返回 0）；空闲任务改 `hlt(); yield_now();`，让被中断唤醒的域立刻接手而不必等一个时钟 tick。驱动 `submit_wait` 的中断路径改为「`SYS_IRQ_POLL` 快路径 → 未命中 `SYS_IRQ_WAIT` 阻塞」，**彻底去掉前一轮的每轮踢宿主自旋**，等不到中断仍是轮数 × 超时的看门狗后粘性回退。实测：中断路径 `irq_cmds=28672 poll_cmds=0` 零回退、自测 **314 s**（旧「每轮踢」实现 342 s，同轮轮询对照 174 s —— 这套 QEMU/KVM 下中断等待每条命令仍多约半个 tick） | ✅ |
| 41 | **多向量 + `wait_any`（中断/等待原语做深）**：等待原语从「按向量取键」改为「**按域取键 + 向量掩码**」—— 调度器 `irq_wait_token(domain) = u64::MAX-0x300-domain`（落点 `[u64::MAX-0x3FF, u64::MAX-0x300]`，与真实域 id / `INPUT_WAIT` 不重叠），`irq` 侧加 `ANY_MASK: [u64; 64]` 记「每个域正在等的向量掩码」。`SYS_IRQ_POLL(34)` / `SYS_IRQ_WAIT(36)` 的入参 `rdi` 从「向量号」改为**掩码**（位 `i` ↔ 向量 `idt::MSI_VECTOR_BASE + i`），返回**命中的向量号**（0 = 无 / 超时 / 非法）；掩码里每个位都须持有 `Capability::Irq` 且是该向量的注册者，一个不满足即整体非法。`take_pending_any(mask, domain)` 只取自己注册的位，`set_pending` 置位后算出「掩码含该向量」的域、放锁后逐个 `wake_one(irq_wait_token(domain))`。NVMe 侧：`NVME_MSIX_VECTORS = 3`、`NVME_DMA_PAGES = 5 → 7`，建 **admin + 2 条 I/O 队列**，每条 CQ 用**自己的**向量（0x50/0x51/0x52），驱动按段**轮转选队列**、I/O 完成等 `1<<IO_QUEUES` 掩码（`wait_any`）。⚠️ 踩到的坑:`Create I/O CQ` 的 `CDW11` 里 **IV 必须等于完成队列下标**（admin CQ 恒 0），写成「队列序号」会让 qid 1 与 admin 抢向量 0 —— 症状是 I/O 完成投的是 0x50 而驱动在等掩码位 1/2，永远等不到、13 条命令后即回退轮询（`vecs=0x1`）。实测三条向量都真实投递：`after volume scan cmds=29 irq_cmds=29 poll_cmds=0 irqs=29 vecs=0x7 mode=irq`、运行期 `cmds=8192 irq_cmds=8192 poll_cmds=0 irqs=8192 vecs=0x7 mode=irq` | ✅ |
| 42 | **终端中文 / 非 ASCII 点阵渲染**：内核终端原只有 8x16 ASCII 位图，`SYS_PUTS` 拿到的是 UTF-8，汉字（3 字节）被逐字节喂进 `draw_char` 后落在「不可打印」分支被丢掉 —— 中文直接不显示。新增 `video/unicode.rs`（UTF-8 解码 + 二分查字形 + 绘制 + 豆腐块）与生成的字库 `video/cjk.bin`（**GNU Unifont**，OFL-1.1：GB2312 全集 ∪ 仓库非 ASCII 字符 ≈ 7500 字 / 276 KB，定长 37 字节记录、**宽度随字形存**，故内核不必维护 East Asian Width 表）。终端行模型从「字节 = 一列」改为**按显示列**：`append_cp` 按列数判满行、渲染逐字符推进 8/16 px、光标按字节下标折算显示位置且下划线宽度取字符宽度、退格/←/→ 走 `prev_index`/`next_index` 不切开多字节字符。验证：`screendump` 截图确认内核启动行与 Ring 3 shell 行都正确显示汉字与全角标点、宽窄混排对齐（**内核侧字库已于第 54 行 G3c 卸除** —— 字库与汉字渲染现在归用户态 `gfx_srv`） | ✅ |
| 43 | **可执行文件加载（ELF + 运行时 spawn）**（**E1**）：此前所有域跑的是同一份编译期嵌入的扁平二进制（`load_user_program` 拷到 `USER_BASE`，用户态 `_start(domain_id)` 按域分流），既跑不了用户编的程序，也没有"每程序独立地址空间"。新增内核 **ELF64 加载器**（`elf.rs`：magic/`ELFCLASS64`/`ET_EXEC`/`EM_X86_64`/`phentsize=56`/`phnum≤32`、每段 `filesz≤memsz`+文件不越界+段落在 `[USER_SPACE_BASE, USER_SPACE_END)`、**入口必须落在已载入段内**；不分配资源、不 panic）与 `exec.rs`（建域 + 逐段映射 + 栈 + 起任务），新 syscall `SYS_SPAWN_ELF(37)`（`Capability::Spawn` 门禁，返回新域 id；镜像字节来自用户态故校验全在核内，并按页确认缓冲**已映射**）。配套地基修正：`MAX_TASKS` 16→32 + `HEAP_SIZE` 1→4 MiB（每任务 32 KiB 内核栈来自内核堆）、`spawn` 满表改 `try_spawn_user` 返回 false（运行时用户可触发路径不 panic）、**`Domain::new` 显式跳过 P4[1]**（否则运行时建域会与调用者共用用户空间页表 → 无隔离且 `PageAlreadyMapped`）、`USER_STACK_TOP/PAGES` 提到 `paging` 作唯一来源。演示程序 `user/hello` 是**独立 crate/独立 ELF**，自测 FS-27 把它写进 `/tmp` 再从**文件**读回加载。实测：314 s、零失败，`FS27 exec loaded 5568 bytes -> domain 14` + 子程序 `exec: … 我的域 = 14, 入口 = 0x8000000000`（入口正是其链接地址）；内核新增 4 个 ELF 解析单测（`cargo test --lib -p morion-kernel` 全过） | ✅ |
| 44 | **用户态运行库 libmorion + `run` 命令**（**E2a**）：抽出 **libmorion**（`user/libmorion`，crate 名 `morion`）—— `syscall`（syscall 封装 + 打印 + `domain_id()`）与 `vfs`（libvfs）从 `morion-user` 移入库，库另提供入口样板 `_start(domain_id)`（放 `.text._start`，`ENTRY(_start)`）+ `#[panic_handler]`，程序只实现 `morion_main(domain_id)`（crt0 把域 id 交给它，返回即退出）—— `morion-user` 与 `morion-hello` 都依赖它，`hello` 从"自带 syscall 桩"瘦成 20 行主逻辑。新增 `exec::spawn_file(path)`：`vfs::open` → 分块读进本域内存 → `SYS_SPAWN_ELF`；**经一页中转**而不是把每个暂存页都共享出去（否则几百 KB 的程序要占几十个共享帧槽位，内核只有 64 个），且对同一 (页, 域) 只 `share_page` 一次、`alloc_page` 前先用 `sys_virt_to_phys` 判已映射 —— 两处漏了都会在**第二次** `run` 时撞 `PageAlreadyMapped` panic。**跨进程那处也漏过**：中转页地址原先所有客户端共用一个固定 vaddr，而 `SYS_SHARE_PAGE` 是映射进目标服务域的**同一地址** —— app 自测的 FS-27 共享给 fat32_srv 之后，shell 再 `run` 同一张盘就在服务域同址撞 `PageAlreadyMapped`（表现为跑完 `SELFTEST DONE` 后 `run` 必崩）。现按**调用方域 id 错开**中转页地址（与 `RESULT_BUF`/`SHELL_RESULT_BUF` 同一做法）。shell 新增 **`run <file>`** 命令（并补 `Capability::Spawn`），把"可执行文件加载"从自测里的证据变成**用户可见的功能**；FS-27 自测改为从**磁盘文件** `/hello.mex` 走同一条 `spawn_file` 路径（不再内嵌镜像、不再经 tmpfs 运输）。构建：`make hello`；`$(NVME_IMG)` 依赖 `$(HELLO_ELF)` 并 `mcopy` 成 `::/hello.mex`（回归脚本也会就地注入，免去"先 make 一遍镜像"的隐含前提）。实测：交互 `run /hello.mex` → `run: loaded /hello.mex -> new domain 14` 且子程序打印 `exec: … 我的域 = 14, 入口 = 0x8000000000`；全量回归零失败 | ✅ |
| 45 | **域销毁 / 退出即回收 + 服务拆成独立程序**（**E2b**，三步）：**① 域销毁（地基）** —— 域表改 `Vec<Option<Domain>>` + `slot_for` **复用空槽**（域 id 是各全局表下标，单调增长会越界 `irq::ANY_MASK`），`domain::destroy` 顺序 = 摘域表槽位 → `paging::free_user_space`（遍历 P4[1] 逐页归还 + 回收页表帧）→ 清 `cap/ipc/pager/irq` 按域行 → `scheduler::remove_domain`（摘任务 + 唤醒等它的域）；帧记账规则 = `frame_allocator::release_user_frame`（登记过引用计数的按计数递减、**未登记**的镜像页/栈帧/页表帧视为独占直接归还）；新 syscall `SYS_DOMAIN_DESTROY(38)`（门禁 `Spawn` 且 `pager::of(target)==调用者`）/`SYS_DOMAIN_COUNT(39)`/`SYS_FRAME_FREE(40)`。**② 退出即回收** —— `exit_current` 若为本域最后一个任务则 `domain::request_destroy` **只登记**（不能就地拆自己正在用的栈/页表），由时钟 `tick` 开头 `reclaim_pending` 在别的任务上下文销毁；引导期服务域走白名单（`is_boot`/`BOOT_DOMAINS=14`）永不自动销毁。**③ 服务拆成独立程序** —— 新建 `user/srv`（crate `morion-srv`）：14 个服务各一个 `[[bin]]` → **各一份独立 ELF**，各模块 `#[cfg(feature="svc-<name>")]` 门控（一个 bin 只编自己的服务 + `common`），删掉 17814 行单文件 `user/src/main.rs` 与 `morion-user`；内核改 `SERVICE_ELFS` 表 + `exec::spawn_elf_at(domain, image)`（不建域/不登记全局表）逐个载入**各自固定域**，删 `load_user_program`/`USER_PROGRAM`；每个程序入口打印 `[up] <name> (domain N)`。验证：内核单测 12 项全过；FS-28 `exit-reclaim OK (domain 14 reused 8x, frames stable)`；启动 `[OK] 14 service ELFs loaded (embedded)` + 14 行 `[up]`；全量回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds==cmds` 且 `poll_cmds=0`、宿主 `sgdisk -v` "No problems found" | ✅ |
| 46 | **服务生命周期收口 ①：用户页 W^X**（**E3a**）：E1 起就记着的一笔 —— `map_user_page` 原先没有权限参数，所有用户页都可写、可执行。本轮：`elf::parse` 解析 `p_flags` 并**拒绝 `PF_W\|PF_X` 的段**（镜像侧）；`paging` 引入 `UserPagePerm { ReadOnly, ReadWrite, ReadExecute }`，`map_user_page` 带权限参数（`ReadOnly`/`ReadWrite` 置 `NO_EXECUTE`，仅 `ReadExecute` 可执行且绝不置 `WRITABLE`），`paging::init` 开 **`EFER.NXE`**（CPUID 无 NX 时告警并跳过）；`exec::map_image` 改**两遍**（先按「页权限并集」建映射、再拷内容，避免事后改页表项），并集为 W+X 的页**拒绝加载**（不静默降级成 RWX）；`user/linker.ld` 在 `.data` 前 `ALIGN(4096)` —— 否则 `.text/.rodata`(RX) 与 `.data/.bss`(RW) 会落在**同一页**（实测改前 `sender.elf` 三段全挤在 `0x…000..0x7d8`，页级 W^X 不可能满足）。顺带补上 W^X 引入的新失败模式：用户态 `P=1` 保护违例（写只读页 / 执行 NX 页）**终止该任务**，而不是转给分页器（那样会让它去映射一个**已映射**的页 → 内核 `PageAlreadyMapped` panic）。验证：内核单测 15 项（新增 W^X 位不变式、三种权限的期望位、W+X 段被拒）；全量回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds==cmds` 且 `poll_cmds=0`、宿主 `sgdisk -v` 无问题 | ✅ |
| 47 | **服务生命周期收口 ②：服务移出内核镜像（引导模块）**（**E3b**）：14 份服务 ELF 原先由 `include_bytes!` 嵌进内核（`SERVICE_ELFS`），内核体积随服务数线性膨胀。改为：**引导器**在 `exit_boot_services` 之前用 UEFI 文件系统从**自己所在的 ESP**（`\EFI\morion\services\<name>.elf`）读入镜像，各拷进 `LOADER_DATA` 页（内核帧分配器只放行 `CONVENTIONAL`，故这些帧天然被保留），再把 `ServiceModule { domain, addr, len }` 表经**扩展的 `BootInfo`**（`version 2 → 3`，新增 `svc_addr/svc_count/svc_entry_size`）交给内核；内核删掉 `SERVICE_ELFS`，改遍历 `BootInfo::service_modules()` 并逐个 `exec::spawn_elf_at`（以物理地址给出的镜像先过 `paging::is_identity_mapped`）。构建：内核不再依赖服务 ELF，改由 `iso` 依赖 `$(SRV_STAMP)` 并把 14 份 `mcopy` 进 ESP。**内核 ELF 702200 → 345376 字节（−51%）**。踩到并修掉两个"引导器第一次读文件/分配内存"才会暴露的坑：① 引导器的全局分配器**必须显式 `uefi::allocator::init(&mut st)`**（`#[entry]` 不代为登记；不初始化就在第一次 `Vec`/`String` 分配时 `#UD`）；② `uefi::fs::FileSystem::read` 的 `vec![0; file_size]` 会在异种卷（ISO9660）上 `capacity overflow` panic → 改走**裸 `SimpleFileSystem` 协议**读取 + 8 MiB 上限。顺带把引导期进度与失败原因镜像到 **COM1**（原先只画帧缓冲，headless 下失败表现为"日志停在 BdsDxe"，无从定位）。验证：`[boot] service modules loaded: 14` + `[OK] 14 service ELFs loaded (boot modules)` + `[up]` × 14；全量回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds==cmds` 且 `poll_cmds=0`、宿主 `sgdisk -v` "No problems found" | ✅ |
| 49 | **服务生命周期收口 ④：重启不依赖盘**（**E3c 后续**）：新增 `SYS_SPAWN_ELF_MODULE(43)`（`rdi=域号`）—— 与 `SYS_SPAWN_ELF_AT` 共用同一套「验镜像 → 目标域无存活任务 → `domain::reset` → `reap_terminated` → 起任务」流程（抽成 `restart_in_place`），区别只在镜像来源：内核按域号去 `bootinfo::get().service_modules()`（E3b 交来的 `LOADER_DATA` 镜像）里取，过 `is_identity_mapped` 后映射，**不依赖磁盘**。init 重启**先试内存镜像、失败回退盘**并打印来源；监督集 6 → 9（新增 `pager / fat32_srv / mfs_srv`），于是「读盘要靠文件服务、文件服务死了没法自救」的鸡生蛋问题不复存在；唯一排除 `block_srv`（`domain::reset` 会释放内核为它映射的 NVMe 配置/DMA 帧、甚至把 BAR0 的 MMIO 地址当 RAM 交出）。回归：`init: restarted echo (domain 3, total 1, from memory)` | ✅ |
| 50 | **图形子系统 G1：帧缓冲用户态化**：把**屏幕**从内核搬到用户态服务 `gfx_srv`（域 15）—— `domain::create()` 增域 + `BOOT_DOMAINS` 15 → 16 + `cap/ipc/pager::init` 16 + `SERVICE_FILES`/`SRV_NAMES` 加 `gfx_srv`；新增无参能力 `Fb`（`CAP_KIND_FB=5`）与三个 syscall：`SYS_FB_INFO(44)`（写回 `FbInfo { addr, width, height, stride, bpp }`）/`SYS_FB_MAP(45)`（把整块帧缓冲按 4 KiB 非缓存页映射进本域，**先整段查重**再映射，避免半途映射与 `PageAlreadyMapped`）/`SYS_FB_TAKEOVER(46)`（宣告接管）。内核 `video` 加 `FB_TAKEN_OVER` 原子标志，置位后内核**不再画帧缓冲**：`print`/`print_logo`/`clear_screen` 直接跳过、`redraw`/`redraw_input_line` 成空操作，`print` 仍写 COM1（headless 回归不受影响）；**输入编辑照旧**（`term_put` 等只跳过重绘，见第 53 行）；`gfx_srv` 取几何 → 映射到 `USER_SPACE_BASE + 1 GiB` → 画测试图案（全屏底色 + 居中色块）→ **回读校验**（四角/中心像素等于写入值）→ 接管 → 重画。顺带把 init 巡检改成**两连击去抖**（连续两轮确认无任务才重启）—— G1 让打印与内存重启都变快，E3c 的 20 ms 轮询方（FS-29）会错过极短的死亡窗口，去抖让死亡状态至少持续一个巡检周期。回归：`gfx: framebuffer 1280x800 stride=1280 phys=0x80000000` + `gfx: 1280x800 text console ready (kernel console detached)` | ✅ |
| 51 | **图形子系统 G2：绘制原语 + 共享表面 + `libmorion::gfx`**：`gfx_srv` 加请求循环（`sys_recv_msg`/`sys_reply`）与三个原语 `GFX_OP_FILL`/`GFX_OP_RECT`/`GFX_OP_BLIT`（+ `GFX_OP_PING`），循环**在接管之后**才起，故客户端首条请求必然落在屏幕已归用户态之后；新增客户端库 [`user/libmorion/src/gfx.rs`](../../user/libmorion/src/gfx.rs)（协议 `GfxReq`/`GFX_TAG`/`GFX_OP_*` + `Surface` 分配/共享/像素/`rect`/`blit` + 屏幕级 `fill_screen`/`screen_rect`/`ping`）。表面按域 id 错开（`SURFACE_BASE = USER_BASE + 64 MiB`，步长 4 MiB，上限 1 MiB），避免多客户端把表面共享到服务域同一 VA 撞 `PageAlreadyMapped`。**关键取证**：`GFX_OP_BLIT` 拷完**回读帧缓冲**抽 5 点与表面比对，全等才回 1 —— 无显示器也能断言"真画上去了"。引导期给 app/shell 授 `SendTo(gfx_srv)` + `MapInto(gfx_srv)`。自测 **GS-1**（app）：`fill_screen`+`screen_rect` → 160×120 表面四条竖直色带 → 客户端回读 → `blit` 到 (16,16) → `app: GS1 gfx primitives + shared surface OK (blit verified on framebuffer)`。**顺带修掉一个静默坑**：app 的 16 个能力槽被新增的 `SendTo/MapInto(gfx_srv)` 占满，导致其后的 `SendTo(echo)` 授权静默失败（FS-29 报 `send quit to echo FAILED`）—— `CAP_SLOTS` 16 → 32，且 `grant` 槽满时打印 `[WARN] capability table full: grant dropped`，不再无声丢授权 | ✅ |
| 52 | **图形子系统 G3a：服务内终端 + 文本渲染外移**：字库归用户态 —— `user/srv/src/gfx/` 新增 `mod.rs`（`Fb` 视图）/`font.rs`（ASCII 8×16 字模表）/`glyphs.rs`（UTF-8 解码 + 字形二分查找 + 显示宽度）/`term.rs`（按**显示列**排版：光标、换行、滚动、清屏、`\r`/`\t`），`cjk.bin`（≈276 KB）`git mv` 进服务（内核 `unicode.rs` 暂留但改指用户态那份，G3c 删）。`gfx_srv` 接管后清屏用服务内终端写横幅。**落笔即校验**：每字符整格逐像素「算色 → 写 → 读回比对」，不一致回 0。新协议 `GFX_OP_TEXT(4)`/`CLEAR(5)`/`MOVE(6)`/`QUERY(7)`（文本经**共享页**传 —— 本域窗口 +1 MiB 一页，不塞 payload；`MOVE` 越界拒而不夹取）；客户端 `morion::gfx::{print, clear_screen, move_cursor, cursor}`。自测 **GT-1**：`TEXT("GT-1 ")` 断言光标列 = 5、`TEXT("汉字宽字符")` 断言列 = 15（5×2 列）→ `app: GT1 text console OK (layout cols verified, framebuffer pixel readback matched)` | ✅ |
| 53 | **图形子系统 G3b：shell 输出上屏**：内核加 `SYS_CONSOLE_READY(47)`（无能力门禁，读 `video::FB_TAKEN_OVER`）—— 客户端不必用一次 `SYS_CALL` 白等图形服务。`libmorion::syscall` 把打印唯一出口收成 `sink()`（`SYS_PUTS` → 内核终端 + COM1，**可选**镜像给 `gfx::print`），开关 `screen_mirror_on()` 按进程 opt-in（只有 shell 打开；自测成千上万条打印不该每条多一次 IPC 往返）。shell 启动有界等待控制台（`CONSOLE_WAIT_MS = 1000`，1 ms 一次查 `SYS_CONSOLE_READY`，到点退回只写串口）→ 确认 `SYS_CONSOLE_READY` 且 `sys_domain_alive(15)`（镜像走 `SYS_CALL`，目标没活任务会一直等回复）→ 开镜像；随后用 `GFX_OP_QUERY` 问光标、`row > 0` 打串口行 `shell: screen console mirror OK (gfx_srv cursor advanced)`（盲测自证）。**接管只停重绘、不停输入**：`term_put`（含回车提交）/退格/左右移/↑↓ 照常改状态并唤醒 `SYS_READLINE` —— 曾经这些函数在接管后一律早退，回车不再提交输入行，shell 永远收不到命令（表现为"终端卡死"），后来收成「只让 `redraw`/`redraw_input_line` 空操作」才对。已用 QEMU monitor `sendkey` 无头实测：注入 `h e l p ⏎` → 串口出完整命令列表；注入带退格的 `echo hi` → 出 `hi`。已知限制归 G4：输入行回显仍在**内核**终端，屏幕上打字看不见；屏幕控制台无行级保护 | ✅ |
| 54 | **图形子系统 G3c：内核卸 CJK 字库（−276 KB）**：内核侧 `cjk.bin` 的 `include_bytes!`（275983 字节）、定长 37 字节记录的二分查表（`Glyph`/`glyph`）与「有字形就按字形宽度排版」那条路径全部删除。留下接管前那几秒 + panic 屏够用的最小集：UTF-8 `decode`/`prev_index`/`next_index` **原样保留**（退格与 ←/→ 不切开多字节字符）、`width`/`str_width` 改按**东亚宽度**粗判（ASCII 1 格、汉字类 2 格、控制字符 0 格，与用户态 `gfx_srv` 口径一致，接管前后列数不突变）、`draw` 非 ASCII 画**空心豆腐块**（仍按 2 格占位）。**内核 ELF 347376 → 70504 字节（−276872，≈ −80%）**，`cjk.bin` 只剩 `user/srv/src/gfx/cjk.bin` 一份。代价（已知且接受）：`gfx_srv` 接管前的几秒与 panic 屏上中文是豆腐块；**COM1 全程原样 UTF-8**（`print` 不经过本模块），headless 回归判据不变。顺带补 4 条 host 单测锁住宽度口径 / 混排列数 / 字符边界 / 截断序列（内核单测 16 → 20） | ✅ |
| 55 | **图形子系统 G4：输入搬出内核（行编辑外移）**：内核侧删光输入机件 —— `term_put`/`term_backspace`/`term_left`/`term_right`/`scroll_view_up/down`/`input_read`/输入行队列/`INPUT_BASE`（行内提示符）/输入输出隔离（`input_detach`+`input_reattach`+`IN_SAVE_*`）/跨行累积 `IN_ACCUM`/`CURSOR_X,Y`+`set_cursor`/历史区光标导航（`CUR_ROW`/`CUR_COL`/`SCROLL_OFFSET`），`scheduler::is_waiting_on` 也一并删除；`video` 从此只有「512 行历史环 + 一行当前输出」，`print` 在接管后只写 COM1（内核终端只剩引导期日志与 panic 屏）。新增 [`key.rs`](../../kernel/src/key.rs)（64 字节环形键队列，满则丢新键）+ `SYS_KEY_PUSH(48)` / `SYS_KEY_READ(49)`（阻塞取键，`wake_one(KEY_WAIT)`，原名 `INPUT_WAIT`）；15..20 与 27 号 syscall **退役不再分配**。`kbd_srv` 只做 scancode→字节（可打印 / 退格 `0x08` / 回车 `'\n'`，方向键丢弃）；行编辑与回显落在 [`morion::console::readline`](../../user/libmorion/src/console.rs)（内核只搬字节；放客户端是因为 `gfx_srv` 单线程，服务端阻塞等键会饿死 app 自测的绘图请求）；`Term::write` 支持 `\b`（左移一列 + 涂背景，走逐像素回读）供退格擦屏；shell 的 `clear` 改清屏幕控制台。**顺带修掉一个真实竞态**：`gfx_srv` 原先「整屏绘制 → 回读校验 → 接管」，而接管前内核仍在整幅重绘，校验点会被擦成背景渐变 → 假失败（实测 `gfx: framebuffer readback FAILED`，随即 shell 报无控制台）；改为「顶部带 `probe` 探测 → 接管 → 独占后 `paint`+`verify`」。内核 ELF 70504 → 66040 字节。运行期证据（QEMU monitor `sendkey`）：`help⏎` → 回显 + 完整命令列表；`echo hiz`+退格+`⏎` → `echo hiz\x08` + `hi` | ✅ |
| 56 | **图形自测提前（g3b-reorder）**：把 app 自测的图形取证 **GS-1 / GT-1** 从 `run()` 的**末尾**（在 ~6 分钟的 NVMe FS 压测之后）挪到**最前**（共享结果页之后、第 1 条 FS 用例之前）—— 图形是唯一"看屏幕"的取证，提前跑让每次回归/开发都能**立刻**拿到图形结果，不必干等 FS 全套。纯重排，两处仍是"失败即 `return`"的语义（图形回归在开头 fail-fast，不被 FS 结果拖到最后）；`gfx_srv`（域 15）后启动，首个 `ipc::call` 会等到它接管并进入请求循环，无需自旋。顺带修掉 [`morion::console::readline`](../../user/libmorion/src/console.rs) 里一处 `clippy::collapsible_match`（内层 `if len < buf.len()` 折进 match guard），`make clippy -- -D warnings` 恢复全绿。验证：GS-1/GT-1 现出现在日志第 ~142 / ~144 行（shell 自证之后、FS 压测之前），且 `SELFTEST DONE` 与 `FAILED`/`PANIC` 0 不变 | ✅ |
| 57 | **图形子系统 G6：图形服务自愈（监督重启 + 客户端会话重建）**：修掉「`gfx_srv` 一崩，shell 的 ipc 永久挂死、屏幕永久死」这个真实缺口。**内核三处**：① `ipc::call` 由无限阻塞改为**带超时轮询**（`CALL_POLL_MS = 200`）+ 目标域**无存活任务**时立即失败返回 `u64::MAX`（调用方阻塞在**自己的域**键上，服务死在回复前就再没人回它，是挂死根因）；② 帧缓冲登记为**内核保留区间**（`frame_allocator::pin_range`，`free_frame` 空操作）—— `SYS_FB_MAP` 走 `map_mmio` 不计引用计数，「同域重启」清地址空间时 `free_user_space` 会把它当独占帧释放（大内存下把显存交回分配器）；③ 「原地重启」新增**丢弃目标域邮箱里未处理的请求**（`restart_in_place` 加 `ipc::remove_domain`），否则新实例会去读旧请求里的悬空共享页而再崩。**客户端**（[`morion::gfx`](../../user/libmorion/src/gfx.rs)）：服务重启会清空它域内页表，共享过去的文本页/表面随之消失 —— `call` 收 `u64::MAX` 时作废会话；`ensure_shared` 重建**只重发 `share` 不再 `alloc`**、且先**有界等待服务活过来**（`reset` 之后重发才安全）；服务端对悬空共享页回新码 `GFX_REPLY_NO_SESSION(2)`，`print`/`blit` 据此**重建共享再试一次**（覆盖"重启落在两次调用之间、客户端没察觉"，shell 的常态）。**服务**：`GFX_OP_EXIT(8)` 自测钩子（**先回复再退出**），`text`/`blit` 对不在本域映射的共享页回 `NO_SESSION`。**init**：`gfx_srv(15)` 纳入 `SUPERVISED`（10 个）。**顺带**：GT-1 的列数断言原依赖**全局光标**，被 shell 的启动输出插队会偏大 → 改为**重试到干净窗口**（屏幕多客户端共享，不引入原子返回列数的协议）。**自测 GS-2**：杀 `gfx_srv` 两轮（① 空窗期不调用走 `NO_SESSION` 重建；② 空窗期调用走 `ipc::call` 快速失败），日志 `init: restarted gfx_srv (domain 15, total N, from memory)` ×2 + `app: GS2 gfx_srv restart + client session rebuild OK (screen recovered)`。回归：`SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds == 28672` 且 `poll_cmds = 0`、宿主 `sgdisk -v` "No problems found" | ✅ |
| 58 | **驱动通用化 D1：通用设备授权**：把内核里 **NVMe 专属**的 bring-up 抽成**通用**原语，让"加新驱动不必改内核"。新增 [`kernel/src/device.rs`](../../kernel/src/device.rs)（**取代 `kernel/src/nvme.rs`**）：`DeviceGrant` 描述结构（BAR + 连续 DMA 块 + MSI-X 参数，**不含**设备语义）+ `GrantRequest` + `grant()`（映射 BAR/DMA/描述页、签发 `Mmio` 能力）+ `grant_empty()` 降级 + `enable_msix()`；**MSI 向量段改为按设备分配**（原来每台设备都从段首拿固定几条，只够一台 NVMe —— 现游标 `MSI_NEXT` 从 `idt::MSI_VECTOR_BASE` 递增、段尽则降级轮询）。`main.rs` 里 NVMe 只剩一条**声明式需求**（`bar_pages: 4 / dma_pages: 7 / msix_vectors: 3`）；`SYS_MSIX_ENABLE` 转调 `device::enable_msix()`。驱动侧 [`block_srv.rs`](../../user/srv/src/block_srv.rs)：读通用描述 + **自行推导队列布局**（`DMA_OFF_*` 页偏移，ASQ/ACQ/ISQ1/ICQ1/ISQ2/ICQ2/data）—— 设备专属知识搬回驱动域。**纯重构，行为零变化**：回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds=28672 poll_cmds=0`（MSI 向量段仍 `0x50..0x52`）、`[OK] 16 service ELFs loaded`、`sgdisk` No problems found | ✅ |
| 59 | **驱动路线 N0：域表扩容 + `net_srv` 骨架**：为网络驱动腾出域号 —— `domain::BOOT_DOMAINS` 16 → **17**、内核 `main.rs` 多建一个域（16）、引导器 `SERVICE_FILES` 加 `(16, "net_srv")`、`Makefile` 的 `SRV_NAMES` 加 `net_srv`、`morion-srv` 新增 `net_srv` feature/`[[bin]]`、`init` 监督集加 `(16, "net_srv")`（11 个）。内核 `ipc::init`/`cap::init`/`pager::init` 由**字面量 16 改为 `domain::BOOT_DOMAINS`** —— 这些表按域 id 下标访问，扩域后若表长不同步会**越界 panic**。`net_srv` 本步只是骨架（报到 + `sys_sleep` 保活），N2 才填 virtio-net 驱动。**验证**：启动日志 `[OK] 17 service ELFs loaded (boot modules)`、`[up] net_srv (domain 16)`、`init: supervising …/net_srv`；全量回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds=28672 poll_cmds=0`、`sgdisk` No problems found（运行时 spawn 的域号随之 16 → 17，FS-28 用动态基线故无需改） | ✅ |
| 60 | **驱动路线 N1：PCI 通用查找 + 网卡设备声明**：`arch/pci.rs` 把 `read_bar0` 抽成通用 `read_bar(index)`（`0..=5`）并新增按类查找 `find_net`（网络控制器 class `0x02` + vendor `1AF4` + device `1041`/`1000` → 返回 **BAR4**，即 virtio-modern 的 common/notify/device/isr 配置区）。**修一个真坑**：高于 4 GiB 的 64 位 BAR（virtio-net 的 BAR4 = `0xC000000000`）低 dword 只剩标志位，若拿低 dword 判"未实现"会把它误判为不存在 → 改为先拼出完整 64 位地址再判 0。`main.rs` 给域 16 加一条**声明式需求**（与 NVMe 同款 `device::grant`）`bar_pages: 4 / dma_pages: 8 / msix_vectors: 2 / label: "net"`；`net_srv` 读 `DeviceGrant` 打印取证。QEMU（`make run-nvme` 与 `scripts/fs-regress.sh`）加 `-netdev user,id=n0 -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56`。**已知缺口（→ N2）**：virtio-net 的 **MSI-X 表在 BAR1**（与设备 BAR 不同根），D1 的 `grant` 目前只映射一根 BAR 且要求 `table_bir == 0` → 现**降级轮询**（日志 `net: MSI-X table in BAR1, not BAR0 -> polling`）。**验证**：`[OK] virtio-net modern BAR4=0x000000C000000000`、`net: device grant bar_vaddr=0x8000820000 … dma_bytes=32768`；全量回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`[OK] 17 service ELFs loaded`、`irq_cmds=28672 poll_cmds=0`、`sgdisk` No problems found | ✅ |
| 61 | **驱动路线 N2a：`net_srv` virtio-net bring-up + 轮询取帧**：新增内核窄接口 **`SYS_DEVICE_CONFIG_READ(50)`** + `device::config_read`：内核按 `domain → bus/dev/func` 绑定表（`grant` 时登记）只放行"读**自己那台**设备"的配置空间 dword，驱动据此**自行解析** virtio PCI 能力（common/notify/ISR/device 四个区域都在内核交给的 BAR 里；notify 另读 `notify_off_multiplier`），内核因此**不必懂 virtio 协议**。[`net_srv`](../../user/srv/src/net_srv.rs)：复位（写 0 等确认）→ `ACKNOWLEDGE|DRIVER` → 读设备特性并只接子集（必须 `VIRTIO_F_VERSION_1`，另取 `VIRTIO_NET_F_MAC`）→ `FEATURES_OK`（回读校验）→ 读 device cfg 取 **MAC** → 建 RX(0)/TX(1) virtqueue（desc/avail/used 三环排在内核交出的**连续 DMA 块**里，深度 8，环地址写**物理**地址）→ 投满 RX 缓冲 → `DRIVER_OK` → 轮询 used 环取帧 + 补投 + 统计。**取证**：`net: virtio-net up MAC=563412005452 num_queues=3 rx=8 tx=8`、`net: DRIVER_OK, RX buffers posted`，且回归里**真的收到帧**（`net: rx frames=1`）；全量回归不退化（`SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`17 service ELFs`、`irq_cmds=28672 poll_cmds=0`、`sgdisk` No problems found）。**缺口（→ N2b）**：MSI-X 表在 BAR1，尚未中断化（现降级轮询） | ✅ |

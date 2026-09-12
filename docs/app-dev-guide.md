# Morion OS 应用开发指南

> 面向**用户态应用 / GUI 开发者**的接口规范与开发手册。
> 本文档只讲「应用怎么用系统」，不涉及内核内部实现；内核内部速查见 [dev-reference.md](dev-reference.md)。
>
> 系统当前处于早期阶段，接口会持续演进。本文档与 [user/src/syscall.rs](../user/src/syscall.rs) 同步维护，
> 新增/修改系统调用时请同步更新本文档，避免后续开发 GUI 时到处翻 API。

---

## 1. 开发模型概览

Morion OS 是微内核 + 能力系统架构：

- **应用 = 一个保护域（Domain）里的用户态程序**，运行在 Ring 3，拥有独立地址空间。
- 应用通过**系统调用（syscall）**与内核交互，通过 **IPC** 与其它域（服务）通信。
- 所有跨域操作都需要**能力（Capability）**授权，默认「无能力即不可访问」。

```
应用(域) ── syscall ──► 内核(微内核)
   │                        │
   └── IPC(需 SendTo) ──────┴──► 服务域(文件服务/驱动服务/...)
```

应用编写者看到的是 libuser（当前为 [user/src/syscall.rs](../user/src/syscall.rs)）提供的一组封装函数，
未来会由 libc/libvfs 进一步封装成 POSIX 风格接口。

---

## 2. 用户程序结构

用户程序在 `user/` crate 内，编译为**扁平二进制**，由内核在启动时按页加载到用户空间基址。

```rust
#![no_std]
#![no_main]

mod syscall;
use syscall::*;

/// 入口: 内核经 rdi 传入本程序所属的域 id。
/// 须置于镜像最前端 (offset 0)。
#[link_section = ".text._start"]
#[no_mangle]
pub extern "C" fn _start(domain_id: u64) -> ! {
    match domain_id {
        0 => app_a_main(),
        1 => app_b_main(),
        _ => {}
    }
    sys_exit();
}

fn app_a_main() { /* ... */ }

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys_exit();
}
```

要点：

- 入口函数固定名为 `_start(domain_id: u64) -> !`，通过 `domain_id` 分流到不同角色。
- `panic_handler` 只能 `sys_exit()`（用户态无法恢复）。
- 链接地址为 `USER_SPACE_BASE = 0x0000_0080_0000_0000`，常简写为 10 位 hex `0x8000_0000_00`（见 [user/linker.ld](../user/linker.ld)）。
- 用户态虚拟地址必须落在 P4[1] 的 canonical 区间：约 `0x8000_0000_00 ~ 0xFF_FFFF_FF_FF`（**10 位 hex**）。
  若误写成 12 位 hex（如 `0x9000_0000_0000`）会让 bit47=1，成为非 canonical 地址，访问时触发 #GP 而非 #PF，
  进而 double fault。分配内存 / 未来 mmap 帧缓冲时务必注意。

### 构建命令

```bash
make user      # 仅构建用户程序 → build/user/user.bin
make kernel    # 内核编译期 include_bytes! 嵌入 user.bin
make iso       # 完整镜像
```

### 当前域角色分配（[kernel/src/main.rs](../kernel/src/main.rs)）

| 域 id | 角色 | 说明 |
| --- | --- | --- |
| 0 | sender | 演示共享内存 + 按需分页 + 同步调用 |
| 1 | receiver | 接收 IPC 通知、读共享页 |
| 2 | pager | 用户态分页器（服务缺页） |
| 3 | echo | 同步 IPC 服务（recv → reply 回显） |
| 4 | kbd | 用户态键盘驱动（注册 IRQ1） |
| 5 | block_srv | 块设备服务（NVMe / IDE PIO） |
| 6 | fat32_srv | FAT32 文件服务 |
| 7 | app | 测试应用（libvfs 读文件 + FS 自测） |
| 8 | shell | 命令行解释器（`SYS_READLINE` 阻塞读行 + libvfs） |
| 9 | mount_srv | 挂载服务（路径前缀 → 文件服务域；libvfs 据此路由） |
| 10 | tmpfs_srv | 内存文件系统（挂载于 `/tmp`） |
| 11 | mfs_srv | MorionFS 原创文件系统（挂载于 `/mfs`；块设备后端 + COW + 快照） |

> 新增一个应用/域：需在 `kernel/src/main.rs` 里 `domain::create()` → `cap::grant(..)` 授权 →
> `load_user_program(..)` → `scheduler::spawn_user(..)`，并在 `user/src/main.rs` 的 `_start` 里加对应分支。
> 目前是手工接线，后续会由「进程管理器」服务统一创建。

---

## 3. 系统调用接口

ABI：编号在 `rax`，参数在 `rdi/rsi/rdx`，返回值在 `rax`。用户态一律通过 [syscall.rs](../user/src/syscall.rs) 的封装调用。

### 3.1 进程控制

| 编号 | 封装 | 参数 | 返回 | 说明 |
| --- | --- | --- | --- | --- |
| 0 | `sys_yield()` | — | — | 主动让出 CPU |
| 1 | `sys_sleep(ms)` | `rdi=ms` | — | 睡眠毫秒 |
| 5 | `sys_exit()` | — | 不返回 | 终止当前用户任务 |

### 3.2 IPC

| 编号 | 封装 | 参数 | 返回 | 前置能力 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 2 | `sys_send(to, tag)` | `rdi=to, rsi=tag` | 1/0 | `SendTo(to)` | 非阻塞发送；失败(无能力/邮箱满)返回 0 |
| 3 | `sys_recv()` | — | `tag` | — | 阻塞接收，返回消息 `tag` |
| 3 | `sys_recv_msg(buf)` | `rdi=buf` | `tag` | — | 阻塞接收，把完整 `Message` 写入 `buf` |
| 12 | `sys_call(to, tag)` | `rdi=to, rsi=tag` | 回复 `tag` | `SendTo(to)` | 同步调用：发送请求并阻塞等回复，失败返回 `u64::MAX` |
| 13 | `sys_reply(tag)` | `rdi=tag` | 1/0 | — | 回复最近一次 `sys_recv` 到的调用者 |

### 3.3 内存

| 编号 | 封装 | 参数 | 返回 | 前置能力 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 6 | `sys_alloc_page(vaddr)` | `rdi=vaddr` | 1/0 | — | 分配一物理帧映射到本域 `vaddr` |
| 7 | `sys_share_page(vaddr, to)` | `rdi=vaddr, rsi=to` | 1/0 | `MapInto(to)` | 把本域 `vaddr` 的页映射进 `to` 域同地址 |
| 8 | `sys_unmap(vaddr)` | `rdi=vaddr` | 1/0 | — | 解除本域 `vaddr` 映射，引用计数归零时释放帧 |
| 9 | `sys_map_anon(domain, vaddr)` | `rdi=domain, rsi=vaddr` | 1/0 | `MapInto(domain)` | 分页器专用：给 `domain` 映射匿名零帧 |
| 10 | `sys_page_fault_reply()` | — | 1/0 | — | 分页器专用：唤醒缺页域 |

### 3.4 能力 / 中断

| 编号 | 封装 | 参数 | 返回 | 前置能力 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 14 | `sys_register_irq(irq)` | `rdi=irq` | 1/0 | `Irq(irq)` | 注册本域接收 `irq`（用户态设备驱动） |
| 21 | `sys_map_mmio(bar, vaddr)` | `rdi=bar, rsi=vaddr` | 1/0 | `Mmio(bar)` | 把物理 MMIO 页（`bar`，页对齐）映射到本域 `vaddr`（非缓存） |

### 3.5 终端 / 视频（文本）

| 编号 | 封装 | 参数 | 返回 | 说明 |
| --- | --- | --- | --- | --- |
| 4 | `sys_puts(s)` | `rdi=ptr, rsi=len` | — | 打印字符串 |
| 15 | `sys_scroll_up()` | — | 1 | 光标上移 / 到顶滚动历史 |
| 16 | `sys_scroll_down()` | — | 1 | 光标下移 / 到底滚动历史 |
| 17 | `sys_backspace()` | — | 1 | 删除输入行光标前一个字符 |
| 18 | `sys_term_put(ch)` | `rdi=ch` | 1 | 在输入行光标处插入字符（`ch=0x0A` 提交当前行） |
| 19 | `sys_term_left()` | — | 1 | 光标左移 |
| 20 | `sys_term_right()` | — | 1 | 光标右移 |
| 28 | `sys_clear()` | — | 1 | 清屏并复位终端状态（历史 / 输入行 / 光标） |

> 打印辅助函数（基于 `sys_puts`）：`print(s)`、`println(s)`、`print_u64(v)`、`print_hex(v)`、`flush()`。
>
> `print` 为**行缓冲**（换行或缓冲满才提交）；`flush()` 立即提交未以换行结束的内容，用于**行内提示符**。
>
> ⚠️ 当前**没有用户态 framebuffer 访问接口**（图形输出后续补充）。GUI 开发前需把 GOP 帧缓冲
> 以 MMIO 方式映射进用户域；通用 MMIO 映射能力 `sys_map_mmio`（编号 21，`Capability::Mmio`）已就绪，
> 见 [roadmap-fs.md](roadmap-fs.md)。

### 3.6 控制台输入

| 编号 | 封装 | 参数 | 返回 | 说明 |
| --- | --- | --- | --- | --- |
| 27 | `sys_readline(buf)` | `rdi=buf ptr, rsi=len` | 行长度 | 阻塞读取一行控制台输入到 `buf`（最多 `len` 字节，不含换行）；无输入时阻塞，直到键盘回车提交一行（由键盘域经 `SYS_TERM_PUT` 驱动）。失败返回 `u64::MAX` |

> 提示符支持**行内显示**（与用户键入内容处于同一行，形如正常终端 `[morion@morion <cwd>]$ `）：
> 内核记录本轮用户输入起点（第一个按键时锁定为当时行尾），`sys_readline` **只返回用户输入部分**，
> 不含此前打印的提示符；退格/左移也不会越过输入起点（不会删掉提示符）。因此 shell 用
> `print(...)` 打印提示符后调用 `flush()` 即可，无需 `println`。

---

## 4. IPC 编程

### 4.1 消息布局

与内核 `ipc::Message` 布局一致（`#[repr(C)]`）：

```rust
#[repr(C)]
struct Message {
    from: u64,          // 发送者域 id
    to: u64,            // 目标域 id
    tag: u64,           // 消息标签（请求号/事件号，业务自定义）
    payload: [u8; 32],  // 固定 32 字节载荷
}
```

- `PAYLOAD_LEN = 32`，每域邮箱容量 `MAILBOX_CAP = 16`（超出则 `send` 失败）。
- **大块数据（文件内容、图像）不能走 payload**，应使用共享内存（见第 5 节）传递地址，payload 只放元数据。

### 4.2 服务模式（以 echo 服务为例）

服务端（域 3）：

```rust
fn echo_main() {
    loop {
        let tag = sys_recv();      // 阻塞等待请求
        sys_reply(tag + 1);        // 回复调用者
    }
}
```

客户端（域 0）：

```rust
let reply = sys_call(3, 0xABCD);   // 同步调用，阻塞到回复
// reply == 0xABCE
```

### 4.3 设备驱动模式（以键盘驱动为例）

驱动是一个用户态域，先注册 IRQ，再循环 `sys_recv` 收中断消息：

```rust
fn kbd_main() {
    if sys_register_irq(1) != 1 { return; }   // 需持有 Irq(1) 能力
    loop {
        let sc = sys_recv() as u8;            // 中断数据即 scancode (作为 tag)
        // 解码 scancode → 字符 / 控制
    }
}
```

「中断即 IPC」：硬件 IRQ → 内核 `irq::dispatch` → 驱动域邮箱 → 驱动 `sys_recv` 取到。

---

## 5. 共享内存

文件内容、图像帧等大数据用共享内存传递，避免 IPC 32 字节载荷限制。

发送方：

```rust
let page = 0x8000_0030_00u64;
if sys_alloc_page(page) == 1 {
    // 写入数据到 page
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), page as *mut u8, len); }
}
sys_share_page(page, receiver_domain);  // 需 MapInto(receiver_domain)
sys_send(receiver_domain, NOTIFY_TAG);  // 通知接收方
```

接收方：

```rust
let tag = sys_recv();
let page = 0x8000_0030_00u64;           // 共享页映射到同一地址
let bytes = unsafe { core::slice::from_raw_parts(page as *const u8, 12) };
```

约定：共享页映射到**双方约定的同一虚拟地址**，发送方通知后接收方直接读。

---

## 6. 能力模型

| 能力 | 含义 |
| --- | --- |
| `SendTo(u64)` | 向指定域发送 IPC 消息 |
| `MapInto(u64)` | 把内存页映射进指定域 |
| `Irq(u8)` | 注册接收指定 IRQ |
| `Mmio(u64)` | 把指定物理基址（页对齐）的 MMIO 区域映射进本域 |

- 每域 `CAP_SLOTS = 16` 个能力槽。
- 新域默认**无任何能力**，由授权方在启动时 `cap::grant` 显式授予（见 [kernel/src/main.rs](../kernel/src/main.rs)）。
- 应用侧通过 syscall 的返回 1/0 感知「是否被授权」；无能力时操作被内核拒绝。

---

## 7. 服务架构

### 现有服务（域）

| 服务 | 职责 | 交互方式 |
| --- | --- | --- |
| pager (2) | 缺页处理，映射匿名零帧 | 接收缺页消息 → `sys_map_anon` → `sys_page_fault_reply` |
| echo (3) | 同步 IPC 演示 | `recv` → `reply` |
| kbd (4) | 键盘驱动 | 注册 IRQ1 → `recv` scancode → 解码 |
| block_srv (5) | 块设备服务 | `recv` `BlockReq` → NVMe / IDE PIO 读写扇区 |
| fat32_srv (6) | FAT32 文件服务，挂载于 `/` | `recv` VFS tag → 解析 FAT32 → 经 block_srv 访问磁盘 |
| mount_srv (9) | 挂载管理（统一目录树） | `recv` `VFS_LOOKUP_TAG` → 最长前缀匹配 → `reply` `(域<<32)|前缀长度`；`MNTA` 运行时挂载（回复槽位号）/ `MNTD` 卸载 |
| tmpfs_srv (10) | 内存文件系统，挂载于 `/tmp` | `recv` VFS tag → 平铺节点表 + 字节区读写 |
| mfs_srv (11) | MorionFS 原创文件系统，挂载于 `/mfs` | `recv` VFS tag → 4 KiB 块 + CRC32 + COW 写时复制 → 经 block_srv 访问第二盘；另有快照 tag `MSNP/MSNL/MSNR` |
| shell (8) | 命令行解释器 | `sys_readline` 取行 → 命令 `help / echo / pwd / ls / cat / cd / mkdir / touch / rm / clear`（含 cwd 相对路径）→ libvfs(先查 mount_srv 路由, 再 `sys_call` 目标服务) |

> libvfs 对每个路径先向 mount_srv 查询，再由 fd 高 32 位的服务域字段路由后续
> `read/write/readdir/close`。应用只看到单一根 `/`：`/tmp/**` 落到 tmpfs_srv、
> `/mfs/**` 落到 mfs_srv，其余落到 fat32_srv。
>
> fd 的最高 16 位是**能力句柄**（`vfs::open`/`creat` 时由内核 `sys_cap_issue` 签发，
> 每次 I/O 前 `sys_cap_lookup` 校验，`vfs::close` 时 `sys_cap_drop` 撤销）。因此
> 关闭后的 fd、或伪造出其它域 fd 数值的 fd 都无法访问文件服务——「无能力即不可访问」。
>
> 路径需要临时改路由时可用 `vfs::mount(prefix, domain)` / `vfs::umount(prefix)`
> 在运行时增删挂载点（前缀传空串则自动分配空闲 `/mnt<N>`）。

### 规划中的服务

| 服务 | 职责 | 状态 |
| --- | --- | --- |
| ext2/ext4 服务（只读） | 兼容既有 Linux 分区 | 规划中，见 roadmap-fs.md |
| GUI 服务 | 图形界面 | 远期 |

### 输出约定（避免打断 shell 提示符）

域与 shell 共用同一控制台。为保持「正常终端」体验，**成功路径不要在运行期打印**
（尤其是反复触发的事件，如缺页、IPC、键盘），只在**失败**时打印一行诊断信息：

- 演示域（sender / receiver / pager / echo / kbd）与自检域（block_srv / fat32_srv / app）成功时静默；
- 自测只保留失败信息（如 `app: FS2 … FAILED`），通过与否以「无 FAILED + 能进 shell」判定；
- shell 自身的启动提示（`type 'help' for commands`）与提示符属于顺序输出，可保留。

---

## 8. 编码约定

- 用户态一律 `#![no_std]`，无动态分配器（`alloc` 暂不可用），内存通过 `sys_alloc_page` 手动管理。
- 跨域结构体（`Message`、`PageFaultInfo`）必须 `#[repr(C)]` 且字段顺序/类型与内核一致。
- 系统调用封装统一放在 `user/src/syscall.rs`，新增 syscall 时**内核编号与用户封装必须同步**。
- 用户态 panic 只能 `sys_exit()`，不要尝试恢复。
- 注释使用中文，与现有代码保持一致。

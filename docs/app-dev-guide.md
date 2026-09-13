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
| 12 | ext2_srv | ext2 只读兼容（挂载于 `/ext2`；解析超级块 / 块组描述符 / inode / 目录，不写盘） |
| 13 | exfat_srv | exFAT（**读 + 写**，挂载于 `/usb`；引导区 / FAT 链 / entry set / 分配位图 / upcase 表，支持 `CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`） |

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
    payload: [u8; 96],  // 固定 96 字节载荷
}
```

- `PAYLOAD_LEN = 96`（VFS 请求要把整条绝对路径装进 payload，故远比结构体大），
  每域邮箱容量 `MAILBOX_CAP = 16`（超出则 `send` 失败）。
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

文件内容、图像帧等大数据用共享内存传递，避免 IPC 96 字节载荷限制。

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
| block_srv (5) | 块设备服务（NVMe / IDE PIO）+ **卷层** | `recv` `BlockReq` → 启动时解析各盘 MBR/GPT 得卷表并探测 FS 类型；`opcode 0/1` 读写（卷号 + 分区偏移）、`opcode 2` 查询卷表 |
| fat32_srv (6) | FAT32 文件服务，挂载于 `/` | `recv` VFS tag → 解析 FAT32 → 经 block_srv 访问磁盘 |
| mount_srv (9) | 挂载管理（统一目录树） | `recv` `VFS_LOOKUP_TAG` → 最长前缀匹配 → `reply` `(域<<32)|前缀长度`；`MNTA` 运行时挂载（回复槽位号）/ `MNTD` 卸载 |
| tmpfs_srv (10) | 内存文件系统，挂载于 `/tmp` | `recv` VFS tag → 平铺节点表 + 字节区读写 |
| mfs_srv (11) | MorionFS 原创文件系统，挂载于 `/mfs` | `recv` VFS tag → 4 KiB 块 + CRC32 + COW 写时复制 → 经 block_srv 访问 MFS 卷；**目录项存 inode 号**，号到块的映射由 inode 表（索引块 `MFIX` → 表块 `MFIT`）给出，故多个名字可共享同一对象（硬链接）；目录是 ext2 风格**变长目录项**（名字 ≤255 字节、大小写敏感，条目区满后挂 `MFXI` 扩展目录块）；节点带**元数据**（`mode`/`owner`/`nlink`/`mtime`/`ctime`/`atime`，时间取自 CMOS RTC；`mode` 只存储与显示、不强制）；额外支持 `LINK`（硬链接）、`TRNC`（truncate，扩展为稀疏）、`RENM`（rename，可跨目录）、`CHMD`（chmod）；另有快照 tag `MSNP/MSNL/MSNR`、空间回收 `MSGC`（mark & sweep，回收不可达的 COW 旧块）与用量查询 `MSST`（回复 `(总块数 << 32) | 空闲块数`） |
| ext2_srv (12) | ext2 只读兼容，挂载于 `/ext2` | `recv` VFS tag → 只服务 `OPEN/READ/READDIR/STAT/CLOSE`（写类 tag 回 `u64::MAX`）→ 解析超级块 / 块组描述符 / inode 块映射 / 目录项 → 经 block_srv 访问 ext2 卷 |
| exfat_srv (13) | exFAT（读 + 写），挂载于 `/usb` | `recv` VFS tag → `OPEN/READ/READDIR/STAT/CLOSE` + `CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`（`RENM`/`CHMD`/`LINK` 回 `u64::MAX`）→ 解析引导区 + boot checksum / FAT 链 / entry set（含 set checksum 与 NameHash 生成）/ 分配位图 / upcase 表 → 经 block_srv 访问 exFAT 卷 |
| shell (8) | 命令行解释器 | `sys_readline` 取行 → 命令 `help / echo / pwd / ls ([-l]) / cat / cd / mkdir / touch / rm / mv / ln / chmod / truncate / stat / clear`（含 cwd 相对路径）→ libvfs(先查 mount_srv 路由, 再 `sys_call` 目标服务) |

> libvfs 对每个路径先向 mount_srv 查询，再由 fd 高 32 位的服务域字段路由后续
> `read/write/readdir/close`。应用只看到单一根 `/`：`/tmp/**` 落到 tmpfs_srv、
> `/mfs/**` 落到 mfs_srv、`/ext2/**` 落到 ext2_srv、`/usb/**` 落到 exfat_srv，其余落到 fat32_srv。
>
> fd 的最高 16 位是**能力句柄**（`vfs::open`/`creat` 时由内核 `sys_cap_issue` 签发，
> 每次 I/O 前 `sys_cap_lookup` 校验，`vfs::close` 时 `sys_cap_drop` 撤销）。因此
> 关闭后的 fd、或伪造出其它域 fd 数值的 fd 都无法访问文件服务——「无能力即不可访问」。
>
> 路径需要临时改路由时可用 `vfs::mount(prefix, domain)` / `vfs::umount(prefix)`
> 在运行时增删挂载点（前缀传空串则自动分配空闲 `/mnt<N>`）。
>
> `vfs::readdir(fd)` 把条目写成 `DirEntry` 数组（`name` 8.3 短名 + `long` 长名 + `size` +
> `is_dir` + `mode`/`owner`/`nlink`/`mtime`）。VFAT 长名、ext2 名字、exFAT 名字与 MFS 名字都填在 `long`
> （`long_len == 0` 表示只有短名）——显示时优先用长名（shell 的非 ASCII 字节折成 `?`，
> 因为内核字体只有 ASCII）。MFS 名字最长 255 字节，但端到端受单条 IPC 路径（95 字节）限制。
>
> 元数据字段只有 MFS 会填真值；其它文件服务填默认值（权限按目录/文件给 0755/0644、
> 属主 0、链接数 1、时间 0 = 未知）。`vfs::stat(path)` 回传完整 `Stat`（含三个时间戳）。
>
> 两个请求把**路径放在调用方共享页**里而不是 payload（payload 只有 95 字节，装不下
> "路径 + 结果页地址"，且结果页地址必须由客户端指定）：
> `vfs::rename(src, dst)` / `vfs::rename_into(src, dst, buf)`（页内 `src\0dst`）、
> `vfs::stat_into(path, buf)` / `vfs::chmod_into(path, mode, buf)`。用 `RESULT_BUF` 的
> 客户端（app）直接调不带 `_into` 的版本。
>
> 注意 `rename` 的两个路径必须路由到**同一个文件服务**（跨挂载点的搬迁不支持），
> 跨服务调用会直接返回 `u64::MAX`。

### 规划中的服务

| 服务 | 职责 | 状态 |
| --- | --- | --- |
| ext2/ext4 服务（读写） | 兼容既有 Linux 分区并支持写入 | 只读已完成（`ext2_srv` 域 12）；写支持规划中，见 roadmap-fs.md |
| exFAT 服务（读写） | 兼容 U 盘 / SD 卡（>32 GiB 主流格式）并支持写入 | ✅ 已完成（`exfat_srv` 域 13，挂载 `/usb`；读 + 写，`rename`/`chmod`/`link` 除外），见 roadmap-fs.md 阶段 D |
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

---

## 9. 面向系统 AI 的能力（AI-callable capabilities）

> **系统 AI** 指运行在同一套微内核之上的用户态 AI 运行时 / Agent。它要替用户完成任务，必须能
> ①**发现**应用提供了哪些能力，②按机器可读的契约**安全地调用**这些能力，③拿到结构化结果继续推理。
> 本节定义应用把功能暴露给系统 AI 的**规范**。
>
> ⚠️ **现状（务必先读）**：本节是**规范 / 目标**，不是既有 API。
> 已就绪、可作为落地基础的是：**能力系统**（能力槽 + 能力句柄 `SYS_CAP_ISSUE/LOOKUP/DROP`）、
> **IPC**（`sys_call` + 96 字节 payload）、**共享内存**（`sys_alloc_page` + `sys_share_page`）。
> 尚未实现的是：**能力注册 / 发现服务**、**AI 调用网关**、用户态 **JSON 序列化 / 解析**（当前无 `alloc`），
> 以及配套的 `#[ai_capability]` 过程宏与构建脚本。因此现在就可以按此规范**设计并标注**能力，
> 待支撑服务就绪即可直接接入，无需返工。

### 9.1 为什么需要单独的「AI 可调用」约定

普通 API 面向人类开发者：参数含义靠文档和常识补齐，出错时人看懂报错再改。AI 调用时缺少
**类型约束、状态语义和错误约定**，极易产生「幻觉式调用」（编造参数、误用相近工具、盲目重试副作用操作）。
因此 AI 可调用能力必须比普通 API **更显式**。下面 5 条原则是本系统所有 AI 能力的硬性要求。

| 原则 | 要求 | 在 Morion OS 上的落地方式 |
| --- | --- | --- |
| **1. 显式类型与约束** | 每个参数声明类型、取值范围、必填/选填、默认值、格式校验 | `input_schema` 用 JSON Schema 描述；调用前由网关按 schema 校验 |
| **2. 单一职责与可组合** | 一个能力只做一件事，靠组合完成复杂任务 | 把「打开并搜索」拆成 `file_open` + `content_search`；用 `composable_with` 给出组合建议 |
| **3. 幂等性与副作用** | 声明是否可安全重放，以及副作用类型 | `idempotent: bool` + `side_effect: read_only / local_write / network / device` |
| **4. 结构化返回值** | 统一 `{ success, data, error }`，`error.code` 机器可读 | 网关统一封装；应用只需返回 `data` 或 `error` |
| **5. 能力安全绑定** | 调用时必须通过能力检查 | `capability_requirements` 声明逻辑能力集 → 映射到内核能力（见 9.2） |

> 原则 2 的关键：**不要暴露参数众多的「万能函数」**。例如不要做 `do_file_op(op, path, pattern, ...)`，
> 而应拆成 `file_open` / `file_search` / `content_search` 三个单一职责能力，由 AI 自行编排。

### 9.2 与 Morion OS 原语的映射

AI 能力不是全新的一套机制，而是**叠在既有能力系统与 IPC 之上的一层标准化契约**：

| AI 能力概念 | Morion OS 落地 |
| --- | --- |
| 一次能力调用 | 网关 → 应用域的一次 `sys_call(tag = AI_CALL_TAG)` 同步 IPC |
| 参数 / 返回值（JSON，可能很大） | **共享内存页**交换：payload 只有 96 字节，放不下 JSON；双方约定固定虚拟地址，用 `sys_alloc_page` + `sys_share_page` 共享 |
| 统一响应 `{ success, data, error }` | 共享内存里的响应帧（见 9.6），`error.code` 为机器可读字符串 |
| `capability_requirements`（逻辑能力集） | 逻辑名（`fs.read` 等）由**应用清单**声明，网关校验后映射到内核能力 `SendTo(域)` / `MapInto(域)` / `Irq(n)` / `Mmio(基址)` |
| 调用鉴权 | 网关持有的**能力句柄**校验（`SYS_CAP_LOOKUP`），与 libvfs 的 fd 守卫同一套机制 |
| 审计日志 | 网关记录：调用者、能力名、参数摘要、结果状态、耗时（经日志服务输出） |
| 超时与限额 | 网关对 `sys_call` 施加超时；`latency_estimate_ms` 用于设定阈值 |

**逻辑能力集**（应用清单中声明的 `capability_requirements` 取值，建议命名空间）：

| 逻辑能力 | 含义 | 典型映射 |
| --- | --- | --- |
| `fs.read` | 只读数据访问 | 读写文件服务域所需的能力句柄 |
| `fs.write` | 本地状态修改 | 同上（写路径） |
| `net.access` | 网络通信 | 网络协议栈服务域 |
| `device.control` | 设备控制 / 直通 | `Mmio(基址)` / `Irq(n)` |
| `ipc.call` | 跨进程通信 | `SendTo(目标域)` |
| `proc.spawn` | 创建新域 / 进程 | 进程管理器服务 |

> 应用安装时由**能力注册服务**校验清单与 `capability_requirements` 是否自洽；运行时由**网关**
> 校验「当前任务的能力包」是否包含该能力所需权限。二者都遵循本系统「**无能力即不可访问**」原则。

### 9.3 能力描述文件 `capabilities.json`

每个注册了 AI 能力的应用，需生成一份机器可读的能力描述文件（参考 MCP 的工具描述模型）。

- **目标路径（Nix 化后）**：`/nix/store/<hash>-<app>/share/ai-capabilities/capabilities.json`
- **当前过渡路径**：随应用资源一起打包，如 `resources/apps/<app>/ai-capabilities/capabilities.json`

文件结构：

```json
{
  "app_id": "org.morion.filemanager",
  "version": "1.2.0",
  "capabilities": [
    {
      "name": "file_search",
      "display_name": "文件搜索",
      "description": "在指定目录中递归搜索匹配 glob 模式的文件，返回路径与大小列表。",
      "when_to_use": "当用户要求查找、定位或列出符合某个名称模式的文件时使用。不适用于按文件内容搜索。",
      "when_not_to_use": "用户要求按内容搜索时，应使用 content_search 能力。",
      "input_schema": {
        "type": "object",
        "required": ["dir", "pattern"],
        "properties": {
          "dir": { "type": "string", "description": "搜索起始目录的绝对路径，如 /mfs" },
          "pattern": { "type": "string", "minLength": 1, "description": "glob 模式，如 *.txt" },
          "max_results": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 }
        }
      },
      "output_schema": {
        "type": "array",
        "items": {
          "type": "object",
          "properties": {
            "path": { "type": "string" },
            "size": { "type": "integer" },
            "is_dir": { "type": "boolean" }
          }
        }
      },
      "capability_requirements": ["fs.read"],
      "side_effect": "read_only",
      "idempotent": true,
      "latency_estimate_ms": 50,
      "composable_with": ["content_search", "file_open"]
    }
  ]
}
```

关键字段说明：

| 字段 | 作用 |
| --- | --- |
| `name` / `display_name` / `description` | 能力标识与人类可读说明 |
| `when_to_use` / `when_not_to_use` | **让 AI 正确选择工具的核心**。只写「功能是什么」不够，必须说明「何时用、何时**不**用」，以区分功能相近的能力 |
| `input_schema` / `output_schema` | JSON Schema。**双职责**：网关运行时参数校验 + AI 理解能力语义 |
| `capability_requirements` | 调用所需的最小逻辑能力集（见 9.2），网关据此刻画权限边界 |
| `side_effect` | `read_only` / `local_write` / `network` / `device`，供策略引擎与 AI 判断风险 |
| `idempotent` | 是否可安全重放。**非幂等操作（发消息、改文件）AI 不会自动重试** |
| `latency_estimate_ms` | 供网关设置超时与 AI 预估耗时 |
| `composable_with` | 建议的组合能力，降低 AI 跨应用编排的试错成本 |

> 该文件应**随代码原子更新**：由 `#[ai_capability]` 过程宏或构建脚本从注解生成，开发者只维护注解。

### 9.4 在 Rust 中标注能力：`#[ai_capability]`

理念：**AI 集成只是额外添加注解**，不改变应用常规的 POSIX 风格开发流程。

```rust
/// 在指定目录中搜索匹配模式的文件。
/// 注解展开后: ①注册到能力注册表 ②生成 capabilities.json 中的对应条目。
#[ai_capability(
    name = "file_search",
    display_name = "文件搜索",
    description = "在指定目录中递归搜索匹配 glob 模式的文件",
    when_to_use = "用户要求按名称查找文件时",
    when_not_to_use = "用户要求按内容搜索时 (用 content_search)",
    capability = "fs.read",
    side_effect = "read_only",
    idempotent = true,
    composable_with = "content_search,file_open"
)]
fn search_files(dir: &str, pattern: &str, max_results: u32) -> Result<Vec<FileEntry>> { ... }
```

要点（与本节规范一致）：

- 函数名用**动词 + 名词**（`search_files`，而非 `do_search`）；
- 参数用强类型（`&str` / `u32` / 枚举），便于宏推导 `input_schema`；
- 返回 `Result<T, AiError>`，`AiError` 携带机器可读的 `code`；
- 需要修改状态的函数必须显式声明 `side_effect` 与 `idempotent`。

### 9.5 注册与发现

1. **注册流程**：应用构建阶段由宏 / 构建脚本生成 `capabilities.json`；安装时**能力注册服务**读取并
   编入全局能力索引（按应用、能力名、权限需求、副作用类型等维度可查）。遵循**一次注册、全局可用**：
   AI 可在文件管理器、终端、搜索等所有入口发现并调用。
2. **能力发现**：AI 编排任务前向注册服务查询，支持
   - **语义搜索**：按自然语言任务描述匹配最相关能力（能力数量大时无需穷举列表）；
   - **权限过滤**：只返回当前任务能力包有权调用的能力；
   - **组合推荐**：给定一个能力，返回其 `composable_with` 声明的可组合能力。
3. **与系统 AI 集成**：AI 运行时维护一份能力**快照**（定期从注册服务拉取），推理阶段把可用能力
   描述注入上下文；生成的调用请求经网关执行，结果注入对话历史。**应用无需感知 AI 的存在**——
   它只暴露标准化的能力描述与调用端点。

### 9.6 调用通道：能力网关

AI 的能力调用统一经**能力网关**（用户态服务）转发，网关负责：

1. 把 AI 的调用请求转换为目标应用的 **IPC 消息**（`sys_call`）；
2. 校验「任务能力包」与 `capability_requirements` 是否匹配（基于能力句柄）；
3. 记录审计日志（调用者 / 能力名 / 参数摘要 / 结果状态 / 耗时）；
4. 执行超时控制与资源限额。

通道选择：普通应用经微内核 IPC；申请了**飞地**的高性能应用可经**共享内存通道**直接转发，
跳过 IPC 拷贝（与「零内核陷落」路径一致）。

**消息约定（建议）**：

```text
tag     = AI_CALL_TAG
payload = { capability_id: u32, request_len: u32, response_cap: u64, _pad }   // 放进 96 字节 payload
共享页  = 请求 JSON (入参) + 响应帧 JSON (出参)，双方映射到约定同一虚拟地址
```

响应帧统一为三段式，`error.code` 为机器可读字符串（便于 AI 判断「重试 / 换工具 / 放弃」）：

```json
{ "success": true,  "data": { "...": "..." }, "error": null }
{ "success": false, "data": null, "error": { "code": "not_found", "message": "目录不存在" } }
```

### 9.7 开发者在应用中集成 AI 能力的步骤

1. **标注能力**：给希望暴露的函数加 `#[ai_capability]`，声明名称、描述、使用场景、权限需求、副作用。
2. **生成描述文件**：构建时由宏 / 构建脚本生成 `capabilities.json` 并打包到约定路径。
3. **声明权限**：在应用清单中声明所需逻辑能力集，安装时由能力注册服务校验。
4. **测试**：用系统提供的 `ai-capability-test` 工具模拟 AI 调用，验证描述文件正确性与 API 行为
   （含 schema 校验、错误码、幂等性）。
5. **（可选）组合提示**：用 `composable_with` 声明适合组合的能力，帮助 AI 高效编排工作流。

### 9.8 设计检查清单

编写或审查一个准备暴露给系统 AI 的 API 时逐项检查：

- [ ] 函数名是否为**动词 + 名词**（`search_files` 而非 `do_search`）？
- [ ] 每个参数是否有类型约束与取值范围说明？
- [ ] `when_to_use` 与 `when_not_to_use` **是否都提供**？
- [ ] 是否声明了 `side_effect`（只读 / 写入 / 网络 / 设备）？
- [ ] 是否标记了 `idempotent`？
- [ ] 错误返回是否有机器可读的 `code` 字段？
- [ ] 是否声明了 `capability_requirements`？
- [ ] 是否提供了 `composable_with` 建议？
- [ ] 返回值是否遵循统一的 `{ success, data, error }` 结构？
- [ ] 是否通过 `ai-capability-test` 验证过调用行为？

### 9.9 当前可用的底座与待补前置

| 组件 | 状态 | 说明 |
| --- | --- | --- |
| 能力系统（能力槽 + 能力句柄） | ✅ 已就绪 | `SYS_CAP_ISSUE/LOOKUP/DROP`（29/30/31）；见第 6 节 |
| IPC 与共享内存 | ✅ 已就绪 | `sys_call` / `sys_reply` / `sys_alloc_page` / `sys_share_page`；见第 4、5 节 |
| 文件系统统一接口 libvfs | ✅ 已就绪 | `open/read/readdir/stat/close` + 挂载路由；可作为首批 AI 能力的底座 |
| 用户态 JSON 序列化 / 解析 | ⏳ 缺失 | 当前 `#![no_std]` 且无 `alloc`；需先补一个最小的 `no_std` JSON 解析器与分配器 |
| `#[ai_capability]` 过程宏 / 构建脚本 | ⏳ 缺失 | 宿主侧代码生成，不依赖目标端 `no_std` |
| 能力注册 / 发现服务 | ⏳ 缺失 | 需新增用户态域；索引可先用内存表，后续换持久化 |
| AI 调用网关 | ⏳ 缺失 | 需新增用户态域，串联能力校验 + 审计 + 超时 |
| `ai-capability-test` 工具 | ⏳ 缺失 | 需新增测试工具域 |

> 结论：**应用能力以标准化、安全、可组合的方式暴露给系统 AI** 是本系统的既定方向。
> 底座（能力 + IPC + 共享内存）已经具备，缺口集中在「注册 / 网关 / JSON」三类支撑服务上。
> 在此之前，开发者按本节规范设计 API 与注解，是零返工的前置投入。

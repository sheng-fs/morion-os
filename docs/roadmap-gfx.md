# Morion OS 图形子系统路线图 (草案)

> 目标：把**图形输出从内核外移**到用户态服务 `gfx_srv` —— 内核只保留最小的 panic 输出，
> 屏幕由用户态渲染。这是「微内核最小化」的收口动作（`video` 是内核里最后一块大功能），
> 也是 GUI / 桌面（README 阶段五）的前置。
>
> 方向：与文件系统同构 —— 内核只提供机制（MMIO 授权 + 页映射 + IPC + 共享页），
> 像素怎么画、文本怎么排全在用户态服务。

---

## 1. 目标链路

```
应用(域) ── libmorion::gfx ──► gfx_srv ──(MMIO 写)──► 帧缓冲 (scanout)
                                  ▲
                                  └── 共享面 (surface): 客户端画在共享页, 服务 blit 上屏
```

最终可验证：用户程序调 `gfx.rect(...)` / `gfx.text(...)`，屏幕上确实出现对应图元。

---

## 2. 现状与关键前置

**已具备**

| 项 | 现状 |
| --- | --- |
| 线性帧缓冲 | 引导器经 `BootInfo.fb_addr/width/height/stride/bpp` 交出 GOP 帧缓冲；内核以**恒等映射**（前 4 GiB 2 MiB 大页）直接写 BGRA 像素（[framebuffer.rs](../kernel/src/video/framebuffer.rs)） |
| 内核终端 | [video/mod.rs](../kernel/src/video/mod.rs)：~1075 行「历史区 + 固定输入行 + 光标」文本终端；ASCII 走 [font.rs](../kernel/src/video/font.rs)，汉字/全角走 [unicode.rs](../kernel/src/video/unicode.rs) + `cjk.bin`（≈276 KB，**现已搬至用户态 `gfx_srv`**，见 G3c） |
| 帧缓冲保留 | [frame_allocator::init](../kernel/src/memory/frame_allocator.rs) 已把 fb 物理区间标为占用，不会被当空闲帧分出去 |
| MMIO 授权 | `Capability::Mmio(页对齐物理基址)` + `SYS_MAP_MMIO`(21)：把设备 BAR 映射进用户域（4 KiB 页 + `NO_CACHE` + `NO_EXECUTE`，[paging::map_mmio](../kernel/src/memory/paging.rs)） |
| 输出旁路 | 所有内核输出已镜像到 **COM1**（[video/mod.rs serial_*](../kernel/src/video/mod.rs)），headless 回归（`scripts/fs-regress.sh`）依赖它 |

**缺口**

> 以下是 G1 开工**前**的快照；G1–G4 与 G6 之后这些已逐一闭环（见第 4 节），仅 D2 的 write-combining 仍留待实测决定。

- 帧缓冲没有作为「可授权 MMIO 区间」暴露给用户域；也没有**缓存属性**选择（`NO_CACHE` vs 帧缓冲应有的 **write-combining**）。
- 字体与文本渲染（含 276 KB 字库）全在内核。
- 输入行编辑 + 行历史也在内核，且与 keyboard 域（`SYS_TERM_PUT`）强耦合。
- 没有 surface / 绘制原语 / 客户端库。

---

## 3. 关键决策（待定 → 定）

| # | 问题 | 候选 | 决定 |
|---|---|---|---|
| D1 | 帧缓冲授权方式 | (a) 复用 `Capability::Mmio(页对齐基址)` + `SYS_MAP_MMIO`；(b) **单列 `Capability::Fb`** + 三个 `SYS_FB_*` | **(b)（实现时修正）**：`Mmio` 能力按「页对齐物理基址」**逐页**匹配（设备 BAR 一页一条），而帧缓冲是**一整块**（可达上千页）——逐页授权既塞不下 16 个能力槽，也无意义。故新增无参能力 `Fb` + `SYS_FB_INFO` / `SYS_FB_MAP` / `SYS_FB_TAKEOVER`；映射仍复用 `paging::map_mmio` 的 4 KiB 非缓存页 |
| D2 | 帧缓冲缓存属性 | (a) 沿用 `NO_CACHE`；(b) 加 **write-combining (PAT)** | **先 (a) 跑通功能**（G1 已按此实现），G3 后按实测决定是否补 (b) —— 未缓存 MMIO **逐像素**写会明显拖慢填充/文字 |
| D3 | 内核终端去留 | (a) 一次性删除；(b) **分两阶段**：先与 gfx 并行，验证后再降级为 panic-only | **(b)**：G1 已实现「**接管开关**」（`SYS_FB_TAKEOVER` → 内核终端停止写帧缓冲、只留 COM1）；早期引导与 panic 输出仍走内核路径，永远不依赖用户态服务 |
| D4 | 文本渲染归属 | (a) 字库与排版搬到用户态；(b) 内核保留、gfx 只做 `draw_text` 薄封装 | **(a)**：把 276 KB 字库与排版逻辑移出内核；内核另留**最小 ASCII 字库**供 panic 路径 |
| D5 | 图形 API 形态 | (a) IPC 原语 `fill/rect/blit/text`；(b) 只把 scanout 直通给特权客户端 | **(a) 起步**：客户端画在**共享页**，服务 blit 上屏；(b) 属「性能飞地」主题，另议 |
| D6 | COM1 日志 sink | 必须保留一条串口输出路径 | **硬约束**：任何输出迁移都不得让 `SELFTEST DONE` 这类关键行从串口消失，否则回归直接失明 |

---

## 4. 设计要点

### G1 — 帧缓冲授权 + `gfx_srv` 起屏 ✅ 已完成

- 新增域 **15 `gfx_srv`**：`domain::create()` 增一个 + `BOOT_DOMAINS` 15 → 16 + `cap/ipc/pager::init` 计数 15 → 16 + `SERVICE_FILES` 加 `(15, "gfx_srv")` + `SRV_NAMES` 加 `gfx_srv`（沿用 E3b/E3c 的建域/打包流程）。
- 内核新增**无参能力 `Fb`**（帧缓冲全局唯一，逐页 `Mmio` 塞不下），授给 `gfx_srv`；配套三个 syscall：
  `SYS_FB_INFO(44)`（写回 `FbInfo { addr, width, height, stride, bpp }`）、
  `SYS_FB_MAP(45)`（把整块帧缓冲按 4 KiB 页映射进本域，先整段查重再映射）、
  `SYS_FB_TAKEOVER(46)`（宣告接管）。
- **接管开关**：内核 `video` 加 `FB_TAKEN_OVER` 标志；`SYS_FB_TAKEOVER` 置位后，`print` / `print_logo` / `clear_screen` / `term_*` / `scroll_*` 全部**跳过帧缓冲**，`print` 仍写 COM1 —— 于是串口日志不断（D6），屏幕归用户态。
- `gfx_srv` 启动：`SYS_FB_INFO` → `SYS_FB_MAP`（映射到 `USER_SPACE_BASE + 1 GiB`）→ 画测试图案（全屏底色 + 居中色块）→ **回读校验**（四角 + 中心像素必须等于写入值）→ `SYS_FB_TAKEOVER` → 重画一次并打印 `gfx: framebuffer takeover OK (kernel console detached)`。回读校验是自动化取证：证明这块映射确实可写、几何算得对，不依赖人工看屏。
- **验收**：串口出现 `gfx: framebuffer takeover OK` + `gfx: framebuffer 1280x800 stride=1280 phys=0x80000000`；**回读校验**（服务内写后读回、值必须一致）是自动化取证。⚠️ 本机 QEMU（`screendump` + virtio/std 两种 vga 都试过）取到的是一张**占位表面**（全域 `0x000033`），无法当作帧缓冲内容的证据 —— 裸写帧缓冲不经 `virtio-gpu` 的 flush，且 QEMU 11 在该组合下不反映 guest 显存；要肉眼确认需带真实显示前端的 QEMU。故正式判据以**回读 + 串口 marker + 全量回归**为准。

### G2 — 图形原语 + 共享面 + `libmorion::gfx` ✅ 已完成

- `gfx_srv` 实现三个原语（[gfx_srv.rs](../../user/srv/src/gfx_srv.rs)）：`GFX_OP_FILL`（整屏铺色）、`GFX_OP_RECT`（屏幕矩形）、`GFX_OP_BLIT`（面 → 屏，带裁剪）；另有 `GFX_OP_PING` 存活探测。请求循环**在接管之后**才启动，故客户端的第一条请求必然落在「屏幕已归用户态」之后。
- 新增客户端库 [`libmorion::gfx`](../../user/libmorion/src/gfx.rs)（模式照抄 `libvfs`）：协议（`GfxReq` / `GFX_OP_*` / `GFX_TAG`）+ `Surface`（分配 / 共享 / 像素 / `rect` / `blit`）+ 屏幕级 `fill_screen` / `screen_rect` / `ping`。
- **共享面**：客户端 `SYS_ALLOC_PAGE` + `SYS_SHARE_PAGE` 把表面**同址**共享给 `gfx_srv`，服务按请求里的 VA 直接读。地址按域 id 错开（`SURFACE_BASE = USER_BASE + 64 MiB`，步长 4 MiB，单面上限 1 MiB）—— 否则多个客户端会把表面共享到服务域的同一个 VA，第二个就撞 `PageAlreadyMapped`（与 libvfs 中转页是同一个坑）。
- **端到端取证（关键）**：`GFX_OP_BLIT` 在拷完后**回读帧缓冲**抽 5 个点与表面比对，全部相等才回 `1` —— 于是「真的画上去了」在**无显示器**时也可断言（本机 `screendump` 取不到内容，见 G1）。
- **自测 GS-1**（app）：`ping` → `fill_screen` + `screen_rect` → 建 160×120 表面画四条竖直色带 → 客户端侧回读 → `blit` 到 (16,16)（服务端回读校验）→ 打印 `app: GS1 gfx primitives + shared surface OK (blit verified on framebuffer)`。
- **能力**：app / shell 各授予 `SendTo(gfx_srv)` + `MapInto(gfx_srv)`。
- **视觉确认（已完成）**：在带真实显示前端的 QEMU（GTK 窗口 + `-serial file:`）里肉眼核对，屏上正是 GS-1 该有的画面 —— 近黑底 + 左上 `#404060` 矩形 + (16,16) 处 160×120 的红/绿/蓝/黄四条竖色带。G1 里「`screendump` 取到占位表面」的结论只适用于 `-display none` 那套无头截图路径，窗口路径是正常的。

### G3a — 服务内终端 + 文本渲染 ✅ 已完成

- **字库搬家**：[`font.rs`](../../user/srv/src/gfx/font.rs)（ASCII 8×16 字模表）、[`glyphs.rs`](../../user/srv/src/gfx/glyphs.rs)（UTF-8 解码 + 字形二分查找 + 显示宽度）、[`cjk.bin`](../../user/srv/src/gfx/cjk.bin)（≈276 KB 汉字点阵，`git mv` 进服务）都归 `gfx_srv`；新增 [`gfx/mod.rs`](../../user/srv/src/gfx/mod.rs)（`Fb` 帧缓冲视图，可复制）与 [`gfx/term.rs`](../../user/srv/src/gfx/term.rs)（终端状态）。
- **分工**：`glyphs` 只回答「某字的第 (row,col) 位亮不亮」，**不碰帧缓冲**；`term` 负责按**显示列**排版（ASCII 1 格、汉字/全角 2 格，宽度取自字库记录）、自动换行、到底滚动、清屏、`\r`/`\t`。
- **落笔即校验**：终端每画一个字符，都对整格**逐像素**「算出应显示的颜色 → 写入 → 立即读回比对」；任一处不一致就回 0。于是「字真的画到帧缓冲上了」在无显示器时可断言，且帧缓冲映射一旦失效会立刻在回复值里暴露（不再有"静静画到空气里"）。
- **协议（客户端 [`libmorion::gfx`](../../user/libmorion/src/gfx.rs)）**：`GFX_OP_TEXT`（`buf` = 文本页、`w` = 字节数，写字节数上限 4096）、`GFX_OP_CLEAR`、`GFX_OP_MOVE`（列/行，越界**拒**而不夹取）、`GFX_OP_QUERY`（回 `行<<32 | 列`）。文本经**共享页**传（本域窗口内 +1 MiB 处一页，按域 id 错开），不塞 IPC payload —— 一行可能很长，且 UTF-8 一个字节不一定一列。
- **内核侧**：G3a 时终端与 `unicode.rs` 暂时保留（G3c 才删），`cjk.bin` 改指用户态那份 —— 一份数据，避免两处漂移；**G3c 已把内核侧那份引用与 CJK 渲染一并删掉**（见下）。`scripts/gen-cjk-font.py --out` 默认输出同步改到 `user/srv/src/gfx/cjk.bin`。
- **自测 GT-1**（app）：`clear_screen` → 断言光标 `(0,0)` → 写 `"GT-1 "` → 断言光标列 = **5**（5 个 ASCII × 1 列）→ 写 `"汉字宽字符"` → 断言列 = **15**（5 个汉字 × 2 列）→ 混排一行 + `move_cursor(0,20)` 再写一行 → 断言越界 `move_cursor(9999,9999)` 被**拒** → `app: GT1 text console OK (layout cols verified, framebuffer pixel readback matched)`。列数断言同时证明 UTF-8 解码、宽窄混排与光标推进。
- **视觉确认**：`gfx_srv` 接管后用服务内终端清屏并写横幅，屏上第一行字完全是用户态画的。

### G3b — shell 输出上屏 ✅ 已完成

- **内核**：新增 `SYS_CONSOLE_READY(47)`（无能力要求）—— 回答「显示是否已交用户态」（读 `video::FB_TAKEN_OVER`）。客户端有了它就不必用一次 `SYS_CALL` 去白等图形服务。
- **运行库**：`libmorion::syscall` 里把打印的唯一出口收成 `sink()`（`SYS_PUTS` + 可选镜像），加 `screen_mirror_on()` 开关；[`gfx::print`](../../user/libmorion/src/gfx.rs) 作为镜像目标。**镜像按进程 opt-in**：只有 shell 打开，自测那种成千上万条打印的路径不受影响（每条多一次 IPC 往返不划算）。
- **shell**：[shell.rs](../../user/srv/src/shell.rs) 启动时有界等待（上限 `CONSOLE_WAIT_MS = 1000`，到点就退回"只写串口"）→ 确认 `SYS_CONSOLE_READY` 且 `gfx_srv` 有活任务 → 开镜像。于是 shell 的横幅、中文欢迎语、每条命令输出都**同时**进串口（内核终端）与屏幕控制台。
- **盲测自证**：开镜像后立刻用 `GFX_OP_QUERY` 问服务端光标，`row > 0` 就打印 `shell: screen console mirror OK (gfx_srv cursor advanced)`（这句走 `sys_puts`，只进串口，不占屏幕）—— 没有显示器也能证明"shell 的输出真的写进了 gfx_srv 的终端"。
- **输入仍可用**：接管只关掉内核的**重绘**（[`redraw`](../../kernel/src/video/mod.rs) / `redraw_input_line` 在接管后为空操作），`term_put` / 退格 / 左右移的**编辑与回车提交照旧执行** —— 否则回车不会把输入行推进队列，阻塞在 `SYS_READLINE` 的 shell 将永远收不到命令（表现为「终端卡死」）。这是接管初期踩过的坑。（**G4 后输入整体搬出内核**，这一节的 `term_put` / `SYS_READLINE` 等已删除，见下方 G4。）
- **已知限制（归 G4）**：内核终端负责的**输入行回显**还没外移，故在屏幕上打字看不见（提示符与命令输出看得见；打字仍能提交、命令仍能执行）。屏幕控制台也不做行级保护 —— 目前只有 shell 一个镜像客户端，暂不会互相顶掉。

### G3c — 内核卸字库、终端降为 ASCII + 豆腐块 ✅ 已完成

- **删了什么**：`unicode.rs` 里的 `cjk.bin` `include_bytes!`（275983 字节）、定长记录/二分查表（`Glyph` / `glyph`）、以及「有字形就按字形宽度排版」那条路径 —— 内核不再携带任何汉字点阵。
- **留了什么**（接管前那几秒 + panic 屏够用即可）：UTF-8 `decode` / `prev_index` / `next_index`（退格与左右移**不会切开多字节字符**）、`width` / `str_width`（改按**东亚宽度**粗判：ASCII 1 格、汉字类 2 格，与用户态口径一致）、`draw`（ASCII 走 8x16 字模，其余画**空心豆腐块**，宽度仍按 2 格占位）。
- **实测收益**：内核 ELF `347376 → 70504` 字节（**−276872，约 −80%**）；`cjk.bin` 只剩用户态那一份（`user/srv/src/gfx/cjk.bin`）。
- **代价（已知且接受）**：`gfx_srv` 接管**之前**那几秒，屏上内核日志里的中文是豆腐块；panic 屏上的中文同理。**COM1 串口全程不变**（`print` 直接写 UTF-8 字节，不经本模块），故 headless 回归与判据完全不受影响。
- **单测**：`video/unicode.rs` 加 4 条 host 单测（宽度口径 / 混排列数 / 字符边界 / 截断序列），内核单测 16 → **20**。

### G4 — 输入搬出内核（行编辑/回显外移）✅ 已完成

- **内核只剩搬运**：新增 `SYS_KEY_PUSH(48)` / `SYS_KEY_READ(49)` 与 `kernel/src/key.rs`（64 字节环形键队列，满则丢新键、绝不阻塞内核）。`kbd_srv` 只把 scancode 译成字节（可打印 / 退格 `0x08` / 回车 `'\n'`，方向键丢弃），**不带任何编辑语义**。15..20 与 27 号 syscall（内核终端输入行）与 `INPUT_WAIT`（改名 `KEY_WAIT`）一并退役。
- **内核侧删除**：`term_put` / `term_backspace` / `term_left` / `term_right` / `scroll_view_up/down` / `input_read` / 输入行队列 / `INPUT_BASE`（行内提示符）/ 输入输出隔离（`input_detach`+`input_reattach`+`IN_SAVE_*`）/ 跨行累积 `IN_ACCUM` / `CURSOR_X,Y`+`set_cursor` / 历史区光标导航（`CUR_ROW`/`CUR_COL`/`SCROLL_OFFSET`）—— `video` 从此只有「512 行历史环 + 一行当前输出」，即**引导期日志 + panic 屏**两件事。
- **行编辑落在客户端库**：[`morion::console::readline`](../../user/libmorion/src/console.rs) 阻塞取键、可打印字符追加并**立刻回显**（走 `print` ⇒ 串口 + 可选屏幕镜像，所以**打字终于看得见了**）、退格发 `\b`、回车成行。
  - ⚠️ **为什么不放服务端**：`gfx_srv` 单线程，服务端一旦阻塞在「等按键」就出不了请求循环 —— app 自测的 `GFX_OP_FILL`/`GFX_OP_TEXT` 会被饿到有人按键为止。`SYS_KEY_READ` 在**客户端**阻塞是免费的（内核把任务睡下，有键再唤醒）。
  - ⚠️ 代价：输入不再有内核兜底，**屏幕控制台不可用时 shell 没有输入源**（如实报错退出，不再"盲打可用"）。
- **屏幕侧配合**：`Term::write` 支持 `\b`（光标左移一列 + 把该格涂成背景色，与其它落笔一样逐像素回读）；键盘输入目前全是 ASCII，故一次退格 = 一列。
- **顺带修掉一个真实竞态**：`gfx_srv` 原先「整屏绘制 → 回读校验 → 接管」，而接管前内核仍在整幅重绘日志，被校验的像素会被擦成背景渐变 → **假失败**（实测 `gfx: framebuffer readback FAILED`，随后 shell 报无控制台）。改为「顶部带 `probe` 探测映射可写 → 接管 → 独占后 `paint`+`verify`」。
- **实测**：内核 ELF `70504 → 66040` 字节。QEMU monitor `sendkey` 无头实测：`h e l p ⏎` → 串口出现 `help` 回显 + 完整命令列表；`e c h o spc h i z ⌫ ⏎` → 串口出现 `echo hiz\x08` + `hi`（取键 → 回显 → 退格删缓冲并擦屏 → 回车提交整条链路）。

### G6 — 图形服务自愈（监督重启 + 客户端会话重建）✅ 已完成

> G4 之后留下的一个真实缺口：`gfx_srv` **不在 init 监督集**里，它一崩，屏幕永久死；更糟的是
> `ipc::call` 的调用方会把请求塞进它邮箱后**无限等回复** —— 服务没了就**永久挂死**（shell 的
> 打印镜像首当其冲）。本节把它补上。

- **内核三处**：
  1. `ipc::call` 由无限阻塞改为**带超时轮询**（`CALL_POLL_MS = 200`），并在目标域**无存活任务**时
     立即失败返回 `u64::MAX`。挂死的根因是：调用方阻塞在**自己的域键**上（为的是被发往本域的任何
     消息唤醒），服务若在回复前退出，就再没人回这条请求。
  2. 帧缓冲登记为**内核保留区间**（`frame_allocator::pin_range`，`free_frame` 对其空操作）。
     `SYS_FB_MAP` 走 `map_mmio` 且**不计引用计数**，于是「同域重启」清地址空间时
     `free_user_space` 会把显存当"本域独占帧"释放 —— 大内存配置下等于把屏幕内存交回分配器。
  3. 「原地重启」新增**丢弃目标域邮箱里未处理的请求**（`restart_in_place` 加 `ipc::remove_domain`）：
     那些请求常引用客户端共享过来的页，而 `reset` 已把映射清掉，新实例去处理就会读悬空地址而再崩。
- **客户端会话重建**（`user/libmorion/src/gfx.rs`）：服务重启会清空它域内的页表，之前
  `SYS_SHARE_PAGE` 共享过去的文本页/表面**随之消失**。故：
  - `call` 收到 `u64::MAX`（服务不可用）时把共享会话标记为失效；
  - `ensure_shared` 重建时**只重发 `share`、不再 `alloc`**（页仍在本域），并在重发前**有界等待
    服务活过来** —— 必须等 `reset` 之后重发才安全，否则会在服务域撞已映射页而 panic；
  - 服务端对"不在本域映射里的共享页"回新码 `GFX_REPLY_NO_SESSION(2)`，`print`/`blit` 据此
    **重建共享再试一次** —— 覆盖"重启落在两次调用之间、客户端没察觉"，这正是 shell 的常态
    （它多半正阻塞在等输入，不会在空窗期调用）。
- **服务侧**：`GFX_OP_EXIT(8)` 自测钩子（**先回复再退出**，否则请求方等不到回复）；`text`/`blit`
  对不在本域映射的共享页回 `GFX_REPLY_NO_SESSION`。
- **init**：把 `gfx_srv(15)` 纳入 `SUPERVISED`（10 个）。
- **顺带修一个并发问题**：GT-1 的列数断言原依赖**全局光标**，而屏幕是**多客户端共享**的
  （shell 也镜像打印、共用一条光标）—— shell 的启动输出插进"清屏 → 写 → 问光标"之间就会让测得
  列数偏大。改为**重试到干净窗口**（shell 打完启动输出就阻塞等输入），不必给服务端加"原子返回
  列数"的协议。
- **自测 GS-2**（app）：杀 `gfx_srv` 两轮 —— ① 空窗期不调用：走 `NO_SESSION` → 重建共享恢复；
  ② 空窗期调用：`ipc::call` 必须**快速失败**（不挂起），重启后又能用。日志：
  `init: restarted gfx_srv (domain 15, total N, from memory)` ×2 +
  `app: GS2 gfx_srv restart + client session rebuild OK (screen recovered)`。

### G5（可选后续）— surface 合成 / 多窗口

- 引入 surface 合成与多窗口，为「桌面」铺路。当前只有一个全屏文本控制台客户端。

---

## 5. 执行顺序（每步独立可回归）

1. **G1 帧缓冲授权 + 起屏**（本批第一步，最小闭环）：建域 + 授权 + `gfx_srv` 清屏画图 + 交接开关 + 回归。
2. **G2 原语 + 共享面 + 客户端库**：`GFX_FILL/RECT/BLIT` + `libmorion::gfx` + 自测 GS-1。
3. **G3a 服务内终端**（✅ 已完成）：字库/排版搬进 `gfx_srv` + `GFX_TEXT/CLEAR/MOVE/QUERY` + 自测 GT-1。
4. **G3b shell 输出上屏**（✅ 已完成）：`SYS_CONSOLE_READY(47)` + `libmorion` 打印镜像 + shell 开镜像与串口自证。
5. **G3c 内核减重**（✅ 已完成）：卸掉内核侧字库（−276 KB），终端降为 ASCII + 豆腐块；视实测决定是否上 write-combining（D2）。
6. **G4 输入外移**（✅ 已完成）：键盘字节走 `SYS_KEY_PUSH`/`SYS_KEY_READ`，行编辑/回显落在 `morion::console`，内核终端只剩输出。
7. **G6 服务自愈**（✅ 已完成）：`ipc::call` 不再永久挂起 + 帧缓冲登记保留区间 + 重启清邮箱 + 客户端会话重建 + `gfx_srv` 纳入监督 + 自测 GS-2。
8. **G5（可选）surface 合成 / 多窗口**：为「桌面」铺路。

---

## 6. 验收（沿用现有口径 + 新增）

| 项 | 判据 |
|---|---|
| 起屏 | `gfx_srv` 能独占帧缓冲并渲染出预期图元（QEMU `screendump` 像素断言 / 串口 marker） |
| 不退化 | `make fmt` / `check` / `clippy` 全 0；内核单测全过；**NVMe 全量 FS 回归**仍 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds == cmds` 且 `poll_cmds = 0`、宿主 `sgdisk -v` "No problems found" |
| 串口 sink | 迁移后关键行仍在 `-serial file:` 里（D6） |
| panic 路径 | 内核 panic / 早期引导仍能输出（不依赖用户态服务） |
| 交接 | gfx 接手后内核不再写屏；接手前日志可见 |

---

## 7. 主要风险

1. **帧缓冲写性能**：未缓存 MMIO 逐像素写是主要瓶颈；G3 前先用 `fill_rect` 整行写 + 后续上 write-combining（D2）。
2. **回归依赖 COM1**：D6 是硬约束；把输出搬到用户态时须显式保留串口路径，否则 `fs-regress.sh` 失明。
3. **交接竞态 / 闪烁**：内核终端与 gfx 双写期间需要明确的「谁在画」开关，避免同屏互相覆盖。
4. **字库进镜像**：276 KB 字库进 `gfx_srv` 的 ELF，引导模块/盘镜像体积增加（与 E3b 的模块表同源，可接受）。
5. **帧缓冲地址范围**：内核恒等映射只覆盖前 4 GiB；需确认 GOP 帧缓冲落在范围内（QEMU q35 下成立），否则要先补映射。

---

## 8. 相关文档

- 内核速查：[dev-reference.md](dev-reference.md)（`SYS_MAP_MMIO`、`paging::map_mmio`、共享缓冲地址约定）
- 应用开发：[app-dev-guide.md](app-dev-guide.md)
- 架构（图形/GUI 目标）：[architecture.md](architecture.md)「系统 UI」
- 文件系统路线（本轮之前的批次）：[roadmap-fs.md](roadmap-fs.md)

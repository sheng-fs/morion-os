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
| 内核终端 | [video/mod.rs](../kernel/src/video/mod.rs)：~1075 行「历史区 + 固定输入行 + 光标」文本终端；ASCII 走 [font.rs](../kernel/src/video/font.rs)，汉字/全角走 [unicode.rs](../kernel/src/video/unicode.rs) + [cjk.bin](../kernel/src/video/cjk.bin)（≈276 KB） |
| 帧缓冲保留 | [frame_allocator::init](../kernel/src/memory/frame_allocator.rs) 已把 fb 物理区间标为占用，不会被当空闲帧分出去 |
| MMIO 授权 | `Capability::Mmio(页对齐物理基址)` + `SYS_MAP_MMIO`(21)：把设备 BAR 映射进用户域（4 KiB 页 + `NO_CACHE` + `NO_EXECUTE`，[paging::map_mmio](../kernel/src/memory/paging.rs)） |
| 输出旁路 | 所有内核输出已镜像到 **COM1**（[video/mod.rs serial_*](../kernel/src/video/mod.rs)），headless 回归（`scripts/fs-regress.sh`）依赖它 |

**缺口**

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

### G3 — 文本渲染外移

- 把 `font.rs` / `unicode.rs` / `cjk.bin` 移到用户态（放 `gfx_srv` 或一个共享 crate），`gfx_srv` 提供 `GFX_TEXT`（含宽窄混排、按显示列排版）。
- 内核保留**最小 ASCII 字库**，仅服务 panic 与「gfx 未接手」窗口。
- shell 的输出改走 gfx（经 libmorion::gfx），串口 sink 保留（D6）。
- **自测 GS-2**：屏幕出现指定中英文文本；与内核旧终端在内容上等价。

### G4（可选后续）— 控制台 / 窗口服务化

- 把**输入行编辑 + 行历史 + 光标**从内核迁到用户态 console 服务，内核只剩 panic 输出（微内核最小化真正到位）。
- 引入 surface 合成 / 多窗口，为「桌面」铺路。

---

## 5. 执行顺序（每步独立可回归）

1. **G1 帧缓冲授权 + 起屏**（本批第一步，最小闭环）：建域 + 授权 + `gfx_srv` 清屏画图 + 交接开关 + 回归。
2. **G2 原语 + 共享面 + 客户端库**：`GFX_FILL/RECT/BLIT` + `libmorion::gfx` + 自测 GS-1。
3. **G3 文本外移**：字库/排版搬家 + `GFX_TEXT` + shell 走 gfx + 自测 GS-2；视实测决定是否上 write-combining（D2）。
4. **G4（可选）控制台/窗口服务化**：输入行与合成外移，内核降为 panic-only。

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

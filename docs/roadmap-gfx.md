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
| D1 | 帧缓冲授权方式 | (a) 复用 `Capability::Mmio` + `SYS_MAP_MMIO`；(b) 新增专用 `SYS_MAP_FB` | **(a)**：机制已就绪，fb 也是一个「物理 MMIO 区间」，不必新增 syscall |
| D2 | 帧缓冲缓存属性 | (a) 沿用 `NO_CACHE`；(b) 加 **write-combining (PAT)** | **先 (a) 跑通功能**，G3 后按实测决定是否补 (b) —— 未缓存 MMIO **逐像素**写会明显拖慢填充/文字 |
| D3 | 内核终端去留 | (a) 一次性删除；(b) **分两阶段**：先与 gfx 并行，验证后再降级为 panic-only | **(b)**：早期引导日志、panic 输出、回归取证都依赖它，不能一次断 |
| D4 | 文本渲染归属 | (a) 字库与排版搬到用户态；(b) 内核保留、gfx 只做 `draw_text` 薄封装 | **(a)**：把 276 KB 字库与排版逻辑移出内核；内核另留**最小 ASCII 字库**供 panic 路径 |
| D5 | 图形 API 形态 | (a) IPC 原语 `fill/rect/blit/text`；(b) 只把 scanout 直通给特权客户端 | **(a) 起步**：客户端画在**共享页**，服务 blit 上屏；(b) 属「性能飞地」主题，另议 |
| D6 | COM1 日志 sink | 必须保留一条串口输出路径 | **硬约束**：任何输出迁移都不得让 `SELFTEST DONE` 这类关键行从串口消失，否则回归直接失明 |

---

## 4. 设计要点

### G1 — 帧缓冲授权 + `gfx_srv` 起屏

- 新增域 **15 `gfx_srv`**：`domain::create()` 增一个 + `BOOT_DOMAINS` 15 → 16 + `cap/ipc/pager::init` 计数 15 → 16 + `SERVICE_FILES` 加 `(15, "gfx_srv")` + `SRV_NAMES` 加 `gfx_srv`（沿用 E3b/E3c 的建域/打包流程）。
- 内核把 fb 物理区间（页对齐，长度按 `stride*height*bpp/8` 向上取整）作为 `Capability::Mmio(fb_pa)` 授给 `gfx_srv`。
- `gfx_srv` 启动：`sys_map_mmio` 映射 fb → 清屏成渐变 + 画一个矩形/文字。
- **交接协议**：内核终端在 `gfx_srv`「接手」**之前**照常画（引导日志可见 + COM1 镜像）；接手后内核停止写屏，仅保留 COM1 与 panic 路径。需要一个「谁来画」的开关（内核侧一个 `FB_OWNER` 标志，或 gfx_srv 首次刷新时通知内核）。
- **验收**：屏幕出现预期图元；`SYS_MAP_MMIO` 返回 1；宿主侧用 QEMU `screendump`（ppm）做像素断言，或用串口 marker 断言到达。

### G2 — 图形原语 + 共享面 + `libmorion::gfx`

- `gfx_srv` 实现原语：`GFX_FILL` / `GFX_RECT` / `GFX_BLIT`（面 → 屏）。
- 新增客户端库 `libmorion::gfx`（分配共享面、提交绘制请求），模式照抄 `libvfs`。
- **共享面**：客户端把一页/多页共享给 `gfx_srv`（`SYS_SHARE_PAGE`），服务把面内容 blit 到 fb。地址约定避开已有固定区（见 dev-reference「共享缓冲地址约定」）。
- **自测 GS-1**：app 画到一个共享面（多条色带/方块）→ `gfx_srv` 拷上屏 → 串口 marker + 像素断言。

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

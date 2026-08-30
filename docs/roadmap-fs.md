# Morion OS 文件系统路线图

> 目标：在 QEMU 里跑通「应用 → libvfs → fat32_srv → nvme_srv → NVMe 磁盘」的读文件链路。
> 方向：**NVMe 块设备驱动 + FAT32 文件系统**，均以用户态服务实现（微内核不解析文件系统元数据）。

---

## 1. 目标链路

```
应用(域) ── libvfs ──► fat32_srv ──(块设备 IPC)──► nvme_srv ──(MMIO/DMA)──► NVMe 磁盘
                         (文件服务)                (块设备驱动)
```

最终可验证：用户程序 `read("/hello.txt")` 读到 FAT32 镜像里的文件内容。

---

## 2. 关键前置（内核能力缺口）

| 缺口 | 现状 | 需补齐 |
| --- | --- | --- |
| PCI 枚举 | ✅ 已补齐（阶段 0） | 扫 bus/device/function，找 NVMe 控制器（class `01:08:02`），读 BAR0 |
| MMIO 映射能力 | ✅ 已补齐（阶段 0） | 新增 `Capability::Mmio` + `map_mmio` syscall，把 BAR0 映射到驱动域 |
| DMA 物理页 | ✅ 已补齐（阶段 0） | 分配**页对齐、物理连续**的队列与数据缓冲，映射到驱动域 |
| 块设备能力 | 无 | 新增能力，让 `fat32_srv` 能调用 `nvme_srv` 读写块 |
| 块设备协议 | 无 | 定义 `read_lba` / `write_lba` IPC 消息格式 |
| 中断 | 仅 PIC；NVMe 需 MSI/MSI-X | **先轮询 CQ 完成队列**，MSI/MSI-X 后置 |

---

## 3. 阶段划分

### 阶段 0 — 内核基础设施 ✅ 已完成

- [x] PCI 枚举（[arch/pci.rs](../kernel/src/arch/pci.rs)，I/O 端口 `0xCF8/0xCFC`，后续可换 MMCFG）。
- [x] `Capability::Mmio` + `sys_map_mmio`（编号 21）：把设备 BAR 映射到驱动域（`paging::map_mmio`，置 `NO_CACHE` 非缓存）。
- [x] DMA 页分配：帧分配器 `allocate_frames(n)` 支持「连续 N 帧」分配，页对齐且物理连续。

**验证（已通过）**：`make run-nvme`（QEMU q35 + `-device nvme`）下，启动日志正确打印 PCI 设备列表并定位
NVMe 控制器（class `01:08:02`，vendor `0x1B36`），读出 BAR0 = `0xC0000000`。

### 阶段 1 — NVMe 驱动服务 `nvme_srv`

QEMU 挂载 NVMe：

```bash
qemu-system-x86_64 \
  -machine q35 \
  -drive file=fat.img,if=none,id=nvme0,format=raw \
  -device nvme,serial=MORION,drive=nvme0 \
  ...
```

用户态初始化流程（NVM Express 规范）：

1. 禁用控制器（`CC.EN=0`）。
2. 配置 admin queue：写 `AQA`（队列深度）、`ASQ`（admin submission queue 基址）、`ACQ`（admin completion queue 基址）。
3. `CC` 置 `EN=1`，轮询 `CSTS.RDY=1`。
4. `Identify Controller/Namespace`（admin 命令）拿到扇区数 + 扇区大小。
5. 创建 I/O Submission/Completion Queue。
6. 实现 `read_lba`/`write_lba`（提交 SQE → 写 doorbell → 轮询 CQ 完成）。

**验证**：读第 0 扇区，打印前 512 字节（应看到 FAT32 BPB 特征：`0x55AA` 结尾 + "FAT32" 卷标）。

### 阶段 2 — FAT32 文件服务 `fat32_srv`

- 解析 BPB → FAT 表 → 根目录 → 目录项 → 簇链。
- 实现：读目录（列出根目录）、读文件（沿 FAT 链读簇）。
- 通过块设备能力调用 `nvme_srv` 的 `read_lba`。
- **读优先，写后置**（写文件/建目录放阶段 4）。

**验证**：列出根目录 + `cat` 一个文件。

### 阶段 3 — libvfs + 集成

- `libvfs` 客户端库：把 `open`/`read`/`readdir` 封装成对 `fat32_srv` 的 IPC。
- 挂载管理 + 统一目录树（先支持单挂载点）。
- 宿主机 `mkfs.fat -F 32 fat.img` 生成镜像 → 挂到 QEMU NVMe → 应用读写。

**验证**：用户程序 `read("/hello.txt")` 成功。

### 阶段 4 — 远期（不在本轮）

- FAT32 写支持（建目录 / 写文件 / 删除）。
- MSI/MSI-X 中断替代轮询。
- 多文件服务挂载 + 统一命名空间。

---

## 4. 主要风险

1. **MSI/MSI-X 未支持** → 阶段 1 用轮询 CQ，功能优先，性能后补。
2. **OVMF 退出后 NVMe 状态未知** → 内核自己完整初始化控制器，不依赖固件（与之前 i8042 键盘同理）。
3. **DMA 物理连续性** → NVMe 队列/缓冲必须物理连续且页对齐，帧分配器需支持连续多帧分配。
4. **用户态 DMA 地址翻译** → 驱动域需拿到「物理地址」写进 SQE，须确保映射关系正确（内核提供 vaddr→paddr 解析）。

---

## 5. 相关文档

- 应用开发接口：[app-dev-guide.md](app-dev-guide.md)
- 内核速查：[dev-reference.md](dev-reference.md)
- 总体架构：[architecture.md](architecture.md)（第 45-52 行「文件系统——用户态服务集合」）

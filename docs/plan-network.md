# 网络能力 + 网卡驱动 开发规划（N5 起）

> 状态：**已实现**（N5–N9 + N8.2 拆分全部落地，见 §9）。承接驱动路线 N0–N4（`net_srv` virtio-net 驱动 + 最小协议栈自测）。
> 本规划把网络工作拆成两条并行主线：**网卡驱动层**（换/加真网卡、多网卡抽象）与
> **网络能力层**（协议栈服务 + socket API + 能力门禁），目标是让**应用真正能用网络**。
> 相关：`docs/roadmap-driver.md`（N 小节 + §8 风险）、`docs/architecture.md` §「网络协议栈——用户态多服务架构」、
> `docs/dev-workflow.md`（加服务 9 处）、`docs/app-dev-guide.md`（`net.access` 能力名）。

---

## 1. 现状（N0–N4）

| 项 | 现状 | 出处 |
|---|---|---|
| 网卡驱动 | **仅 virtio-net**（modern）：能力链表自解析 / RX·TX virtqueue / MSI-X 中断收包 | `user/srv/src/net_srv.rs:1-26` |
| 协议栈 | ARP（含缓存老化 TTL 30s）、最小 IPv4（头构造/解析/校验和/拒分片）、ICMP echo 收发、UDP 构造/发送、DHCP 客户端、**最小 TCP**（三次握手 + 单段数据） | `net_srv.rs`；`roadmap-driver.md` N3b/N3c/N4 |
| 应用接口 | **没有**。`net_srv::run()` 是**自测 + 收包循环**，不处理 `sys_recv_msg`，无 IPC 服务 | `net_srv.rs:1457` |
| 客户端库 | `user/libmorion` **无** net 模块（只有 syscall/vfs/exec…） | `user/libmorion/src/` |
| 能力门禁 | **无** `Net` 类能力；`cap.rs` 只有 SendTo/MapInto/Irq/Mmio/Spawn/Fb/IoPort | `kernel/src/cap.rs:13-59`；`app-dev-guide.md:492` 已给 `net.access` 命名 |
| 多网卡 | 不支持（内核按类找**一台** virtio-net，`dma_pages=8`、`msix_vectors=2`） | `kernel/src/main.rs:446-460` |

**结论**：现在只有"网卡能收发包 + 一堆自测"，**离"应用能上网"还差一整层**：协议栈要 service 化、要 socket API、要能力授权、要至少一台真网卡验证抽象。

---

## 2. 目标与非目标

**目标**
1. **应用可用**：应用/服务经 socket 风格 API（`socket/connect/send/recv/bind/listen/accept/close`）收发数据；有端到端自测。
2. **能力可管**：绑定/连接端口受**能力**门禁（`net.access`），无能力即被拒（可拒绝取证）。
3. **驱动可换**：驱动层与协议栈解耦 —— 至少再落一台**真网卡**（e1000e），协议栈不变；为多网卡铺路。
4. **协议栈可用**：UDP 完整 socket + **TCP 连接状态机/重传/窗口**（不再只是"握手段"）。

**非目标（明确排除 / 顺延）**
- POSIX libc 全兼容、`epoll`/`select` 语义；TLS、HTTP 服务端、DNS 递归解析器（只做最小 stub 解析）。
- IPv6、IP 分片/重组、拥塞控制（完整 CUBIC/Reno，只做最小）。
- 高性能零拷贝飞地直通网卡（架构蓝图 §网络 3 的"高性能场景"，顺延 E2/E3 之后）。
- 防火墙/流量整形（先在能力层留门，策略引擎顺延 ② 的策略引擎）。

---

## 3. 目标架构（驱动 / 栈 / 应用 三层分离）

```
应用/服务 (app/shell/...)
   │  socket 风格调用 (libnetv: user/libmorion/src/net.rs)
   ▼
netstack_srv  (新服务, 协议栈 + socket 服务)
   │  ARP / IPv4 / ICMP / UDP / TCP  +  端口能力门禁
   │  "送帧 / 收帧" IPC (共享页零拷贝)
   ▼
net_srv (域 16, 纯 NIC 驱动)
   │  多网卡: virtio-net / e1000e / ...
   ▼
网卡硬件
```

- **驱动层只做帧**：`net_srv` 只提供「收一帧 / 发一帧」原语与多网卡索引，不含任何 IP/TCP 语义。
- **协议栈层承 socket**：`netstack_srv` 承载 ARP/IPv4/ICMP/UDP/TCP 与 socket 生命周期，是应用唯一入口。
- **应用经库**：`libnetv` 把 socket 调用转成对 `netstack_srv` 的 IPC（结果走共享页，与 `libvfs` 同款）。

---

## 4. 关键决策（待确认）

| # | 问题 | 候选 | 倾向 |
|---|---|---|---|
| D1 | 栈与驱动是否分离 | (a) 全留在 `net_srv`；(b) **拆 net_srv(驱动) + netstack_srv(栈)**；(c) 拆 ip/tcp/udp 三个服务 | **(b)**：符合蓝图、隔离好、可换网卡；三服务过多，先合并 |
| D2 | 应用接口形态 | (a) 新增 syscall；(b) **IPC + `libnetv` 客户端库** | **(b)**：与文件系统同款，内核不新增网络语义 |
| D3 | 端口/连接能力 | (a) 无门禁；(b) **新增 `Capability::Net{port_lo,port_hi}`** | **(b)**：`bind` 须持覆盖端口的能力；`connect` 出站先放行（后续可加） |
| D4 | 第二网卡选型 | (a) e1000e（QEMU `-device e1000e`）；(b) e1000；(c) RTL8139 | **(a)**：寄存器规整、QEMU 稳定，最能验证"驱动≠栈" |
| D5 | 阻塞 `recv` 语义 | (a) 轮询 + `sys_sleep`；(b) 收包 IRQ 唤醒等待者 | **先 (a)**（确定、易回归），(b) 作为优化 |
| D6 | 多网卡 | (a) 单卡先做；(b) 一上来多卡 + 路由选择 | **先 (a)**，N9 再扩展到多卡 |

---

## 5. 分阶段任务（每步独立可回归）

### N5 — 驱动/栈解耦（等价重构，零行为变化）
- **范围**：把 `net_srv` 拆出「NIC 驱动」职责 —— 只保留 virtqueue 收发 + 多网卡枚举，暴露
  「收一帧 / 发一帧」IPC（`NET_TX`/`NET_RX` + 调用方共享页）；协议栈暂时**仍在 `net_srv` 内**调用这些原语。
- **落点**：`net_srv.rs` 内部重构；`libdevice` 增加网卡无关的"帧缓冲/队列"抽象（可选）。
- **验收**：四道门禁 + 全量回归全绿，`NET1/2/3/4` marker **逐字不变**（纯重构）。

### N6 — `netstack_srv` + UDP socket（新增服务）
- **范围**：新增 `netstack_srv`（新域，沿 9 处接线 + `init` 监督）；迁移 ARP/IPv4/ICMP/UDP 到该服务；
  在 `libmorion` 新增 `net.rs`（`libnetv`）：`socket(AF_INET, SOCK_DGRAM)` / `bind` / `sendto` / `recvfrom` / `close`；
  经 IPC 到 `netstack_srv`，数据走共享页。
- **能力**：内核 `cap.rs` 加 `Capability::Net{port_lo,port_hi}` + `decode`/`pack_audit`/审计白名单（② 的 `cap-audit` 要同步加白）；
  `bind` 无覆盖端口能力 → 拒绝；`init` 给 shell/app 授一条最小端口能力。
- **验收**：`NET5 udp socket OK`（与 slirp 对端 `10.0.2.2` 收发 UDP）；**拒绝取证**：越权端口 `bind` 失败。

### N7 — TCP 完整化 + TCP socket
- **范围**：在 `netstack_srv` 内把最小 TCP 升级为**连接状态机**（LISTEN/SYN_SENT/SYN_RCVD/ESTABLISHED/FIN_WAIT/CLOSE_WAIT/TIME_WAIT）；
  重传（RTO，指数退避）、接收窗口、最小拥塞（慢启动阈值）；TCP socket：`connect`/`listen`/`accept`/`send`/`recv`/`close`。
- **验收**：`NET6 tcp conn OK`：与 QEMU slirp 完成一次**真实**的 HTTP `GET`（对 `10.0.2.2` 的静态响应）或 TCP echo；
  重传路径用"丢一段后仍成功"或确定性自证。

### N8 — 应用面 + DNS 最小解析
- **范围**：`libnetv` 补 `getaddrinfo`（最小，走 UDP 53 到 `NET_DNS`）；shell 增加 `wget <url>`（最小 HTTP 客户端）与
  `nc`（可选）；`app` 自测用 socket 端到端（`NET7 app socket OK`）。落地 `net.access` 能力的正/负例。
- **验收**：`NET7 app socket OK` + `NET8 dns OK, A=…`；能力拒绝路径 marker。

### N9 — 第二真网卡（e1000e）+ 多网卡
- **范围**：`net_srv` 加 e1000e 驱动（QEMU `-device e1000e`，BAR0 MMIO + INTx/MSI）；内核 `pci::find_e1000e` +
  `device::grant`（**多设备**：驱动侧按设备索引区分）；`netstack_srv` 按网卡索引选出口。
- **验收**：`NET9 e1000e OK`（读 MAC + ARP 应答）；两种网卡可同时存在，协议栈对上层不变。

---

## 6. 测试装置与回归判据

- **QEMU**：延续 `-netdev user`（slirp，网关 `10.0.2.2`、DNS `10.0.2.3`）；N9 加 `-device e1000e`。
  自测按"收到应答"判定，**不钉死 MAC/IP**（`roadmap-driver.md` §7-9）。
- **marker 与判据**：`NET5 udp socket OK` / `NET6 tcp conn OK` / `NET7 app socket OK` / `NET8 dns OK` / `NET9 e1000e OK`，
  新增到 `scripts/fs-regress.sh`（沿用现有 `NET1..NET4` 判据，旧 marker 不得回归）。
- **四道门禁**：`fmt/check/clippy` 0 warning、`cargo test --lib` 全过、镜像构建、全量回归 `REGRESS_EXIT=0`；
  改到驱动/IOMMU 额外跑 `IOMMU=1`。
- **加服务的 10 处接线**：见 `docs/dev-workflow.md`（含 `MFS_BOOT_DOMAINS` 对齐常量）——`netstack_srv` 落地时逐条同步。

---

## 7. 风险与未决

1. **加 `Capability::Net` 是内核公共改动**：`cap.rs` 的 `CAP_KINDS`/`decode`/`pack_audit`、用户态 `CAP_KIND_*`、
   ② 的 `cap-audit` 审计策略白名单都要同步（漏一处要么审计误报、要么能力不可用）。
2. **阻塞 I/O 唤醒**：D5 先轮询；真实应用希望"收包即唤醒"，需把 RX IRQ 接到等待者（依赖内核等待队列原语，评估工作量）。
3. **TCP 复杂度**：完整状态机/重传/窗口是本次最大工作量；建议先把"可回归的最小正确子集"做完（N7），再迭代拥塞控制。
4. **驱动抽象边界**：N5 若把抽象做得过重会拖慢；目标是"帧级"最小抽象，够换网卡即可。
5. **未决**：D1 是否最终拆 ip/tcp/udp 三服务（性能/隔离权衡）；是否引入 `Net` 能力还是复用既有 `IoPort`/`SendTo` 组合；
   DNS/HTTP 是否进主线（可能只需最小 stub）。

---

## 8. 相关文档

- `docs/roadmap-driver.md` — N0–N4 已完成记录、§8 风险与相关文档
- `docs/architecture.md` §「网络协议栈——用户态多服务架构」— 蓝图（驱动/栈分离、socket 封装、能力控端口）
- `docs/app-dev-guide.md` — `net.access` 能力命名、服务域表
- `docs/dev-workflow.md` — 加一个服务要同步的 10 处接线
- `docs/plan-fs-streams.md` / `docs/design-iso9660-installer.md` — 同风格的规划/设计稿范例
- [plan-net-v6.md](plan-net-v6.md) — 下一批规划：驱动矩阵 + IPv6（N10 起）

---

## 9. 完成情况（N5–N9 + N8.2 拆分，全部落地）

| 阶段 | 内容 | 关键 marker / 取证 |
|---|---|---|
| N5 | 驱动/栈解耦：`net_srv` 收敛为**纯帧级网卡驱动**（`NETW`：`NET_OP_TX/RX/INFO`） | `NET1..NET4` 逐字不变 |
| N6 | 新增 `netstack_srv`（域 21）+ UDP socket + `Capability::Net{lo,hi}` 端口门禁 | `netstack: up (frame link to net_srv OK)` / `app: NET5 udp socket OK` |
| N6.5/N6.6 | 应用面 socket + 栈内回环 + **virtio-net 12 字节头修复** | `netstack: nic rx (icmp unreachable) OK` |
| N7.1 | TCP 连接状态机 + RTO 重传 + 伪首部校验和 | `NET6 tcp conn OK` / `netstack: tcp peer refused (RST) OK` |
| N7.2 | TCP socket API（客户端）；协议栈状态移入 `static`（避 32 KiB 用户栈溢出） | `app: NET12 tcp client OK` |
| N8 | DNS 最小 A 记录解析（UDP 53 → `10.0.2.3`）+ 应用面汇总 | `app: NET8 dns OK, A=…` / `app: NET7 app socket OK` |
| N9.1 | 第二台真网卡 `e1000e_srv`（域 22）：MMIO + RX/TX 环，MAC 读 RAL/RAH | `NET9 e1000e OK, MAC=…, ARP reply OK` |
| N9.2 | 多网卡出口：`netstack_srv` 按**网卡索引**选出口，上层 socket API 不变 | `netstack: nic1 up (e1000e OK)` / `app: NET11 udp via e1000e (nic1) OK` |
| N8.2 | 回环 TCP 服务端（被动打开）+ 客户机内建 HTTP + shell `wget` | `app: NET13 http OK` |
| N8.2 拆分 | 内建 HTTP → **独立服务 `httpd_srv`（域 23）**；协议栈只留 TCP 原语；连接引入**归属域**；`net.rs` 负载共享页**按域派生** | `httpd: listening on :80` / `app: NET13 http OK` / `cap-audit: domains=24 caps=107 violations=0` |

**与规划稿的差异（最终落地）**：
1. 应用接口 = **IPC + `libnetv`**（`user/libmorion/src/net.rs`），内核不新增网络语义（D2 倾向 b）。
2. 端口能力 = `Capability::Net(lo,hi)` 闭区间；`bind` 经内核登记、协议栈用 `SYS_NET_OWNER` 核对（D3 倾向 b）。
3. HTTP 服务端原内建于 `netstack_srv`，N8.2 拆分后为**独立服务域 `httpd_srv`** —— 对应「加服务的 10 处接线」已逐条同步（`dev-workflow.md`）。
4. 第二网卡选型 = **e1000e**（D4 倾向 a）；多网卡于 N9.2 落地（D6 倾向 b 的扩展）。
5. 完整变更记录见 [dev-reference.md](dev-reference.md) 阶段 90–99。

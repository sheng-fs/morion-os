//! 能力安全模型 — 微内核访问控制的唯一原语
//!
//! 无全局 UID/GID。所有资源访问通过 Capability (能力) 进行:
//!   - 每个进程有一个能力空间 (CapTable)
//!   - 能力类型: Endpoint, Frame, PageTable, IRQ, Domain
//!   - 能力操作: Mint (复制+裁剪权限), Move, Destroy, Revoke
//!   - 复合能力: CNode (存储其他 Cap 的容器)

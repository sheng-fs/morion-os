---
name: agent-architecture-refactor
description: 把能跑但难维护的 Agent 系统改造成模块化、契约化、可观测、可恢复的架构，并完成勘察、实施、测试与文档闭环。用户要求重构 Agent 核心循环、拆分工具/记忆/编排/可观测层、增加 Trace 或 Checkpoint 时使用；不用于从零新建项目、单纯编码或只要架构建议。
---

# Agent 系统架构重构

你同时是**资深架构师**和**亲手落地的工程师**。目标不是写一份架构文档，而是让代码结构、接口契约、测试和文档同时到位。环境允许写文件时直接实施；只能读时，输出可直接落地的补丁级方案和文件级改动说明。

先做勘察和目标架构，然后**继续实施、测试、补文档**，不要停在方案层。只有发现破坏性风险（不可逆数据迁移、公共 API 断裂、无法回滚的重写）时才暂停说明。

## 1. 输入与假设

优先从用户消息中获取：项目路径、技术栈/Agent 框架、入口、允许/禁止改动的范围、测试命令、禁止事项、运行约束（超时、模型、密钥来源）。

缺失时按顺序自行解析，不要反复询问：

1. 项目路径：当前 workspace；多根工作区时选包含用户所提文件的根。
2. 入口与测试：README、Makefile、package.json、pyproject.toml、Cargo.toml、go.mod、CI 配置。
3. 约束：现有 lint/test 配置、公开 API、数据库 schema、依赖清单。

把每个假设写进勘察报告的「假设」一节。不要用自然语言约定代替可执行的契约；不要把密钥写进代码或文档。

## 2. 阶段 0：勘察

阅读代码库，定位：

- 入口与请求生命周期；
- Agent 核心循环（上下文拼装、决策、工具调用、终止）；
- 工具定义与执行；
- 记忆读写与检索；
- 编排/工作流/重试/人工介入；
- 配置、日志、Trace、测试。

找出「上帝模块」和耦合点：一个文件/类同时做决策、执行工具、读写数据库、管理记忆、拼日志时，逐条记录它的职责、调用方、修改影响面。

输出《勘察报告》，至少包含：

- 模块清单（路径 + 职责 + 对外接口）；
- 问题清单（按严重度排序，附文件/行号）；
- 耦合图或依赖方向；
- 风险与假设；
- 改造优先级（先测试/可观测，再契约，再拆模块，最后编排）。

## 3. 阶段 1：目标架构与契约

给出 Mermaid 架构图和模块职责表。推荐分层，但按项目规模裁剪：

```mermaid
flowchart TD
  U[User / API] --> O[Orchestration]
  O --> C[Agent Core]
  C --> P[Decision Policy / LLM]
  C --> TR[Tool Registry]
  TR --> TE[Tool Executor]
  C --> M[Memory Interface]
  M --> MS[Memory Store]
  C --> T[Tracer]
  TE --> T
  O --> CP[Checkpoint Store]
  CP --> S[(State)]
```

| 层 | 只负责 | 不负责 |
|---|---|---|
| 编排层 | 流程、重试、暂停/恢复、人工介入 | 具体工具细节、记忆存储实现 |
| Agent 核心 | 拼上下文、决定调用哪个工具或终止 | 执行工具、读写数据库、管理记忆 |
| 工具执行 | 注册、参数校验、超时、沙箱、结果封装 | 决定何时调用 |
| 记忆层 | 读写、检索、摘要、淘汰 | 决策与编排 |
| 可观测 | Trace、结构化日志、Token/耗时/错误分类 | 业务决策 |
| 状态持久化 | Checkpoint、恢复、回溯 | 上下文拼装 |

定义最小契约（用项目现有 schema 库，如 Pydantic、dataclass、TypeScript 类型、Rust struct）：

- `AgentDecision`：`action`（call_tool / final / ask_human）、`tool_name`、`arguments`、`reason`、`schema_version`。
- `ToolSpec`：`name`、`description`、`input_schema`、`output_schema`、`timeout_ms`、`side_effect`、`idempotent`。
- `ToolResult`：`ok`、`data`、`error{category, code, message, retryable}`、`duration_ms`、`trace_id`。
- `MemoryRecord`：`id`、`kind`、`content`、`metadata`、`created_at`、`source`、`ttl`。
- `TraceEvent`：`trace_id`、`span_id`、`parent_span_id`、`event_type`、`timestamp`、`duration_ms`、`model`、`tokens`、`tool`、`input_digest`、`output_digest`、`error_category`。
- `Checkpoint`：`checkpoint_id`、`task_id`、`turn`、`state`、`pending_tool_calls`、`memory_ref`、`created_at`、`schema_version`。

接口保持最小：`DecisionPolicy`、`ToolRegistry`、`ToolExecutor`、`MemoryStore`、`Orchestrator`、`Tracer`、`CheckpointStore`。具体实现（SBB）可替换，核心循环只依赖接口。

明确「必须做」与「暂不做」。小项目优先**模块化单 Agent + 契约 + 可观测**，不要为架构而架构，不要强行拆多 Agent。

## 4. 阶段 2：渐进实施

按以下顺序小步改，每步都能单独验证和回滚：

1. **基线**：先补 characterization/golden 测试，记录当前行为；加最小 Trace ID 与结构化日志。
2. **契约**：定义 schema 与接口；先用适配器包住现有实现，保持行为不变。
3. **拆工具**：把工具执行移出核心循环，引入 `ToolRegistry` + `ToolExecutor`；核心只产出 `AgentDecision`。
4. **拆记忆**：核心只依赖 `MemoryStore`；把读写/检索/摘要移到独立模块。
5. **拆状态**：加 `CheckpointStore`；支持暂停、恢复、回溯；工具调用要幂等或可去重。
6. **编排**：确有多个职责单元时才引入多 Agent/工作流；否则保持单 Agent 编排。
7. **文档与自检**：ADR、运行手册、自检清单。

每个改动记录：文件、改动说明、验证命令、风险、回滚方式。禁止一次性重写；禁止破坏现有公共 API；禁止新增依赖（除非用户明确允许）；禁止把配置散落在代码里；禁止硬编码密钥；禁止用自然语言描述 JSON 再做脆弱解析。

新增工具必须走注册/插件：实现 `ToolSpec` + 执行函数/类，注册到 `ToolRegistry`；核心循环不得改动。

## 5. 阶段 3：测试与验证

补单元测试与关键流程集成测试，并逐项验证：

- 核心循环不直接执行工具（用 mock/spy 证明它只调用接口）；
- 新工具只通过注册表接入，核心文件零改动；
- 记忆只经 `MemoryStore` 读写，替换 fake store 后核心行为不变；
- 工具参数/输出符合 schema，非法参数被拒绝；
- Trace ID 贯穿模型调用、工具调用、日志、Checkpoint；
- 暂停后从 Checkpoint 恢复，已完成步骤不重复产生副作用；
- 错误分类正确（validation / model / tool / timeout / permission / state / unknown），可重试性明确；
- 行为兼容（重构前后 golden 测试一致）。

运行项目测试命令并报告结果。失败就修复后重跑；不要用「测试环境问题」跳过。

## 6. 阶段 4：文档与自检

输出 ADR：为什么这样分层、契约如何定义、取舍是什么、哪些暂不做。更新 README/运行手册：如何新增工具、如何配置模型、如何查看 Trace、如何恢复任务。

最终报告固定为七节：

1. 勘察报告
2. 目标架构与契约设计
3. 实施步骤与每步验证
4. 实际改动摘要（文件级）
5. 测试结果
6. ADR 与运维文档
7. 自检清单与剩余风险

自检清单逐项标记「通过 / 未通过 / 原因」：核心职责单一、工具与记忆解耦、显式契约、新增工具零改核心、Trace 全链路、Checkpoint 可恢复、测试通过、行为兼容、文档可维护。

## 7. 护栏

- 先测试/可观测，再抽契约，再拆模块，最后接编排。
- 不做大爆炸重写；每一步可回滚。
- 不破坏公共 API；必须变更时先加兼容层并记录迁移方案。
- 不新增依赖、不改数据库结构，除非用户明确允许。
- 不把密钥写进代码；配置集中管理。
- 不为了「看起来像多 Agent」而拆多 Agent。
- 不确定时做最小合理假设并记录；只有破坏性风险才暂停。
- 如果只能读不能写，输出补丁级方案与文件级改动说明，不要假装已实施。

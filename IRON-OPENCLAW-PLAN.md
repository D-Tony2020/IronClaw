# Iron-OpenClaw 开发计划 (v3 — 五轮评审修订版)

> **目标：在 IronClaw (Rust) 的安全架构之上，完整复刻 OpenClaw 的多 Agent 编排能力，打造一个安全且功能完备的 AI OS。**

## 参考库

| 库 | 路径 | 角色 |
|----|------|------|
| Raw-OpenClaw | `/Users/max/Desktop/Raw-OpenClaw/` | TypeScript 参考实现（只读） |
| IronClaw | `/Users/max/Desktop/IronClaw/` | 当前 Rust 代码库（在此基础上开发） |

---

## 评审修订摘要

> 本版经过五轮架构师评审（R1: Agent 生命周期 & 并发 / R2: 消息流 & 路由 / R3: 工程落地 / R4: v2 自洽性验证 / R5: 最终确认），共修复 8 项 BLOCKING 问题、14 项 HIGH 问题。

### 关键修订 (v2→v3 增量)

| 修订 | v2 方案 | v3 修订 |
|------|---------|---------|
| **出站推送** | 新增 Channel trait `send_to()` 方法 | 复用现有 `broadcast()`，不改 Channel trait |
| **WASM 新字段** | 隐含需要修改 WIT 签名 | 通过 `metadata-json` 桥接，WIT 不变，向后兼容 |
| **BeforeOutbound Hook** | 出站路径中遗漏 | 在 `multi_loop.rs` 出站 spawn 块中恢复 hook 触发 |
| **RAII MessageContextGuard** | 新增 RAII guard | 删除（async Drop 不可行 + Actor 串行已保证安全） |
| **AgentDeps Clone** | 声明 derive Clone | 逐字段验证 Clone 可行性 + 编译时断言 |
| **event triggers** | 未迁移到 inbox loop | 在 `start_inbox_loop` 中 handle 后调用 |
| **routines FK** | 无外键处理 | 添加 `PRAGMA foreign_keys = OFF/ON` 包裹 |
| **辅助任务归属** | 未说明 | 新增任务归属表 (全局 vs per-agent) |

### 全部关键修订 (v1→v3 汇总)

| 修订 | 原方案 (v1) | 最终方案 (v3) |
|------|-------------|---------------|
| **新增 Phase 0.5** | 无 | Agent 消息循环重构 — Actor inbox 模式 |
| **出站投递** | 依赖 `channels.respond(&message)` | `DeliveryPlan` + `OutboundRouter`，复用 `broadcast()` |
| **IncomingMessage** | 仅 channel + user_id | 新增字段通过 `metadata-json` 从 WASM 桥接，WIT 不变 |
| **并发安全** | tokio::spawn + 全局 context | Actor inbox 串行 + `set_message_tool_context` 保持现有模式 |
| **Config 模型** | env + toml + DB 三源 | Config file = 定义，DB = 状态，reconciliation |
| **ThreadKey** | 新增必选 agent_id | `agent_id: Option<String>`，默认 None 兼容 |
| **Binding schema** | 仅 channel + user_pattern | peer_id 精确匹配 + account_id |
| **Hook 系统** | 随 run() 循环内置 | 显式在出站路径中恢复 BeforeOutbound hook |
| **工作量估算** | ~2900 行 | ~4200 行 |

---

## 关键发现：IronClaw 已有的 Multi-Agent 基础设施

IronClaw 的数据库和 Workspace 层**已经为 multi-agent 预留了接口**，但从未被上层调用：

| 组件 | 现状 | 说明 |
|------|------|------|
| `memory_documents.agent_id` | ✅ 列已存在 | `UNIQUE(user_id, agent_id, path)` |
| `heartbeat_state.agent_id` | ✅ 列已存在 | `UNIQUE(user_id, agent_id)` |
| `Workspace.agent_id` | ✅ 字段已存在 | `with_agent(uuid)` builder 从未被调用 |
| `WorkspaceStore` trait | ✅ 方法签名已支持 | 所有方法接受 `agent_id: Option<Uuid>` |
| `conversations.agent_id` | ❌ 不存在 | 需新增 |
| `agent_jobs.agent_id` | ❌ 不存在 | 需新增 |
| `routines.agent_id` | ❌ 不存在 | 需新增（需 table recreation — UNIQUE 约束限制） |
| `SessionManager` agent 感知 | ❌ 不存在 | `ThreadKey` 无 `agent_id` |
| `Config.agent` | ❌ 单数 | 需改为多 agent 配置 |

---

## Phase 0: DB Schema Migration + Agent 注册表

**目标：** 建立 agents 表，扩展现有表的 agent_id 列。

### 文件变更

**`src/db/libsql_migrations.rs`** — 新增 migration：

```sql
-- Agent 注册表
CREATE TABLE IF NOT EXISTS agents (
    id TEXT PRIMARY KEY,                -- UUID
    agent_id TEXT NOT NULL UNIQUE,      -- 人类可读 ID: "main", "newsbot", "tutor"
    display_name TEXT,
    description TEXT,
    is_default INTEGER DEFAULT 0,       -- 默认 agent
    enabled INTEGER DEFAULT 1,
    config_json TEXT DEFAULT '{}',      -- 运行时状态 (reconcile 自 config file)
    workspace_prefix TEXT,              -- "agents/newsbot/"
    created_at TEXT DEFAULT (datetime('now')),
    updated_at TEXT DEFAULT (datetime('now'))
);

-- Agent 路由绑定 (v2: 精确化字段)
CREATE TABLE IF NOT EXISTS agent_bindings (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    channel TEXT,                        -- "wechat", "telegram", "*"
    account_id TEXT DEFAULT '*',         -- channel 内的账号标识 (v2: 默认通配)
    peer_id TEXT,                        -- 精确用户/群组 ID (v2: 替代 user_pattern)
    peer_type TEXT,                      -- "dm", "group", NULL=全匹配
    priority INTEGER DEFAULT 0,         -- 高优先级先匹配
    enabled INTEGER DEFAULT 1,
    created_at TEXT DEFAULT (datetime('now')),
    FOREIGN KEY (agent_id) REFERENCES agents(agent_id)
);

-- 扩展现有表
ALTER TABLE conversations ADD COLUMN agent_id TEXT DEFAULT 'default';
ALTER TABLE agent_jobs ADD COLUMN agent_id TEXT DEFAULT 'default';

-- routines 表需 recreation (UNIQUE 约束不支持 ALTER TABLE ADD COLUMN)
-- v3: 必须禁用外键 (routine_runs 有 FK 引用 routines)
PRAGMA foreign_keys = OFF;
-- 步骤: CREATE routines_new → INSERT INTO ... SELECT → DROP routines → ALTER TABLE RENAME
CREATE TABLE IF NOT EXISTS routines_new (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    agent_id TEXT DEFAULT 'default',      -- v2: 新增
    trigger_type TEXT NOT NULL,
    trigger_config TEXT NOT NULL,
    action_type TEXT NOT NULL,
    action_config TEXT NOT NULL,
    enabled INTEGER DEFAULT 1,
    last_run TEXT,
    next_run TEXT,
    created_at TEXT DEFAULT (datetime('now')),
    updated_at TEXT DEFAULT (datetime('now')),
    UNIQUE(name, agent_id)               -- v2: per-agent unique
);
INSERT INTO routines_new SELECT id, name, 'default', trigger_type, trigger_config,
    action_type, action_config, enabled, last_run, next_run, created_at, updated_at
    FROM routines;
DROP TABLE routines;
ALTER TABLE routines_new RENAME TO routines;
PRAGMA foreign_keys = ON;  -- v3: 恢复外键约束
```

> ⚠️ **v2 修订**: `user_pattern` 替换为 `peer_id`（精确匹配语义，对齐 OpenClaw `binding.match.peer.id`）。`routines` 表 recreation 处理 UNIQUE 约束。

**`src/db/mod.rs`** — 新增 `AgentStore` trait：

```rust
#[async_trait]
pub trait AgentStore: Send + Sync {
    // Agent CRUD
    async fn create_agent(&self, agent: &AgentRecord) -> Result<(), DatabaseError>;
    async fn get_agent(&self, agent_id: &str) -> Result<Option<AgentRecord>, DatabaseError>;
    async fn list_agents(&self) -> Result<Vec<AgentRecord>, DatabaseError>;
    async fn update_agent(&self, agent: &AgentRecord) -> Result<(), DatabaseError>;
    async fn delete_agent(&self, agent_id: &str) -> Result<(), DatabaseError>;
    async fn get_default_agent(&self) -> Result<Option<AgentRecord>, DatabaseError>;

    // Binding CRUD
    async fn create_binding(&self, binding: &AgentBindingRecord) -> Result<(), DatabaseError>;
    async fn list_bindings(&self, agent_id: &str) -> Result<Vec<AgentBindingRecord>, DatabaseError>;
    async fn list_all_bindings(&self) -> Result<Vec<AgentBindingRecord>, DatabaseError>;
    async fn delete_binding(&self, id: &str) -> Result<(), DatabaseError>;
}
```

**`src/db/libsql_migrations.rs`** — PostgreSQL 对称 migration：

> ⚠️ **v2 新增**: 所有 libSQL migration 必须同步维护 PostgreSQL 版本（`src/db/postgres_migrations.rs`），保持 schema 对称。

**参考 OpenClaw：** `src/config/types.agents.ts` 的 `AgentConfig` + `AgentBinding` 类型。

### 测试

- [ ] `agents` 表 CRUD 单元测试
- [ ] `agent_bindings` 表 CRUD 单元测试（含 peer_id / peer_type 精确匹配）
- [ ] `routines` 表 recreation 验证（原数据不丢失）
- [ ] 现有表 `ALTER TABLE` 兼容性验证
- [ ] 默认 agent 创建与查询
- [ ] PostgreSQL migration 对称性

---

## Phase 0.5: Agent 消息循环重构 (v2 新增)

> **评审结论**: 这是最关键的基础设施变更，但工程量很低（~120 行改动）。所有后续 Phase 依赖此重构。

**目标：** 将 `Agent::run(self)` 分解，使 Agent 可被多实例共享并接受外部消息投递。

### 问题分析

当前 `Agent::run(self)` 的 3 个耦合问题：

```
1. run(self) 消费 self → 无法 Arc<Agent> 共享
2. run() 内部持有 message_stream → Agent 绑定到 ChannelManager
3. handle_message() 结果通过 run() 循环中的 channels.respond() 发送 → 回复逻辑与消息循环耦合
```

### 重构方案：Actor 模式

```
Before:
  Agent::run(self) {
      loop { msg = stream.next() → self.handle_message(&msg) → channels.respond() }
  }

After:
  Agent {
      inbox: mpsc::Receiver<AgentEnvelope>    // 外部投递消息
      fn start(self: Arc<Self>)               // 启动 inbox 消费循环 (不消费 self)
      fn handle_message(&self, envelope)      // 处理消息并通过 envelope.reply_tx 回复
  }
```

### 文件变更

**`src/agent/agent_loop.rs`** — 核心重构（~120 行变更）：

```rust
use tokio::sync::{mpsc, oneshot};

/// 消息信封 — 将 IncomingMessage 与回复通道打包
/// (v2: 解决 tokio::spawn 断裂返回路径问题)
pub struct AgentEnvelope {
    pub message: IncomingMessage,
    pub reply_tx: oneshot::Sender<Result<Option<String>, Error>>,
}

/// Agent inbox handle (用于外部投递)
pub type AgentInbox = mpsc::Sender<AgentEnvelope>;

impl Agent {
    /// v2: 分离出不消费 self 的启动方法
    /// 单 agent 模式下仍可使用 run() 作为便捷入口
    pub async fn run(self) -> Result<(), Error> {
        let (inbox_tx, inbox_rx) = mpsc::channel(64);
        let agent = Arc::new(self);

        // 启动 channel → inbox 桥接
        let bridge = Self::bridge_channels_to_inbox(
            agent.channels.clone(), inbox_tx
        );

        // 启动 inbox 消费循环
        let loop_handle = agent.clone().start_inbox_loop(inbox_rx);

        // 启动附属任务 (repair, heartbeat, routines)
        let aux = agent.start_auxiliary_tasks();

        tokio::select! {
            _ = bridge => {}
            _ = loop_handle => {}
            _ = tokio::signal::ctrl_c() => {}
        }

        agent.shutdown().await
    }

    /// 多 agent 模式下直接创建 inbox
    pub fn create_inbox(&self) -> (AgentInbox, mpsc::Receiver<AgentEnvelope>) {
        mpsc::channel(64)
    }

    /// 启动 inbox 消费循环 (不消费 self)
    /// v3: routine_engine 作为参数注入（避免修改 Agent struct）
    pub async fn start_inbox_loop(
        self: Arc<Self>,
        mut inbox: mpsc::Receiver<AgentEnvelope>,
        routine_engine: Option<Arc<RoutineEngine>>,  // v3: 从 run() 或 AgentInstance 注入
    ) {
        while let Some(envelope) = inbox.recv().await {
            let result = self.handle_message_with_context(&envelope.message).await;
            let _ = envelope.reply_tx.send(result);

            // v3: 迁移 event trigger 检查到 inbox loop
            // (原在 run() 主循环的 handle_message 之后)
            if let Some(ref engine) = routine_engine {
                let fired = engine.check_event_triggers(&envelope.message).await;
                if fired > 0 {
                    tracing::debug!("Fired {} event-triggered routines", fired);
                }
            }
        }
    }

    /// v3: 替代原 handle_message，绑定 per-call context
    /// (v3 修订: 移除 RAII guard — async Drop 不可行，Actor 串行已保证安全)
    async fn handle_message_with_context(
        &self,
        message: &IncomingMessage,
    ) -> Result<Option<String>, Error> {
        // 设置 message tool context (保持现有 API，Actor 串行保证无并发覆盖)
        let target = message.metadata
            .get("signal_target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| message.user_id.clone());

        self.tools()
            .set_message_tool_context(Some(message.channel.clone()), Some(target))
            .await;

        self.handle_message(message).await
    }

    /// Channel stream → inbox 桥接
    async fn bridge_channels_to_inbox(
        channels: Arc<ChannelManager>,
        inbox: AgentInbox,
    ) {
        let mut stream = channels.start_all().await.expect("channels start");
        while let Some(msg) = stream.next().await {
            let (reply_tx, reply_rx) = oneshot::channel();
            if inbox.send(AgentEnvelope { message: msg.clone(), reply_tx }).await.is_ok() {
                // 等待处理结果并回复 channel
                if let Ok(Ok(Some(response))) = reply_rx.await {
                    let _ = channels.respond(&msg, OutgoingResponse::text(response)).await;
                }
            }
        }
    }
}
```

> **v3 修订**: 移除了 v2 的 `MessageContextGuard` RAII 方案。原因：(1) `Drop` trait 不支持 async，无法在 drop 中执行 `async fn set_context()`；(2) Actor 串行模式已保证同一 Agent 内不会并发调用 `handle_message`，RAII 是冗余的。保持现有 `set_message_tool_context()` API 不变。

### 向后兼容

- `Agent::run(self)` 保留，内部调用 `Arc::new(self).start_inbox_loop()`
- 单 agent 模式行为完全不变
- 所有现有测试无需修改

### 测试

- [ ] 单 agent `run()` 行为不变（回归测试）
- [ ] `AgentEnvelope` 投递 + reply_tx 回复链路
- [ ] reply_tx drop (agent panic) → 调用方收到错误响应（非静默丢失）
- [ ] 并发投递到同一 Agent inbox → 串行处理验证
- [ ] event trigger 在 inbox loop 中正常触发
- [ ] 编译时 `assert_send::<Result<Option<String>, Error>>()` 通过

---

## Phase 1: AgentRegistry + 多 Agent 初始化

**目标：** 支持在一个进程内创建和管理多个 Agent 实例。

### 设计决策 (v2 修订)

**Config 来源 reconciliation：**

```
                    ┌─────────────┐
                    │ config.toml │  ← 声明式定义 (source of truth)
                    │ [agents]    │
                    └──────┬──────┘
                           │ reconcile on startup
                           ▼
                    ┌─────────────┐
                    │   DB agents │  ← 运行时状态 (merge/upsert)
                    │   table     │
                    └──────┬──────┘
                           │ initialize
                           ▼
                    ┌─────────────┐
                    │ AgentRegistry│  ← 内存运行时 (source of Arc<Agent>)
                    └─────────────┘
```

- **config file 定义 agent 列表**（不在 DB 中手动创建 agent）
- **DB 存储运行时状态**（last_active, runtime metrics, 动态禁用）
- **启动时 reconcile**：config 新增的 agent → insert into DB；config 删除的 agent → soft-disable in DB
- `.env` 只用于全局参数（端口、API key），不定义 agent

### 新增文件

**`src/agent/registry.rs`** — Agent 注册表：

```rust
/// 参考 OpenClaw: src/agents/agent-scope.ts
pub struct AgentRegistry {
    agents: RwLock<HashMap<String, Arc<AgentInstance>>>,
    store: Arc<dyn Database>,
    default_agent_id: RwLock<String>,
}

/// 包含 Agent 运行时状态和配置
pub struct AgentInstance {
    pub agent_id: String,                       // "main", "newsbot"
    pub db_id: Uuid,                            // DB 主键 (用于 workspace scoping)
    pub config: AgentInstanceConfig,            // Per-agent 配置
    pub agent: Arc<Agent>,                      // Agent 实例
    pub inbox: AgentInbox,                      // v2: Actor inbox sender
    pub workspace: Arc<Workspace>,              // 独立 workspace
    pub session_manager: Arc<SessionManager>,   // 独立 session 管理
    pub routine_engine: Option<Arc<RoutineEngine>>,
}

/// Per-agent 配置覆盖 (参考 OpenClaw: AgentConfig)
#[derive(Clone)]  // v2: 必须 Clone
pub struct AgentInstanceConfig {
    pub display_name: String,
    pub enabled: bool,
    pub model_override: Option<String>,         // Per-agent LLM model
    pub fallback_model: Option<String>,
    pub enabled_skills: Option<Vec<String>>,    // Skill allowlist (None = all)
    pub enabled_tools: Option<Vec<String>>,     // Tool allowlist (None = all)
    pub system_prompt_extra: Option<String>,    // 额外系统提示
    pub heartbeat: Option<HeartbeatConfig>,     // Per-agent 定时
    pub max_parallel_jobs: Option<usize>,
    pub dm_scope: DmScope,                      // v2: Session 隔离策略
}

/// v2 新增: Session 隔离策略 (参考 OpenClaw: dmScope)
#[derive(Clone, Default)]
pub enum DmScope {
    #[default]
    Main,               // 所有 DM 共享一个 session (默认，单用户场景)
    PerPeer,            // 每个用户独立 session
    PerChannelPeer,     // 每个 (channel, user) 组合独立 session
}

impl AgentRegistry {
    /// 从 config file + DB 加载所有 agents
    /// v2: reconciliation 模式
    pub async fn initialize(
        store: Arc<dyn Database>,
        config: &Config,
        base_deps: &AgentDeps,     // v2: AgentDeps 可 Clone
    ) -> Result<Self> {
        // 1. Reconcile: config.agents.list → upsert into DB
        // 2. Load all enabled agents from DB
        // 3. For each: create Agent + inbox + workspace + session_manager
        // 4. Start each agent's inbox loop (tokio::spawn)
    }

    /// 根据 agent_id 获取 Agent 实例
    pub async fn get(&self, agent_id: &str) -> Option<Arc<AgentInstance>>;

    /// 获取默认 agent
    pub async fn get_default(&self) -> Arc<AgentInstance>;

    /// 列出所有 agent
    pub async fn list(&self) -> Vec<Arc<AgentInstance>>;

    /// 运行时添加 agent (热添加)
    pub async fn register(&self, record: AgentRecord, deps: &AgentDeps) -> Result<()>;

    /// v2: 运行时移除 agent (带 drain)
    pub async fn unregister(&self, agent_id: &str) -> Result<()> {
        // 1. 从 registry 移除 (不再接受新消息)
        // 2. Drop inbox sender → inbox loop 自然结束
        // 3. 等待 inflight 消息处理完成 (inbox.closed())
        // 4. Shutdown agent 附属任务
    }
}
```

### 变更文件

**`src/config/mod.rs`** — 配置结构扩展：

```rust
pub struct Config {
    // ... 现有字段不变 ...
    pub agent: AgentConfig,          // 保留，作为全局 default
    pub agents: AgentsConfig,        // 新增：多 agent 配置
}

/// 参考 OpenClaw: AgentsConfig
pub struct AgentsConfig {
    pub defaults: AgentDefaultsConfig,    // 共享默认值
    pub list: Vec<AgentDefinition>,       // Agent 定义列表
}

pub struct AgentDefinition {
    pub id: String,                       // "newsbot"
    pub display_name: Option<String>,
    pub default: bool,
    pub model: Option<String>,
    pub fallback_model: Option<String>,
    pub skills: Option<Vec<String>>,
    pub workspace_dir: Option<PathBuf>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub dm_scope: Option<DmScope>,        // v2: Session 隔离策略
    pub enabled: bool,
}
```

**`src/agent/agent_loop.rs`** — AgentDeps derive Clone (v3: 逐字段验证)：

```rust
// v3: 所有字段 Clone 可行性验证
#[derive(Clone)]
pub struct AgentDeps {
    pub store: Option<Arc<dyn Database>>,              // ✅ Option<Arc<T>>
    pub llm: Arc<dyn LlmProvider>,                     // ✅ Arc<dyn T>
    pub cheap_llm: Option<Arc<dyn LlmProvider>>,       // ✅
    pub safety: Arc<SafetyLayer>,                       // ✅
    pub tools: Arc<ToolRegistry>,                       // ✅
    pub workspace: Option<Arc<Workspace>>,              // ✅
    pub extension_manager: Option<Arc<ExtensionManager>>, // ✅
    pub skill_registry: Option<Arc<std::sync::RwLock<SkillRegistry>>>, // ✅
    pub skill_catalog: Option<Arc<SkillCatalog>>,       // ✅
    pub skills_config: SkillsConfig,                    // ⚠️ 需确认 derive Clone
    pub hooks: Arc<HookRegistry>,                       // ✅
    pub cost_guard: Arc<CostGuard>,                     // ✅
    pub sse_tx: Option<broadcast::Sender<SseEvent>>,    // ✅ broadcast::Sender is Clone
    pub http_interceptor: Option<Arc<dyn HttpInterceptor>>, // ✅
}
// 前置条件: SkillsConfig 必须 derive Clone (如未有，需先添加)
```

> v3: 添加编译时静态断言确保 `Error: Send`（跨 tokio::spawn 需要）：
> ```rust
> const _: () = { fn assert_send<T: Send>() {} fn check() { assert_send::<Result<Option<String>, crate::error::Error>>(); } };
> ```

### 辅助任务归属 (v3 新增)

当前 `Agent::run()` 启动的辅助任务在多 agent 场景下的归属：

| 辅助任务 | 归属 | 说明 |
|----------|------|------|
| self-repair (stuck job 检测) | 全局 | 检查所有 agent 的 stuck jobs |
| session pruning | per-agent | 每个 AgentInstance 有独立 SessionManager |
| heartbeat | per-agent | 每个 agent 独立心跳配置和调度 |
| routine engine + cron ticker | per-agent | 每个 agent 独立 routine 集合 |
| notification forwarder | per-agent → OutboundRouter | 通过 `OutboundRouter::plan_push()` 投递 |

**`src/main.rs`** — 启动流程改造：

```
当前:  Config → 单个 Agent::new() → agent.run()
目标:  Config → AgentRegistry::initialize() → 多个 AgentInstance (各自 inbox loop)
                → OutboundRouter + AgentDispatcher → main_loop()
```

### 测试

- [ ] AgentRegistry 创建 + 多 agent 初始化
- [ ] Config reconciliation: config 新增 agent → DB 同步
- [ ] Config reconciliation: config 删除 agent → DB soft-disable
- [ ] 默认 agent 解析
- [ ] Per-agent workspace 隔离验证
- [ ] Agent 热添加/移除（含 drain 验证）
- [ ] DmScope 配置生效

---

## Phase 2: AgentDispatcher + OutboundRouter — 消息路由与出站投递

**目标：** 实现入站路由 + 出站投递，完整覆盖消息生命周期。

> ⚠️ **v2 重大修订**: 原方案只有入站路由，缺失出站投递设计（评审 BLOCKING 问题 B1）。本 Phase 合并入站路由 + 出站投递。

### 新增文件

**`src/agent/dispatcher.rs`** — 入站路由：

```rust
/// 参考 OpenClaw: src/routing/resolve-route.ts
pub struct AgentDispatcher {
    registry: Arc<AgentRegistry>,
    store: Arc<dyn Database>,
    bindings_cache: RwLock<Vec<AgentBindingRecord>>,
}

impl AgentDispatcher {
    /// 核心路由：IncomingMessage → AgentInstance
    ///
    /// 路由优先级 (参考 OpenClaw resolveAgentRoute):
    /// 1. message.target_agent (显式指定，如 /agent newsbot)
    /// 2. AgentBinding 匹配 (channel + account_id + peer_id + peer_type)
    /// 3. Session 历史 (同一 ThreadKey 上次路由到的 agent)
    /// 4. Default agent
    ///
    /// v2: Binding 匹配使用精确字段而非 glob pattern
    pub async fn route(&self, message: &IncomingMessage) -> Arc<AgentInstance>;

    /// 刷新 binding 缓存 (从 DB 重新加载)
    pub async fn refresh_bindings(&self);
}

/// Binding 匹配逻辑 (v2: 精确匹配)
/// 参考 OpenClaw: src/routing/resolve-route.ts 的 7 层匹配
///
/// 简化为 4 层 (单用户场景不需要 guild/team/roles):
/// 1. channel + account_id + peer_id + peer_type → 精确匹配
/// 2. channel + account_id + peer_type → 账号级匹配
/// 3. channel + peer_type → channel 级匹配
/// 4. channel only → 最宽泛匹配
fn match_binding(
    binding: &AgentBindingRecord,
    channel: &str,
    account_id: &str,
    peer_id: &str,
    peer_type: Option<&str>,
) -> Option<u32>;  // 返回匹配分数，None = 不匹配
```

**`src/agent/outbound.rs`** — 出站投递 (v2 新增)：

```rust
/// 参考 OpenClaw: src/infra/outbound/agent-delivery.ts
///
/// 解决: Agent 回复 / Routine 产出 / Heartbeat 结果 → 发送到正确的 channel + user
pub struct OutboundRouter {
    channels: Arc<ChannelManager>,
}

/// 投递计划 (参考 OpenClaw: AgentDeliveryPlan)
pub struct DeliveryPlan {
    /// 目标 channel 名 ("wechat", "telegram", "web")
    pub channel: String,
    /// 投递目标 (user_id / group_id)
    pub to: String,
    /// 来源账号 (channel 内的 bot 账号标识)
    pub account_id: Option<String>,
    /// Thread ID (用于线程式回复)
    pub thread_id: Option<String>,
    /// 投递模式
    pub mode: DeliveryMode,
}

pub enum DeliveryMode {
    /// 回复原始消息 (有 IncomingMessage 可用)
    Reply { original: IncomingMessage },
    /// 主动推送 (Routine/Heartbeat 场景，无原始消息)
    Push,
}

impl OutboundRouter {
    /// 从 IncomingMessage 构建回复投递计划
    /// v2: turnSourceChannel 语义 — 回复必须回到消息来源 channel
    pub fn plan_reply(message: &IncomingMessage) -> DeliveryPlan {
        DeliveryPlan {
            channel: message.channel.clone(),
            to: message.metadata
                .get("signal_target")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| message.user_id.clone()),
            account_id: message.account_id.clone(),
            thread_id: message.thread_id.clone(),
            mode: DeliveryMode::Reply { original: message.clone() },
        }
    }

    /// 从 Routine 配置构建推送投递计划
    pub fn plan_push(
        channel: &str,
        to: &str,
        thread_id: Option<&str>,
    ) -> DeliveryPlan {
        DeliveryPlan {
            channel: channel.to_string(),
            to: to.to_string(),
            account_id: None,
            thread_id: thread_id.map(|s| s.to_string()),
            mode: DeliveryMode::Push,
        }
    }

    /// 执行投递
    /// v3: Push 模式复用现有 broadcast()，不新增 Channel trait 方法
    pub async fn deliver(
        &self,
        plan: &DeliveryPlan,
        response: OutgoingResponse,
    ) -> Result<(), ChannelError> {
        match &plan.mode {
            DeliveryMode::Reply { original } => {
                self.channels.respond(original, response).await
            }
            DeliveryMode::Push => {
                // v3: 复用已有的 broadcast(channel, user_id, response)
                self.channels.broadcast(
                    &plan.channel,
                    &plan.to,
                    response,
                ).await
            }
        }
    }
}
```

**`src/channels/channel.rs`** — 扩展 IncomingMessage (v2+v3)：

```rust
pub struct IncomingMessage {
    // ... 现有字段 ...
    pub id: Uuid,
    pub channel: String,
    pub user_id: String,
    pub user_name: Option<String>,
    pub content: String,
    pub thread_id: Option<String>,
    pub received_at: DateTime<Utc>,
    pub metadata: serde_json::Value,

    // v2 新增 (解决 BLOCKING B2)
    pub account_id: Option<String>,       // channel 内的 bot 账号标识
    pub peer_type: Option<PeerType>,      // DM / Group / Channel
    pub peer_id: Option<String>,          // 精确对话标识 (群组 ID 等)
    pub target_agent: Option<String>,     // 显式指定目标 agent
}

/// v2 新增
#[derive(Debug, Clone)]
pub enum PeerType {
    DirectMessage,
    Group,
    Channel,
}
```

> **v3 WASM 兼容性**: 新增字段不修改 WIT `emitted-message` record。WASM Channel 通过 `metadata-json` 传递这些信息，Host 端在 `emitted-message → IncomingMessage` 转换时提取：
>
> ```rust
> // Host 端转换 (src/channels/wasm/host.rs 或类似文件):
> let account_id = metadata.get("account_id").and_then(|v| v.as_str()).map(String::from);
> let peer_type = metadata.get("peer_type").and_then(|v| v.as_str())
>     .and_then(|s| match s { "dm" => Some(PeerType::DirectMessage), "group" => Some(PeerType::Group), _ => None });
> let peer_id = metadata.get("peer_id").and_then(|v| v.as_str()).map(String::from);
> let target_agent = metadata.get("target_agent").and_then(|v| v.as_str()).map(String::from);
> ```
>
> 不需要重新编译现有 WASM channel 二进制。长期可在 WIT 0.3.0 正式添加。

**`src/channels/manager.rs`** — 无需新增方法 (v3 修订)：

> v3: `ChannelManager` 已有 `broadcast(channel_name, user_id, response)` 方法，语义与 `send_to()` 完全一致。不需要新增 Channel trait 方法。
>
> ⚠️ **v3 WASM broadcast 修复** (R5 发现): 当前 WASM channel 的 `broadcast()` 实现（`wrapper.rs`）忽略了 `user_id` 参数（使用 `_user_id` 前缀），而是使用 `last_broadcast_metadata`。需修改 WASM `broadcast()` 将 `user_id` 合并到 `metadata_json` 中传递给 `on_respond`，确保 Push 模式能正确指定投递目标。单用户场景当前可工作（只有一个微信用户），但应在 Phase 2 一并修复。

### 主循环改造

**新增 `src/agent/multi_loop.rs`** (v2 修订版)：

```rust
/// 多 Agent 主循环 (v2: 使用 inbox 投递 + OutboundRouter)
///
/// 替代原来的 Agent::run() 中的 select! 循环
pub async fn run_multi_agent_loop(
    dispatcher: Arc<AgentDispatcher>,
    outbound: Arc<OutboundRouter>,
    channels: Arc<ChannelManager>,
) {
    let mut stream = channels.start_all().await.expect("channels start");

    loop {
        let message = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => break,
            msg = stream.next() => match msg {
                Some(m) => m,
                None => break,
            }
        };

        // 1. 路由到目标 agent
        let agent_instance = dispatcher.route(&message).await;

        // 2. 构建投递计划 (v2: turnSourceChannel 语义)
        let plan = OutboundRouter::plan_reply(&message);

        // 3. 通过 inbox 投递给 agent (不用 tokio::spawn — inbox loop 本身已在独立 task)
        let (reply_tx, reply_rx) = oneshot::channel();
        if agent_instance.inbox.send(AgentEnvelope {
            message: message.clone(),
            reply_tx,
        }).await.is_err() {
            tracing::error!("Agent {} inbox closed", agent_instance.agent_id);
            continue;
        }

        // 4. 异步等待回复并投递 (v3: 含 BeforeOutbound hook)
        let outbound = outbound.clone();
        let hooks = agent_instance.agent.hooks().clone();  // v3: hook registry
        let msg_clone = message.clone();  // v3: for hook event
        tokio::spawn(async move {
            match reply_rx.await {
                Ok(Ok(Some(response))) if !response.is_empty() => {
                    // v3: BeforeOutbound hook (恢复 v1 中 run() 循环的 hook 调用)
                    let event = crate::hooks::HookEvent::Outbound {
                        user_id: msg_clone.user_id.clone(),
                        channel: plan.channel.clone(),
                        content: response.clone(),
                        thread_id: msg_clone.thread_id.clone(),
                    };
                    let final_response = match hooks.run(&event).await {
                        Err(err) => {
                            tracing::warn!("BeforeOutbound hook blocked: {}", err);
                            return;
                        }
                        Ok(crate::hooks::HookOutcome::Continue { modified: Some(new) }) => new,
                        _ => response,
                    };

                    if let Err(e) = outbound.deliver(
                        &plan,
                        OutgoingResponse::text(final_response),
                    ).await {
                        tracing::error!("Outbound delivery failed: {}", e);
                    }
                }
                Ok(Err(e)) => {
                    // v3: Agent error → 回复错误消息给用户（非静默丢失）
                    tracing::error!("Agent error: {}", e);
                    let _ = outbound.deliver(
                        &plan,
                        OutgoingResponse::text(format!("Error: {}", e)),
                    ).await;
                }
                Err(_) => {
                    // v3: reply_tx dropped (agent panic) → 通知用户
                    tracing::error!("Agent inbox loop crashed");
                    let _ = outbound.deliver(
                        &plan,
                        OutgoingResponse::text("Internal error, please retry.".to_string()),
                    ).await;
                }
                _ => {} // None response — no reply needed
            }
        });
    }
}
```

### Session 隔离 (v2 修订)

**`src/agent/session_manager.rs`** — ThreadKey 扩展：

```rust
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct ThreadKey {
    user_id: String,
    channel: String,
    agent_id: Option<String>,           // v2: Optional 保持兼容，默认 None
    external_thread_id: Option<String>,
}
```

> ⚠️ **v2 修订**: `agent_id` 为 `Option<String>` 而非必选。现有 session 的 ThreadKey（agent_id=None）hash 值不变，保持向后兼容。新 agent 创建的 session 带 `Some("newsbot")` 自动隔离。

### 参考 OpenClaw

```
OpenClaw: src/routing/resolve-route.ts
  resolveAgentRoute(ctx):
    1. ctx.explicitAgentId → 直接使用
    2. queryBindings(channel, accountId, peer) → 匹配 binding
    3. findSessionAgent(ctx.sessionKey) → session 历史
    4. resolveDefaultAgentId(config) → 默认 agent

OpenClaw: src/infra/outbound/agent-delivery.ts
  resolveAgentDeliveryPlan(session, opts):
    1. turnSourceChannel → 回复必须回到来源 channel
    2. session.lastChannel → 回退到上次 channel
    3. INTERNAL_MESSAGE_CHANNEL → 无外部 channel 时内部处理
```

### 测试

- [ ] 路由优先级测试 (显式 > binding > 历史 > default)
- [ ] Binding 匹配测试 (channel, account_id, peer_id, peer_type, 通配符)
- [ ] Session 隔离测试 (同 user 不同 agent 的 session 不共享)
- [ ] ThreadKey 兼容性 (旧 session 无 agent_id 仍可访问)
- [ ] OutboundRouter.plan_reply() → 回复到原始 channel
- [ ] OutboundRouter.plan_push() → 复用 `broadcast()` 主动推送
- [ ] BeforeOutbound hook 在出站路径中触发 (阻止 / 修改)
- [ ] Agent error → 用户收到错误消息（非静默丢失）
- [ ] IncomingMessage 新字段从 metadata-json 正确提取 (WASM 兼容)
- [ ] 并发消息投递到同一 Agent inbox → 串行处理

---

## Phase 3: Per-Agent 配置 (Model / Skills / Tools)

**目标：** 每个 Agent 可以有独立的 LLM model、Skills 过滤、Tools 限制。

### 变更

**`src/agent/agent_loop.rs`** — Agent 初始化时应用 per-agent 配置：

```rust
impl AgentInstance {
    /// 创建 per-agent 的 AgentDeps
    /// v2: AgentDeps 已 derive Clone，直接 clone 再覆盖
    fn build_deps(
        base_deps: &AgentDeps,
        agent_config: &AgentInstanceConfig,
    ) -> AgentDeps {
        let mut deps = base_deps.clone();

        // Per-agent model override
        // 参考 OpenClaw: src/agents/model-selection.ts resolveDefaultModelForAgent()
        if let Some(model) = &agent_config.model_override {
            deps.llm = create_llm_for_model(model, &agent_config.fallback_model);
        }

        // Per-agent tool filtering
        if let Some(allowed_tools) = &agent_config.enabled_tools {
            deps.tools = Arc::new(deps.tools.filtered(allowed_tools));
        }

        // Per-agent skill filtering
        if let Some(allowed_skills) = &agent_config.enabled_skills {
            deps.skill_registry = filter_skills(deps.skill_registry, allowed_skills);
        }

        deps
    }
}
```

**`src/tools/registry.rs`** — 新增 `filtered()` 方法：

```rust
impl ToolRegistry {
    /// 返回仅包含 allowed_names 中工具的子 registry
    pub fn filtered(&self, allowed_names: &[String]) -> ToolRegistry;
}
```

**`src/workspace/mod.rs`** — 确保 `with_agent()` 被调用：

```rust
// AgentInstance 创建时:
let workspace = Workspace::new_with_db(&user_id, db.clone())
    .with_agent(agent_db_uuid);  // 关键：启用 agent 隔离
```

### Per-Agent Identity Files

每个 Agent 有独立的 identity 文件目录：

```
~/.ironclaw/agents/main/
  ├── AGENTS.md      (agent 身份描述)
  ├── SOUL.md        (人格设定)
  ├── MEMORY.md      (持久记忆)
  ├── HEARTBEAT.md   (定时任务指令)
  └── TOOLS.md       (可用工具说明)

~/.ironclaw/agents/newsbot/
  ├── AGENTS.md
  ├── SOUL.md
  ├── MEMORY.md
  ├── HEARTBEAT.md
  └── TOOLS.md
```

### 参考 OpenClaw

```
OpenClaw: src/config/types.agents.ts

interface AgentConfig {
  id: string;
  default?: boolean;
  workspace?: string;                  → workspace_prefix
  model?: { primary, fallback };       → model_override, fallback_model
  skills?: string[];                   → enabled_skills
  heartbeat?: HeartbeatConfig;         → heartbeat
  subagents?: SubagentConfig;          → (future)
}
```

### 测试

- [ ] Per-agent LLM model override
- [ ] Per-agent tool filtering (agent A 有 shell，agent B 没有)
- [ ] Per-agent skill 过滤
- [ ] Per-agent workspace 文件隔离
- [ ] Per-agent identity file 加载

---

## Phase 4: Gateway API — Agent 管理

**目标：** 通过 Web API 管理 agents 和 bindings。

### 新增 API 端点

**`src/channels/web/server.rs`** — 新增路由：

```
# Agent 管理
GET    /api/agents              → 列出所有 agents
POST   /api/agents              → 创建 agent (runtime add)
GET    /api/agents/:id          → 获取 agent 详情 (含 inbox queue 深度)
PUT    /api/agents/:id          → 更新 agent 配置
DELETE /api/agents/:id          → 删除 agent (soft-disable)
POST   /api/agents/:id/enable   → 启用 agent
POST   /api/agents/:id/disable  → 禁用 agent

# Binding 管理
GET    /api/agents/:id/bindings         → 列出 agent 的 bindings
POST   /api/agents/:id/bindings         → 创建 binding
DELETE /api/agents/:id/bindings/:bid    → 删除 binding

# 路由测试
POST   /api/agents/route-test   → 测试消息路由 (给定 channel/user, 返回匹配的 agent)

# Session (扩展)
GET    /api/agents/:id/sessions → 列出 agent 的 sessions
```

### Web UI 更新

**`src/channels/web/static/`** — Agent 管理 Tab：

- Agent 列表 (名称、状态、model、绑定数、inbox queue depth)
- Agent 创建/编辑表单
- Binding 规则编辑器
- 路由测试面板

### OpenAI-Compatible API 扩展

```
POST /v1/chat/completions
  新增 body 参数: "agent_id": "newsbot"  (可选，默认路由)
```

### 测试

- [ ] Agent CRUD API
- [ ] Binding CRUD API
- [ ] 路由测试端点
- [ ] OpenAI API 的 agent_id 参数
- [ ] Web UI agents tab (手动测试)

---

## Phase 5: Per-Agent Heartbeat & Routines

**目标：** 每个 Agent 可以有独立的定时任务和 Routine。

### 变更

**`src/agent/routine_engine.rs`** — Per-agent 实例化：

```rust
// 每个 AgentInstance 有自己的 RoutineEngine
pub struct AgentInstance {
    // ...
    pub routine_engine: Option<Arc<RoutineEngine>>,
}

// RoutineEngine 增加 agent_id 感知
impl RoutineEngine {
    pub fn with_agent_id(mut self, agent_id: String) -> Self;

    // 加载时只取属于当前 agent 的 routines
    async fn load_routines(&self) -> Vec<Routine> {
        self.store.list_routines_for_agent(&self.agent_id).await
    }
}
```

**`src/db/mod.rs`** — RoutineStore 扩展：

```rust
async fn list_routines_for_agent(&self, agent_id: &str) -> Result<Vec<RoutineRecord>>;
async fn create_routine_for_agent(&self, agent_id: &str, routine: &RoutineRecord) -> Result<()>;
```

### Per-Agent Heartbeat (v2 修订: 经过 session 系统)

```rust
// v2: Heartbeat 结果通过 AgentEnvelope 投递，经过 session 系统
// (原方案直接调 LLM，绕过了 session，无历史上下文)

impl AgentInstance {
    async fn run_heartbeat(&self) {
        let message = IncomingMessage::new(
            "internal",           // 内部 channel
            "system",             // 系统用户
            "/heartbeat",         // 触发 heartbeat 处理
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.inbox.send(AgentEnvelope { message, reply_tx }).await;

        // 使用 OutboundRouter.plan_push() 投递到配置的 target channel
        if let Ok(Ok(Some(response))) = reply_rx.await {
            let plan = OutboundRouter::plan_push(
                &self.config.heartbeat_target_channel,
                &self.config.heartbeat_target_user,
                None,
            );
            outbound.deliver(&plan, OutgoingResponse::text(response)).await;
        }
    }
}
```

### 参考 OpenClaw

```
OpenClaw: src/infra/heartbeat-runner.ts

startHeartbeatRunner():
  for each agent in config.agents.list:
    if isHeartbeatEnabledForAgent(config, agent.id):
      schedule(runHeartbeatOnce(agent.id), agent.heartbeat.every)

runHeartbeatOnce(agentId):
  load agentConfig
  load session (per-agent)             ← v2: 修正 — 经过 session
  spawn agent with heartbeat prompt
  deliver result to agent's target channel
```

### 测试

- [ ] Per-agent routine 隔离
- [ ] Per-agent heartbeat 独立调度
- [ ] Heartbeat 经过 session 系统（有历史上下文）
- [ ] Active hours 遵守
- [ ] Routine 创建时指定 agent_id
- [ ] Heartbeat 结果通过 OutboundRouter 投递到 target channel

---

## Phase 6: Agent Lifecycle Hooks

**目标：** Hook 系统增加 agent 生命周期事件。

### 新增 Hook Points

```rust
pub enum HookPoint {
    // 现有
    BeforeInbound,
    BeforeOutbound,
    BeforeToolCall,
    OnMessage,
    OnSessionStart,
    OnSessionEnd,
    TransformResponse,

    // 新增 — agent 生命周期
    OnAgentStart,           // Agent 实例启动
    OnAgentStop,            // Agent 实例停止 (含 drain 完成)
    OnAgentSwitch,          // 消息从一个 agent 切换到另一个
}
```

### Hook 的 Agent 作用域

```rust
pub struct HookEvent {
    // 现有
    pub point: HookPoint,
    pub message: Option<IncomingMessage>,

    // 新增
    pub agent_id: Option<String>,       // 触发此 hook 的 agent
}

// Hook 注册时可指定 agent 范围
pub struct HookRegistration {
    pub point: HookPoint,
    pub agent_filter: Option<String>,   // None = 全局, Some("newsbot") = 仅 newsbot
    pub handler: HookHandler,
}
```

### Per-Agent Workspace Hooks

```
~/.ironclaw/agents/newsbot/hooks/
  ├── hooks.json              (hook 定义)
  └── on_message.hook.json    (具体 hook 配置)
```

### 测试

- [ ] Agent 生命周期 hook 触发
- [ ] Per-agent hook 过滤
- [ ] Workspace hooks 加载

---

## Phase 7: Media Handling

**目标：** 支持图片、音频、PDF 处理。

### 新增模块

**`src/media/`** — 媒体处理模块：

```rust
src/media/
  ├── mod.rs           // MediaProcessor trait
  ├── image.rs         // 图片缩放/转码 (image crate)
  ├── audio.rs         // 音频转写 (Whisper API)
  ├── pdf.rs           // PDF 文本提取 (pdf-extract crate)
  └── mime.rs          // MIME 类型检测
```

### IncomingMessage 媒体扩展

```rust
pub struct IncomingMessage {
    // 现有字段 ...
    pub attachments: Vec<MediaAttachment>,  // 新增
}

pub struct MediaAttachment {
    pub media_type: MediaType,  // Image, Audio, Video, Document
    pub mime_type: String,
    pub url: Option<String>,    // 远程 URL
    pub data: Option<Vec<u8>>,  // 内联数据
    pub filename: Option<String>,
    pub size_bytes: Option<u64>,
}
```

### WASM Channel 媒体支持

WeChat WASM channel 需扩展以传递图片/语音消息：

```rust
// wechat WASM channel on_http_request:
// 识别 MsgType=image/voice，提取 MediaUrl/Recognition
// 通过 emit_message 附带 media metadata
```

### 测试

- [ ] 图片 resize 和格式转换
- [ ] PDF 文本提取
- [ ] 音频转写集成
- [ ] WeChat 图片消息处理
- [ ] WeChat 语音消息处理

---

## Phase 8: Channel Health Monitor + 高可用

**目标：** 自动检测 channel 异常并重启。

### 新增

```rust
pub struct ChannelHealthConfig {
    pub check_interval: Duration,    // 默认 5 分钟
    pub restart_on_failure: bool,
    pub max_restart_attempts: u32,
}

pub struct ChannelHealthMonitor {
    channels: Arc<ChannelManager>,
    config: ChannelHealthConfig,
}

impl ChannelHealthMonitor {
    pub async fn run(&self) {
        loop {
            for channel in self.channels.list().await {
                if !channel.is_healthy().await {
                    tracing::warn!(channel = %channel.name(), "Channel unhealthy, restarting");
                    channel.restart().await;
                }
            }
            tokio::sleep(self.config.check_interval).await;
        }
    }
}
```

### 测试

- [ ] Channel health check 定时执行
- [ ] 不健康 channel 自动重启
- [ ] 重启次数上限

---

## Future Work (不在当前计划范围内)

| 特性 | 说明 | 前置条件 |
|------|------|---------|
| Agent 间通信 | Agent A 调用 Agent B 处理子任务（OpenClaw `subagents` 概念）| P2 AgentRegistry + inbox |
| WIT 0.3.0 升级 | 在 `emitted-message` record 中正式添加路由字段 | P2 完成后评估 |
| Session LRU 淘汰 | 对 SessionManager 实施 LRU 策略防止内存无限增长 | P0.5 完成后评估 |
| multi-user 支持 | 多用户隔离 (当前为单用户单机场景) | P2 binding + DmScope |

---

## 开发优先级和依赖关系

```
Phase 0 (DB Schema)
    │
    ▼
Phase 0.5 (Agent 消息循环重构)     ← v2 新增，最关键
    │
    ▼
Phase 1 (AgentRegistry + Config Reconciliation)
    │
    ├──────────────┐
    ▼              ▼
Phase 2         Phase 3
(Dispatcher     (Per-Agent Config)
 + Outbound)
    │              │
    ├──────────────┘
    ▼
Phase 4 (Gateway API)
    │
    ▼
Phase 5 (Per-Agent Routines + Heartbeat via Session)
    │
    ▼
Phase 6 (Agent Hooks)

Phase 7 (Media) ── 独立，可并行
Phase 8 (Health Monitor) ── 独立，可并行
```

## 工作量估算 (v2 修订)

| Phase | 新增代码量 (估) | 改动文件数 | 复杂度 | 预计时间 |
|-------|----------------|-----------|--------|---------|
| P0: DB Schema | ~250 行 | 4 (含 PG 对称) | 低 | 1 session |
| **P0.5: Agent 循环重构** | **~350 行** | **3** | **高** | **2 sessions** |
| P1: AgentRegistry + Config | ~750 行 | 7 (含 config parser) | **高** | 3 sessions |
| P2: Dispatcher + Outbound | ~650 行 | 5 (复用 broadcast) | **高** | 2-3 sessions |
| P3: Per-Agent Config | ~300 行 | 6 | 中 | 1-2 sessions |
| P4: Gateway API | ~400 行 | 3 | 中 | 1-2 sessions |
| P5: Per-Agent Routines | ~300 行 | 4 | 中 | 1-2 sessions |
| P6: Agent Hooks | ~150 行 | 2 | 低 | 1 session |
| P7: Media | ~600 行 | 5+ | 中 | 2-3 sessions |
| P8: Health Monitor | ~150 行 | 2 | 低 | 1 session |
| **总计** | **~3900 行** | | | **15-19 sessions** |

> v3 vs v2: P0.5 减少 50 行（移除 RAII guard），P1 增加 150 行（config parser 扩展），P2 减少 50 行（复用 broadcast），总体更精确。

## 向后兼容性保证

1. **零 agent 配置 = 行为不变：** 如果 `agents.list` 为空，系统自动创建一个名为 "default" 的 agent，使用全局 `Config.agent` 配置，效果与当前 IronClaw 完全一致。

2. **现有 .env 不需修改：** `AGENT_NAME` env var 作为默认 agent 的名称保留。

3. **数据库迁移安全：** 所有 `ALTER TABLE ADD COLUMN` 都带 `DEFAULT` 值。`routines` 表使用 recreation 保留数据。

4. **ThreadKey 兼容：** `agent_id: Option<String>` 默认 None，现有 session hash 不变。

5. **API 兼容：** 所有现有 Gateway API 保持不变，新 API 都在 `/api/agents/` 命名空间下。

6. **Agent::run() 保留：** 单 agent 模式仍可直接调用 `agent.run()`，内部透明使用 inbox。

## 安全边界（Iron-OpenClaw 继承 IronClaw）

| 安全能力 | 状态 |
|----------|------|
| WASM 沙箱 (Channel + Tool) | ✅ 继承 |
| AES-256-GCM 凭证加密 | ✅ 继承 |
| 运行时凭证注入 (WASM 看不到密钥) | ✅ 继承 |
| Prompt Injection 4 层防护 | ✅ 继承 |
| Leak Detection (15+ patterns) | ✅ 继承 |
| Shell env 清洗 | ✅ 继承 |
| Docker sandbox + Network Proxy | ✅ 继承 |
| SSRF WASM allowlist | ✅ 继承 |
| **新增：Per-agent tool 权限隔离** | 🆕 P3 |
| **新增：Per-agent workspace 隔离** | 🆕 P1 (利用已有 agent_id 基础设施) |
| **新增：Per-agent session 隔离** | 🆕 P2 (ThreadKey agent_id) |
| **新增：Actor inbox 并发安全** | 🆕 P0.5 |

---

## 评审 Issue 对照表

| Issue | 严重度 | 修复 Phase | 修复方式 | 引入版本 |
|-------|--------|-----------|----------|---------|
| B1: 无出站投递设计 | BLOCKING | P2 | `OutboundRouter` + `DeliveryPlan` + 复用 `broadcast()` | v2 |
| B2: IncomingMessage 缺路由元数据 | BLOCKING | P2 | 新增字段 + metadata-json 桥接 (WIT 不变) | v2+v3 |
| B3: tokio::spawn 断裂返回路径 | BLOCKING | P0.5 | `AgentEnvelope` + `oneshot::Sender` | v2 |
| B4: set_message_tool_context 全局状态 | BLOCKING | P0.5 | Actor 串行保证 (移除 RAII) | v2+v3 |
| B5: Channel trait 无 send_to() | BLOCKING | P2 | 复用已有 `broadcast()` | v3 |
| B6: BeforeOutbound hook 遗漏 | BLOCKING | P2 | 在出站 spawn 块中恢复 hook 触发 | v3 |
| B7: AgentDeps 未 derive Clone | BLOCKING | P1 | 逐字段验证 + derive Clone | v3 |
| B8: WASM WIT 签名兼容性 | BLOCKING | P2 | metadata-json 桥接，不改 WIT | v3 |
| H1: Config 三源冲突 | HIGH | P1 | Config file = 定义，DB = 状态，reconciliation | v2 |
| H2: ThreadKey hash 兼容性 | HIGH | P2 | `agent_id: Option<String>` 默认 None | v2 |
| H3: SessionManager 内存无上限 | HIGH | P0.5 | 预留 LRU 淘汰接口 (P2 实现) | v2 |
| H4: Agent 热移除无 drain | HIGH | P1 | Drop inbox sender → loop 自然结束 + await | v2 |
| H5: user_pattern 语义含糊 | HIGH | P0 | 替换为 `peer_id` 精确匹配 | v2 |
| H6: Binding schema 不足 | HIGH | P0 | 新增 peer_id, peer_type, account_id | v2 |
| H7: 无 dmScope | HIGH | P1 | `DmScope` enum in AgentInstanceConfig | v2 |
| H8: Heartbeat 绕过 session | HIGH | P5 | Heartbeat 通过 inbox 投递，经过 session | v2 |
| H9: routines 外键约束 | HIGH | P0 | `PRAGMA foreign_keys = OFF/ON` 包裹 | v3 |
| H10: event triggers 未迁移 | HIGH | P0.5 | inbox loop 中 handle 后调用 | v3 |
| H11: reply_tx drop 静默丢失 | HIGH | P2 | 错误分支回复用户错误消息 | v3 |
| H12: 辅助任务归属未定义 | HIGH | P1 | 任务归属表 (全局 vs per-agent) | v3 |
| H13: Config parser 扩展 | HIGH | P1 | config.toml [agents] section 解析 | v3 |
| H14: Error Send 断言 | HIGH | P0.5 | 编译时 assert_send 静态断言 | v3 |

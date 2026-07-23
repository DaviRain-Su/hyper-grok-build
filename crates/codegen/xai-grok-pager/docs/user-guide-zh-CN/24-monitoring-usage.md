# 监控用量（外部 OpenTelemetry）

> **状态：alpha。** 下方 schema 已版本化（`grok_code.schema.version = v1`）；
> 可能在不另行通知的情况下做加法式变更；重命名/删除会提升版本号，
> 并在 changelog 中说明。

Grok CLI 可将用量 **指标（metrics）** 与 **事件（events）** 导出到贵组织
自有的 OpenTelemetry collector，便于平台团队在整个机群范围内监控采用情况、token
消耗、工具权限决策与错误——且数据不会经过 SpaceXAI。

## 相关设置

以下开关彼此独立（也与本指南中的外部 OTEL 流无关）：

| 设置 | 如何配置 |
|---------|---------------|
| 遥测总开关 | `[features] telemetry` / `GROK_TELEMETRY_ENABLED` |
| `/privacy` | `/privacy opt-in` / `/privacy opt-out`，或通过 Settings |
| Trace 上传 | `[telemetry] trace_upload` / `GROK_TELEMETRY_TRACE_UPLOAD` |
| 外部 OpenTelemetry | `GROK_EXTERNAL_OTEL` / `[telemetry] otel_*`（本指南） |

另见 [身份验证](02-authentication.md#related-settings) 与
[配置](05-configuration.md#telemetry)。

## 外部 OTEL 流

外部流具备以下特点：

- **默认关闭**，且需要 *双重选择加入*（总开关 **以及**
  显式选择 exporter）。
- **默认不含内容**：无 prompt、无代码、无文件路径（仅扩展名）、
  无工具参数、无 bash 命令；MCP/skill/plugin 名称折叠为类别。可选内容门控可重新启用其中部分项。
- **与 SpaceXAI 内部遥测在结构上分离**：其 exporter 仅携带
  你配置的 headers，永不携带 SpaceXAI 凭据。
- **独立于 SpaceXAI 数据保留选择退出**：即使
  `telemetry` 已禁用，以及 ZDR（零数据保留）团队，该流仍可工作。那些设置
  管控的是 SpaceXAI 侧的保留策略；外部流仅由你自己的 OTEL 配置管控。

## 快速开始

```bash
export GROK_EXTERNAL_OTEL=1                  # master switch
export OTEL_METRICS_EXPORTER=otlp
export OTEL_LOGS_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf  # or grpc
export OTEL_EXPORTER_OTLP_ENDPOINT=https://collector.corp.example:4318
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <collector-token>"
grok
```

仅设置 `GROK_EXTERNAL_OTEL=1` **不会启用任何功能**——还必须至少选择
一个 exporter。反之，仅设置 `OTEL_*` 变量而没有总开关，
同样不会启用任何功能。

## 环境变量

| 变量 | 默认值 | 含义 |
|---|---|---|
| `GROK_EXTERNAL_OTEL` | `0` | 总开关。与 `GROK_TELEMETRY_ENABLED` 不同，后者控制 SpaceXAI 内部产品分析——二者管控的是方向相反的数据流。 |
| `OTEL_METRICS_EXPORTER` | `none` | `otlp` \| `console` \| `none`。 |
| `OTEL_LOGS_EXPORTER` | `none` | `otlp` \| `console` \| `none`。控制事件流的门控。 |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `http/protobuf` | `http/protobuf` \| `grpc`。 |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | HTTP 为 `http://localhost:4318`，gRPC 为 `http://localhost:4317` | 基础 endpoint。对 `http/protobuf`，会按 OTLP 规范追加 `/v1/logs` 与 `/v1/metrics`；对 `grpc`，collector endpoint 按原样使用。 |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` / `..._METRICS_ENDPOINT` | — | 按信号覆盖的 endpoint，按字面使用。对 gRPC，通常应为不含 `/v1/...` 路径的 collector endpoint。 |
| `OTEL_EXPORTER_OTLP_HEADERS`（及按信号的变体） | — | Collector 鉴权（`k=v,k2=v2`）。这是外部 exporter **唯一**会发送的 headers，也是唯一支持的 collector 鉴权机制（无配置文件 headers 键——token 永不落盘）。 |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | `10000`（毫秒） | 导出超时。 |
| `OTEL_METRIC_EXPORT_INTERVAL` | `60000`（毫秒） | 指标导出间隔。 |
| `OTEL_BLRP_SCHEDULE_DELAY`（或别名 `OTEL_LOGS_EXPORT_INTERVAL`） | `5000`（毫秒） | 日志批处理间隔。 |
| `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` | `delta` | `delta` \| `cumulative`。 |
| `OTEL_METRICS_INCLUDE_SESSION_ID` | `1` | 将 `session.id` 附加到指标（基数选择退出）。 |
| `OTEL_METRICS_INCLUDE_VERSION` | `0` | 将 `app.version` 附加到指标。 |
| `OTEL_LOG_USER_PROMPTS` | `0` | 内容门控：在 `grok_code.user_prompt` 上包含 prompt 文本（上限 60 KB，经密钥擦除）。 |
| `OTEL_LOG_TOOL_DETAILS` | `0` | 内容门控：工具参数（上限 4 KB）、完整文件路径、原样 MCP/skill/plugin 名称。Bash 命令文本在 v1 中 **永不**导出，即使开启此门控。 |

`OTEL_RESOURCE_ATTRIBUTES` 被有意忽略：resource 由固定的、
经审计的属性集合构建。

> **迁移说明：** 旧版本可能将 `OTEL_EXPORTER_OTLP_*` 与
> 产品自身的分析管道共用。该行为已弃用：当设置了
> `GROK_EXTERNAL_OTEL` 时，产品分析会忽略这些变量；若产品分析已消费过这些变量，
> CLI 会拒绝以任何此类配置激活外部流——你的 collector 只会接收
> 你已选择加入的外部流。

## 配置文件

组织默认值位于 `config.toml` 中既有的 `[telemetry]` 表下
（环境变量优先）。键名是其他
`[telemetry]` 设置的 `otel_` 前缀对应项：

```toml
[telemetry]
otel_enabled = true
otel_metrics_exporter = "otlp"
otel_logs_exporter = "otlp"
otel_endpoint = "https://collector.corp.example:4318"
otel_protocol = "http/protobuf"  # or "grpc"
otel_log_user_prompts = false   # admins can pin these via requirements
otel_log_tool_details = false
```

配置键为 `[telemetry]` 下的 `otel_*`；**环境变量保留其
标准 OTEL 名称**（`GROK_EXTERNAL_OTEL`、`OTEL_*`）以便生态互通，
因此两层刻意使用不同命名空间。
`otel_protocol` 配置键映射到 `OTEL_EXPORTER_OTLP_PROTOCOL`。

有意不提供 `headers` 键：请通过
`OTEL_EXPORTER_OTLP_HEADERS` 提供 collector 鉴权，使 token 永不落盘。

受管部署还可通过 `grok setup` 的托管配置 / requirements 固定项分发
`[telemetry]` 的 `otel_*` 键来启用组织范围遥测，或通过相同的本地配置层
强制在机群范围禁用（`external_otel_disabled`、内容门控锁定）。

## Resource 属性

| 属性 | 值 |
|---|---|
| `service.name` | `grok-cli` |
| `service.version`、`client.version` | 构建/客户端版本 |
| `app.entrypoint` | `cli` \| `headless` \| `agent` |
| `terminal.type` | 终端模拟器品牌 |
| `grok_code.schema.version` | `v1` |

身份属性（`user.id`，以及已知时的 `organization.id` / `team.id` /
`deployment.id`）在身份验证完成后附加到每个指标数据点与每个事件。
`prompt.id`（每个 prompt 的 UUID）仅出现在事件上，永不出现在指标上。

## 指标（meter scope `ai.xai.grok_code`）

| 指标 | 单位 | 属性 |
|---|---|---|
| `grok_code.session.count` | `{session}` | 仅基础属性 |
| `grok_code.token.usage` | `{token}` | `type` = `input` \| `output` \| `reasoning` \| `cache_read`；`model` |
| `grok_code.turn.count` | `{turn}` | `outcome` = `completed` \| `cancelled` \| `error`；`model` |
| `grok_code.tool.decision` | `{decision}` | `tool_name`，`decision` = `allow` \| `deny` \| `cancelled` \| `followup`，`access_kind`，`permission_mode` |
| `grok_code.tool.usage` | `{call}` | `tool_name`，`outcome` |
| `grok_code.error.count` | `{error}` | `error_category`，`model` |

没有 `cost.usage` 指标：请将 `grok_code.token.usage` 与你自己的
价目表关联。`lines_of_code.count` 与 `active_time.total` 计划在
后续阶段提供。

`tool_name` 取值：内置工具名按原样传递；MCP 工具折叠为
`mcp_tool`，其他非内置工具折叠为 `custom_tool`，除非
`OTEL_LOG_TOOL_DETAILS=1`。

## 事件（OTLP log records）

每个事件都携带 `event.sequence`、`session.id`、`turn_number`（回合内）、
`prompt.id`，以及身份属性。门控图例：**details** =
需要 `OTEL_LOG_TOOL_DETAILS`，**prompts** = 需要
`OTEL_LOG_USER_PROMPTS`；其余项在流处于活动状态时始终导出。

| `event.name` | 属性 |
|---|---|
| `grok_code.session_start` | `model`，`permission_mode`，`mcp_server_count`，`plugin_count`，`skill_count`，`hook_count`，`memory_enabled`，`is_git_repo`，`client_identifier` |
| `grok_code.session_end` | `duration_secs`，`turn_count`，`tool_call_count`，`compaction_count`，`model` |
| `grok_code.user_prompt` | `prompt_length`，`model`，`screen_mode?`（`fullscreen` \| `inline` \| `minimal` \| `headless` \| `other`）；`prompt`（**prompts**） |
| `grok_code.turn_completed` | `outcome`，`duration_ms`，`tool_call_count`，`model`，`error_category?`，`cancellation_category?` |
| `grok_code.api_request` | `model`，`duration_ms`，`stop_reason?`，`input_tokens`，`output_tokens`，`reasoning_tokens`，`cache_read_tokens` |
| `grok_code.api_error` | `error_category`，`model`，`status_code?`，`duration_ms?` |
| `grok_code.tool_result` | `tool_name`，`outcome`，`success`，`duration_ms`，`file_extension`；`tool_parameters`，`file_path`（**details**） |
| `grok_code.tool_decision` | `tool_name`，`decision`，`access_kind`，`permission_mode`，`source` |
| `grok_code.mcp_server_connection` | `status`，`transport_type`，`duration_ms`，`tool_count?`，`error_type?`；`mcp_server.name`（**details**；否则折叠为 `mcp_server`） |
| `grok_code.permission_mode_changed` | `to_mode`，`trigger` |
| `grok_code.skill_activated` | `skill_source`；`skill.name`（**details**） |
| `grok_code.plugin_loaded` | `install_kind?`，`success`，`error_category?`；`plugin_name`（**details**） |
| `grok_code.compaction` | `duration_ms`，`tokens_before`，`tokens_after`，`model?` |
| `grok_code.subagent` | `phase` = `launched` \| `completed`，`subagent_type?`，`outcome?`，`duration_ms?` |
| `grok_code.auth` | `auth_method` |
| `grok_code.internal_error` | `error_type`（仅类名——无 message、无 location） |
| `grok_code.model_switched` | `from_model`，`to_model`，`success`，`error_code?` |

## 隐私模型

三道彼此独立的默认拒绝（fail-closed）机制守护线上格式：

1. **类型化 schema**：属性键为封闭枚举；其外的任何内容
   都无法附加。
2. **发出时脱敏**：每个字符串都会经过密钥形态擦除与
   主目录擦除，并截断（每个值 512→128 字符、工具参数 4 KB、
   prompt 上限 60 KB）。
3. **导出时校验**：任何携带非 schema 键、已关闭门控的键、
   或未擦除密钥形态的记录，在离开进程前会被丢弃；带有越界 schema 属性键的指标导出
   会被整条丢弃。

永不导出：bash 命令文本、错误消息正文、prompt 文本
（无门控时）、文件路径（无门控时）、`api_key.id`、机器
指纹、电子邮件地址、订阅层级。

## Collector 配置示例

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
      grpc:
        endpoint: 0.0.0.0:4317

processors:
  batch:

exporters:
  prometheus:
    endpoint: 0.0.0.0:9464

service:
  pipelines:
    metrics:
      receivers: [otlp]
      processors: [batch]
      exporters: [prometheus]
    logs:
      receivers: [otlp]
      processors: [batch]
      exporters: []   # point at your log backend (loki, elasticsearch, …)
```

示例查询（PromQL，配合上方 Prometheus exporter）：

```promql
# Tokens by model and type across the org, 1h rate
sum by (model, type) (rate(grok_code_token_usage_total[1h]))

# Sessions per team per day
sum by (team_id) (increase(grok_code_session_count_total[1d]))

# Tool-permission denial ratio
sum(rate(grok_code_tool_decision_total{decision="deny"}[1h]))
  / sum(rate(grok_code_tool_decision_total[1h]))
```

## 调试

设置 `OTEL_LOGS_EXPORTER=console` / `OTEL_METRICS_EXPORTER=console` 可将
已脱敏记录打印到 **stderr**（在 `agent`/`headless` 入口中会抑制，
以保持捕获日志干净）。导出错误不会在 TUI 中显示；请查看
调试日志。

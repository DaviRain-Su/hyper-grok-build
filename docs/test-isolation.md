# Test isolation rules in `xai-grok-shell`

| 项 | 内容 |
|----|------|
| 状态 | 活文档，随测试演进 |
| 受众 | 修改 `xai-grok-shell` 测试的提交者 |
| 起因 | 2026-07-22 修复 13 个 env-dependent 失败时沉淀 |

## 1. 为什么需要这份文档

`xai-grok-shell` 的 lib test 在一台**设置了 BYOK 平台 API key 的开发者机器**上
（典型：`ANTHROPIC_API_KEY`、`OLLAMA_API_KEY` 等）会失败 13+ 个用例，但在干净的
CI 上全绿。根因不是某个测试写错，而是**进程全局可变状态**从开发者 shell 泄漏进
测试。本文记录泄漏点与隔离规约，避免下次 monorepo sync 之后再撞同一类问题。

## 2. 进程全局可变状态清单

下面这些状态会被测试读取，且无法用 `tempfile::TempDir` 隔离——必须在测试里
显式 `EnvGuard::unset` / `EnvGuard::set` 并配 `#[serial_test::serial]`。

### 2.1 BYOK 平台 API-key 环境变量

`resolve_model_list` → `inject_moonshot_builtin_models` 会把
`xai-grok-models::platform_builtin_models()` 注入 catalog。每个平台条目都带
`env_key`（见 `PlatformId::api_key_env_names()`），指向该平台的 API-key 环境变量名
（`ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`OLLAMA_API_KEY`、`MOONSHOT_API_KEY`、…）。

`ModelEntry::has_own_credentials()` 会**在调用时**读取 `std::env::var` 解析
`env_key`。因此：

- 开发者 shell 设置了 `ANTHROPIC_API_KEY` ⇒ 所有 `anthropic/*` 条目
  `has_own_credentials() == true` ⇒ `visible_for_auth()` 返回 true ⇒
  `AuthStatus::resolve` 返回 `ModelCredentials("anthropic/claude-fable-5")`，
  抢在用户 `[model.*]` BYOK 条目之前。
- 同理 `OLLAMA_API_KEY`、`OPENAI_API_KEY` 等。

**隔离方式**：调用 `xai_grok_test_support::unset_all_byok_platform_api_key_envs()`
（遍历 `PlatformId::ALL`，对每个平台的 `api_key_env_names()` 逐个 `EnvGuard::unset`，
内部已去重）。返回 `Vec<EnvGuard>`，caller 必须在测试生命周期内持有。

### 2.2 `GROK_AUTH` / `GROK_AUTH_PATH`

`AuthManager::new(grok_home, …)` **先读环境变量**再考虑 `grok_home` 参数：

```text
GROK_AUTH       // 内联 JSON 凭据，最高优先
GROK_AUTH_PATH  // 自定义 auth.json 路径，覆盖 $GROK_HOME/auth.json
```

一个 `#[serial]` 测试若设置了 `GROK_AUTH`（例如 `cli_models::resolve_oauth_session`），
并发的另一个测试构造 `AuthManager::new(tempdir, …)` 就会读到那条内联 JSON，
而不是测试自己的 tempdir。

**隔离方式**：只需注入内存凭据的 crate 单元测试优先使用
`AuthManager::new_test_isolated(tempdir, …)`；它完全不读 `GROK_AUTH` /
`GROK_AUTH_PATH`，也不需要临时 unset 进程环境。确实要覆盖环境变量解析或
`auth.json` 读写的测试，必须加默认组 `#[serial]`，再用 `EnvGuard` 显式设置
scratch 路径（`cli_models` 用的就是这个，避开 `OnceLock`-cached 真实 home，见
§2.3）。不要在非 serial helper 中临时 unset：它会清掉另一个并行测试的 fixture。

### 2.3 `GROK_HOME` 与 `xai_grok_config::grok_home()` 的 `OnceLock`

`xai_grok_config::paths::grok_home()` 用 `OnceLock<PathBuf>` 缓存第一次解析的结果。
一旦某次调用在没有 `GROK_HOME` 的情况下解析到真实 `~/.grok`，整个进程剩余生命周期
都会返回 `~/.grok`，后续 `EnvGuard::set("GROK_HOME", …)` 也改不回来。

`apply_platform_credentials`（在 `resolve_model_list` 内调用）会从
`xai_grok_config::grok_home()` 读 kimi/codex OAuth bearer 并 stamp 到
`kimi-code/*` / `openai-codex/*` 条目的 `api_key`，使它们 `has_own_credentials()`
成 true。开发者 `~/.grok/auth.json` 若有真实 codex token，这些条目就会在测试里
带上凭据。

**隔离方式**：不要依赖 `GROK_HOME`（会被 `OnceLock` 锁死）。改用 `GROK_AUTH_PATH`
指 scratch `auth.json`，`openai_codex_catalog_access_token_cached` 与
`kimi_code_catalog_access_token_cached` 都 honor `GROK_AUTH_PATH`。`load_effective_config()`
仍然会读 `~/.grok/config.toml`——这一点目前**无法**在测试里隔离，是遗留债务（§4）。

### 2.4 `attribution_emit_count` 进程全局计数器

`auth::attribution::EMIT_COUNT` 是 `static AtomicU64`，任何观察它的测试必须
`#[serial_test::serial(attribution_emit_count)]`。详见
`packages/coding-agent/xai-grok-shell/src/auth/attribution.rs:57`。

## 3. 规约

写或改 `xai-grok-shell` 测试时：

1. **凡是测试触达 `AuthStatus::resolve` / `resolve_model_list` /
   `should_advertise_xai_api_key` / `model_auth_facts` / `has_own_credentials`
   这条链路的**，必须：
   - `#[serial_test::serial]`（默认组）；
   - `let _byok = unset_all_byok_platform_api_key_envs();` 持有到测试结束；
   - 若只需内存凭据：用 `AuthManager::new_test_isolated(tempdir, …)`；若必须测试
     环境/磁盘解析，则显式 set `GROK_AUTH_PATH` 到 temp，并保持默认组 `#[serial]`。

2. **`EnvGuard` 的 `set`/`unset` 是 `unsafe` 的进程全局 mutation**，caller 必须是
   `#[serial]`，否则与并发测试互相踩 env。默认组 `#[serial]` 与
   `#[serial(attribution_emit_count)]` 是两个**不同**的组，组内串行、组间并发——
   涉及 env 的测试**不要**放进 `attribution_emit_count` 组，放进默认组。

3. **`with_resolved_model` 在一个 8MB scoped thread 上反序列化整份 effective config**
   （见 `agent/config.rs:5355` 的 `with_resolved_model`）。这意味着测试期间真实
   `~/.grok/config.toml` 会被读进 catalog。若你的测试对 catalog 内容敏感，
   必须先清掉所有 BYOK env var，否则平台 catalog 条目会带凭据污染断言。

4. **JSON 目录 fixture 用 `xai-grok-models::PlatformId::ALL`，不要硬编码**。
   新增平台时 `unset_all_byok_platform_api_key_envs()` 自动覆盖，无需改测试。

## 4. 遗留债务（本次未修）

- **`~/.grok/config.toml` 与 `~/.grok/auth.json` 的 `OnceLock`-cached `grok_home`
  无法在测试里重定向**。涉及 `load_effective_config()` / `apply_platform_credentials`
  / `grok_home()` 的测试在多线程并行下仍可能读到开发者真实凭据。本次只把直接
  踩到 `has_own_credentials` 的 13 个用例修绿；剩余的多线程 flakiness 来自这一层，
  根因是 `OnceLock` 不能在测试间 reset。彻底修需要把 `grok_home()` 改成接受
  `&Config` 或测试 inject 的路径，属于生产代码重构，另开设计。
- **`auth::manager::lock` 子进程测试**（`subprocess_lock_holder` 系列）在
  `--test-threads=1` 下偶发 `spawn lock-holder subprocess` 失败，与本隔离规约无关，
  是子进程 `cargo test` 调度问题。

## 5. 已修用例索引（2026-07-22）

| 测试 | 失败原因 | 修复 |
|------|----------|------|
| `cli_models::tests::resolve_*`（6 个） | `ANTHROPIC_API_KEY` 等使 catalog 条目带凭据 | `isolate_auth_sources()` 增 `unset_all_byok_platform_api_key_envs()` + `GROK_AUTH_PATH` 指向 temp |
| `agent::auth_method::tests::enterprise_byok_config_does_not_require_login` | 同上 + codex bearer stamp | 增 `_byok` + `GROK_AUTH_PATH` |
| `agent::config::tests::resolve_model_list_prefetch_visibility_matches_auth_and_server_list` | 同上 | `#[serial]` + `_byok` + `GROK_AUTH_PATH` + unset `XAI_API_KEY`/`GROK_AUTH` |
| `agent::mvp_agent::tests::cached_token_fallthrough_falls_to_grok_com_without_credentials` | 同上 | 同上 |
| `agent::platform_models_fetch::tests::think_efforts_maps_max_token_to_max_variant` | 断言过期：`"max"` 现解析为 `ReasoningEffort::Max`，而非旧的 `Xhigh` | 改断言为 `Max`，重命名测试 |
| `agent::platform_models_fetch::tests::wire_k3_entry_gets_catalog_key_and_efforts` | 同上 | 同上 |
| `session::acp_session::auth_error_no_retry_tests::pre_flight_hard_expired_refresh_failure_skips_jwt_fallthrough` | BYOK env 使 `model_auth_facts` 判 `Byok`，gate 关闭 | `#[serial]` + `_byok` |
| `session::acp_session::auth_error_no_retry_tests::*` 的 `auth_manager_with_*` callers | helper 临时 unset `GROK_AUTH`/`GROK_AUTH_PATH`，会清掉并行 Codex fixture | helper 改用 `AuthManager::new_test_isolated`，不再 mutation 进程 env |
| `session::storage::jsonl::tests::workflow_restore_rejects_symlinks_and_caps_run_count` | `read_dir` 顺序不确定 + 截断在 sort 前，symlink 抢占合法 run 槽位 | 生产代码：sort 前移到 truncate 前（`jsonl/mod.rs`） |

## 6. 已修用例索引（2026-07-23，`xai-grok-pager`）

同类问题在 pager crate 的 17 个预置失败，按根因分四组修完（全量 lib 测试现 0 失败）：

| 测试组 | 失败原因 | 修复 |
|------|----------|------|
| `views::agents_modal::tests::*`（6 个） | `merge_persona_lists` 直读 `OnceLock`-cached `grok_home()`，开发者真实 `~/.grok/personas` 泄漏进断言 | 生产代码拆出 `merge_persona_lists_in(bundle, cwd, grok_home)` 注入路径；测试改传 temp home（§2.3 债务的测试侧规避） |
| `slash::commands::tests::shell_collision_contract_*` | 新增 `/providers` 命令与 `provider` 别名未登记 `SHELL_RESERVED` | 补登记（真实 test-vs-code drift，测试在尽职） |
| `diagnostics::fix::tests::shell_aliases_*` | 交互 bash 读用户真实 `~/.bashrc`，其中 `export PATH="$HOME/.grok/bin:$PATH"` 使真 grok 二进制抢占测试 shim | 别名改用 shim 绝对路径（绕过 PATH/函数查找） |
| 主题/颜色断言（`scrollback::blocks::user::*` ×9、`views::picker`、`app::modals`、`settings_modal`、`workflow`、`edit_highlight_worker` 等） | 运行环境 `NO_COLOR=1`/`TERM=dumb` ⇒ `color_support::detect()` 的 `OnceLock` 缓存为 `ColorLevel::None`，颜色全部退化为 `Reset`；且读-改-读缓存对与并行写者撕裂 | `xai-grok-pager-render` 新增 `color_support::force_level_for_test`（逐调用覆盖，绕过 OnceLock）；`test_util::pin_theme()` RAII guard 统一持 `test_lock` + 钉 GrokNight + 强制 TrueColor；全模块测试统一点位（`scrollback/blocks/user.rs` 41 个测试全钉），两个无锁 `cache::set` 写者（`text_selection.rs`、`bg_task.rs`）一并改持锁 |

规约补充：**凡断言颜色/样式/主题缓存的测试，必须 `let _theme = crate::test_util::pin_theme();` 持锁**——既防环境污染，也防并行写者撕裂读-改-读对。残留的 `mermaid_worker` / `app_view` 动画时序 flake 属负载敏感的既有问题，与本组无关。

## 7. 主题缓存 lost-update 竞态的根修（2026-07-23,`xai-grok-pager-render`)

合并 upstream 后主题测试以 ~1 次/轮的频率随机失败（渲染读到 GrokNight、断言读到 TokyoNight——后者来自开发者真实 `~/.grok/config.toml`)。写入端全部持锁仍无法解释,最终由写日志（`THEME_DEBUG_WRITES`）证明**无外部写者**,oracle 分析定位到机制:

`theme::cache::current_kind()` 的慢路径是 check-then-disk-read-then-store 的无锁序列:某个并行测试在 `LOADED=false` 时进入 `load_from_disk()`（慢磁盘读）,期间失败测试的 pin 完成 `set(GrokNight)` 并渲染;在途的磁盘读返回后执行 `CURRENT.store(TokyoNight)` 覆盖显式写入——`TEST_LOCK` 管不到不持锁的普通读者。

**修复（生产级,非测试补丁)**:`theme/cache.rs` 新增 `STATE_LOCK`——`current_kind()` 慢路径持锁复查 `LOADED` 后再发布,`set()` / `reset_for_test()` 也持同一把锁（不复用 `TEST_LOCK`,否则与持锁读者死锁）。同一 lost-update 在生产上显式换肤与首次初始化重叠时也存在,故修在渲染 crate 而非测试侧。回归测试经 `set_disk_kind_override_for_test` / `set_load_pause_for_test` 两个测试钩子确定性覆盖两种交错序。验证:scrollback::blocks ×16 线程 ×8 轮 0 失败,全量套件 ×5 轮仅余 `mermaid_worker` 时序 flake。

## 8. 已修用例索引（2026-08-03 / follow-up，测试套件稳定性）

本轮（含 Oracle follow-up）聚焦：**auth env 竞争**、**模型缓存真实 HOME 污染**、**SessionActor 巨大 future 栈溢出**。

### 8.1 Auth 测试构造器（不碰 effective config / grok_home）

| API | 说明 |
|-----|------|
| `AuthManager::for_test_with_auth_path(path, cfg, proxy)` | **公开** `#[doc(hidden)]`：集成测试可用；绝不读 env / effective config |
| `AuthManager::for_test_home(home, cfg)` | `home/auth.json` + stub proxy |
| `AuthManager::new_test_isolated` | lib unit 别名（`cfg(test)`） |
| `AuthManager::new` | 生产 / **env precedence 测试**（`#[serial]` + `EnvGuard`） |

不读：`GROK_AUTH` / `GROK_AUTH_PATH` / `HOME` / `GROK_HOME`。空串 override = unset。
回归：`new_test_isolated_ignores_*`、`for_test_home_ignores_poisoned_env`（integration）、`production_new_*`。

### 8.2 动态模型 / radius 缓存：显式路径注入

| 项 | 约定 |
|----|------|
| Models | `ModelsCacheManager::with_path` / `prefetch_models_blocking_with_cache`；单测禁止裸 `::new()` |
| Radius | `load/store/lock_radius_cache(path, …)`；生产 fetch 内 `let cache_path = radius_cache_path()` |
| 覆盖 | 非空 `GROK_MODELS_CACHE_PATH` / `GROK_RADIUS_MODELS_CACHE_PATH`；**空串 = unset** |
| 单测 | 每测独立 `TempDir`（直接 Path 或 EnvGuard） |

**遗留**：`grok_home()` OnceLock 仍可能钉死真实 home；生产 `ModelsCacheManager::new()` / 未覆盖 env 的默认路径仍可写真实 home。目标测试路径已显式隔离，**不保证全 crate hermetic**。

### 8.3 SessionActor / `auth_retry_budget` 栈溢出（shell 无 stacker）

| 项 | 内容 |
|----|------|
| 根因 | 巨大 async state machine；`Box::pin(fut)` 在调用方栈构造 |
| 禁止 | **xai-grok-shell** 生产依赖 `stacker`（或通过 stacker 拉入 `psm`）；永久 `RUST_MIN_STACK`；每 poll grow。不声称整个 workspace 无 `psm`（其他 crate 可能仍有传递依赖） |
| 修法 | 堆化大 locals；`#[inline(never)]` body + `box_turn_future` 薄帧 pin |
| 回归 | libtest 默认栈下用例通过；**另** `auth_retry_budget_*_on_explicit_2mib_stack` 用 **命名 2 MiB 线程** + current-thread runtime + LocalSet（不依赖 libtest 默认栈；跨平台/OS 栈语义可能不同，以 Linux Debug 为准）。Release 可在 CI workflow 另行启用 |

### 8.4 触点摘要

- `auth/manager.rs` + `manager_tests.rs`
- `agent/models/cache.rs`、`platform_models_fetch.rs` + 相关 tests
- `session/acp_session_impl/turn.rs`、`tasks_cancel.rs`、`auth_retry_budget_tests.rs`
- `docs/test-isolation.md`（本文）

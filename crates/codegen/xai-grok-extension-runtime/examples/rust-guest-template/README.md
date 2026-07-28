# Rust guest template（官方推荐作者路径）

Hyper WASM 扩展的 **首选编写语言是 Rust**。其它语言（Go / AS / TS→component）后置。

## 构建

```bash
rustup target add wasm32-unknown-unknown
cd examples/rust-guest-template   # 相对 xai-grok-extension-runtime
cargo build --release --target wasm32-unknown-unknown

cp target/wasm32-unknown-unknown/release/hyper_ext_rust_guest_template.wasm \
   ./extension.wasm
```

本 crate **不在** workspace members 里（避免默认 `cargo test` 拉 wasm 目标）；单独构建即可。

## 校验与安装

```bash
# 仓库根
grok plugin validate crates/codegen/xai-grok-extension-runtime/examples/rust-guest-template

mkdir -p ~/.grok/plugins/rust-guest-template
cp plugin.json extension.wasm ~/.grok/plugins/rust-guest-template/
```

```toml
# ~/.grok/config.toml
[plugins]
enabled = ["rust-guest-template"]
```

## 能力

模板默认：

- `pre_tool_gate`：输入含 `rm -rf` 则 deny  
- `before_agent_inject`：注入一条静态 system-reminder 策略文案  

按需改 `src/lib.rs` 与 `plugin.json` 的 `capabilities`。

## 与 WAT 示例的关系

`../safe-shell-plugin/` 用 WAT 方便单测与 ABI 演示，**不是**推荐的日常作者路径。  
新扩展请从本 Rust 模板复制。

## 文档

- [WASM Extensions user guide](../../../xai-grok-pager/docs/user-guide/31-wasm-extensions.md)
- [Design](../../../../../docs/design-wasm-extensions.md)
- [Review notes](../../../../../docs/extension-review-2026-07-28.md)

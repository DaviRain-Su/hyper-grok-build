# Rust guest template（官方 · 基于 SDK）

第三方扩展的默认路径：依赖 `xai-grok-extension-sdk`，用过程宏生成 ABI，不要手写 `hyper_host` 的 `ptr`/`len` 或 `hyper_ext_*` 导出。

## 构建

```bash
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/hyper_ext_rust_guest_template.wasm \
   ./extension.wasm
```

或从仓库任意处创建并构建独立插件：

```bash
grok plugin init ./my-ext --name my-ext
cd my-ext
grok plugin build . --validate
```

这里只重新构建插件自己的 `extension.wasm`，**不需要重新构建 Hyper 二进制**。安装或替换 WASM 后，在当前会话执行 `/plugins reload` 即可重载。

## 写起来长什么样

```rust
use xai_grok_extension_sdk::prelude::*;

#[hyper_plugin]
mod plugin {
    use super::*;

    #[hyper_hook(pre_tool_use)]
    fn guard_destructive_commands() -> i32 {
        if input_contains("rm -rf") {
            deny("blocked")
        } else {
            allow()
        }
    }

    #[hyper_tool(description = "Echo args JSON")]
    fn echo(args: &str) -> i32 {
        tool_result(args);
        allow()
    }
}
```

Hook 和 tool 实现都是普通命名函数，rust-analyzer 可以直接跳转、重命名和定位错误。`#[hyper_plugin]` 只负责生成当前 bootstrap ABI 的胶水。

## 校验

```bash
grok plugin validate . --load
```

## 文档

- [WASM Extensions](../../../xai-grok-pager/docs/user-guide/31-wasm-extensions.md)
- [Next roadmap](../../../../../docs/extension-next-roadmap.md)
- [ABI strategy](../../../../../docs/design-wasm-abi-strategy.md)

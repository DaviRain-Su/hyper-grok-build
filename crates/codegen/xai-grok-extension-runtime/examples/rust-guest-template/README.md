# Rust guest template（官方 · 基于 SDK）

第三方写扩展的**默认路径**：依赖 `xai-grok-extension-sdk`，不要手写 `hyper_host` ptr/len。

## 构建

```bash
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/hyper_ext_rust_guest_template.wasm \
   ./extension.wasm
```

或从仓库任意处：

```bash
grok plugin init ./my-ext --name my-ext
```

## 写起来长什么样

```rust
use xai_grok_extension_sdk::prelude::*;
xai_grok_extension_sdk::extension_boilerplate!();

#[no_mangle]
pub extern "C" fn hyper_ext_on_pre_tool_use() -> i32 {
    if input_contains("rm -rf") {
        deny("blocked")
    } else {
        allow()
    }
}
```

## 校验

```bash
grok plugin validate . --load
```

## 文档

- [WASM Extensions](../../../xai-grok-pager/docs/user-guide/31-wasm-extensions.md)
- [Next roadmap](../../../../../docs/extension-next-roadmap.md)
- [ABI strategy](../../../../../docs/design-wasm-abi-strategy.md)

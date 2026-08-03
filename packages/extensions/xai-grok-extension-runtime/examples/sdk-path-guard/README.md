# sdk-path-guard

SDK example: `pre_tool_gate` denies destructive shell substrings in tool input.

```bash
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/sdk_path_guard.wasm extension.wasm
# or: *.wasm name may be sdk_path_guard.wasm depending on cargo
ls target/wasm32-unknown-unknown/release/*.wasm
```

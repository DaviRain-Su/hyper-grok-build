# sdk-stop-once

SDK example: `stop_gate` blocks the **first** stop so the agent continues once
with feedback; later stops are allowed (host still has a global continuation cap).

```bash
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/sdk_stop_once.wasm extension.wasm
```

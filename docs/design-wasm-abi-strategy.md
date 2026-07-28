# WASM Extension ABI 策略：为什么提过「换 API」，现在怎么选

| 项 | 内容 |
|----|------|
| 日期 | 2026-07-28 |
| 结论 | **不急着换**。当前 **core-wasm bootstrap** 是正式作者 API；Component Model/WIT 是**可选升级**，不是推翻重来。 |

---

## 1. 现在的 API 是什么？

作者写 **Rust → `wasm32-unknown-unknown`**，导出：

```text
hyper_ext_abi_version / on_session_start / on_pre_tool_use / …
hyper_host.*  imports（读 input、写 reason/inject/tool result）
```

这套叫 **bootstrap ABI**：手写 `extern "C"`，简单、已上线、有测试。

`plugin.json` 里的 `"wit": "hyper:extension@0.1.0"` 是**目标契约名**，目前主要用于版本标签与 validate，**并不是**已经用 wit-bindgen 生成绑定。

---

## 2. 为什么文档里提过「换 API」（Component Model + WIT）？

不是因为 bootstrap「错了」，而是工程上有更长的目标：

| 动机 | 含义 |
|------|------|
| **类型化接口** | WIT 描述函数/结构体，少靠 `ptr+len` 字符串 |
| **版本与兼容** | `package@0.1` → `0.2` 有标准升级路径 |
| **多语言** | 将来 Go/JS 走同一 component 工具链 |
| **行业默认** | Wasm 生态（Bytecode Alliance）主推 Component Model |

对 **Rust-first、功能已够** 的阶段，这些是「更好的封装」，**不是**「不做就无法用扩展」。

---

## 3. 现在为什么**不要**急着换？

1. **功能面已齐**：gate / inject / stop / register_tool 都在 bootstrap 上跑通  
2. **换 ABI 成本高**：双轨加载、模板重写、迁移文档、风险回归  
3. **作者策略是 Rust**：WIT 的多语言红利暂时用不上  
4. **稳定比漂亮更重要**：先让插件生态长出来，再冻最终 ABI  

**说法（给贡献者）：**

> Hyper 扩展 API = 今天的 bootstrap 导出表。  
> Component Model 是路线图上的**增强**，有明确触发条件再迁；在此之前写的 `extension.wasm` 会继续被支持至少一个主版本。

---

## 4. 什么时候才值得换？

同时满足再开：

- 外部/团队已有 **多个** 真实 Rust 插件  
- 出现 bootstrap 撑不住的需求（复杂类型、多 guest 语言、严格版本协商）  
- 有带宽做 **双轨**：新 component + 旧 bootstrap 并存一段时间  

---

## 5. 和「第三梯队新功能」的关系

| 能力 | 是否依赖换 API |
|------|----------------|
| register_tool | **否**（已在 bootstrap） |
| before_model inject | **否** |
| fail-closed / validate --load | **否** |
| Component Model | **就是**换 API 那件事 |
| 多语言一等公民 | 换 API 后更顺，但非前提 |

所以：**第三梯队不全是「换 API」**；只有 Component Model / 多语言深绑才是。  
产品新能力优先继续在 **bootstrap 上加**，换 API 单独排期。

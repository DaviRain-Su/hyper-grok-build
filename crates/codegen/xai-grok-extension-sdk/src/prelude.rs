//! Common imports for guest authors.

pub use crate::{
    allow, append_system, deny, describe_tool, inject_context, input_contains, log, log_debug,
    log_error, log_info, log_warn, prompt, stop_hook_active, tool_index, tool_input_json, tool_name,
    tool_result, CORE_ABI_VERSION, EMPTY_OBJECT_SCHEMA, LOG_DEBUG, LOG_ERROR, LOG_INFO, LOG_WARN,
};

// Macros: use crate paths —
//   xai_grok_extension_sdk::hyper_extension! { … }
//   xai_grok_extension_sdk::export_pre_tool_use!(|| { … });
//   xai_grok_extension_sdk::extension_tools! { … }

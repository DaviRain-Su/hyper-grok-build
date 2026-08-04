//! Common imports for guest authors.

pub use crate::{
    CORE_ABI_VERSION, EMPTY_OBJECT_SCHEMA, LOG_DEBUG, LOG_ERROR, LOG_INFO, LOG_WARN, allow,
    append_system, deny, describe_tool, hyper_command, hyper_hook, hyper_plugin, hyper_tool,
    inject_context, input_contains, log, log_debug, log_error, log_info, log_warn, plugin_data_dir,
    prompt, stop_hook_active, tool_index, tool_input_json, tool_name, tool_result,
    tool_result_preview, tool_success,
};

// Recommended attributes are imported by this prelude:
//   #[hyper_plugin], #[hyper_hook(pre_tool_use)], #[hyper_tool(...)]
// Legacy `macro_rules!` exports remain available through explicit crate paths:
//   xai_grok_extension_sdk::hyper_extension! { … }

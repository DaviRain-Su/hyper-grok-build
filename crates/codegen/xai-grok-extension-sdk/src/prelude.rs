//! Common imports for guest authors.

pub use crate::{
    allow, append_system, deny, describe_tool, inject_context, input_contains, prompt, tool_index,
    tool_input_json, tool_name, tool_result, CORE_ABI_VERSION, EMPTY_OBJECT_SCHEMA,
};

// Re-export the macro at prelude path for `use prelude::*; extension_boilerplate!()`
// Note: macros are used as `xai_grok_extension_sdk::extension_boilerplate!` or
// after `#[macro_use] extern crate` — in 2018+ edition use the crate path.

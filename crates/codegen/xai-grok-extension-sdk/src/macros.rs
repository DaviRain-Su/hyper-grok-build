//! Declarative macros for Hyper WASM guests.
//!
//! **Policy:** author DX is `macro_rules!` only — no proc-macro crate.
//! Handlers are plain closures/`fn` so rust-analyzer still works.

/// Minimal required exports: `abi_version`, `session_start`, `session_end`.
///
/// ```ignore
/// xai_grok_extension_sdk::extension_boilerplate!();
///
/// // Custom lifecycle:
/// xai_grok_extension_sdk::extension_boilerplate! {
///     session_start: || { /* warm */ 0 },
///     session_end: on_end,
/// }
/// ```
#[macro_export]
macro_rules! extension_boilerplate {
    () => {
        $crate::extension_boilerplate!(session_start: || 0i32, session_end: || 0i32);
    };
    (session_start: $start:expr) => {
        $crate::extension_boilerplate!(session_start: $start, session_end: || 0i32);
    };
    (session_end: $end:expr) => {
        $crate::extension_boilerplate!(session_start: || 0i32, session_end: $end);
    };
    (session_start: $start:expr, session_end: $end:expr $(,)?) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_abi_version() -> i32 {
            $crate::CORE_ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_session_start() -> i32 {
            ($start)()
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_session_end() -> i32 {
            ($end)()
        }
    };
}

/// Export `hyper_ext_on_pre_tool_use` (capability `pre_tool_gate`).
///
/// ```ignore
/// export_pre_tool_use!(|| {
///     if input_contains("rm -rf") { deny("no") } else { allow() }
/// });
/// ```
#[macro_export]
macro_rules! export_pre_tool_use {
    ($body:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_pre_tool_use() -> i32 {
            ($body)()
        }
    };
}

/// Export `hyper_ext_on_before_agent_start` (capability `before_agent_inject`).
#[macro_export]
macro_rules! export_before_agent_start {
    ($body:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_before_agent_start() -> i32 {
            ($body)()
        }
    };
}

/// Export `hyper_ext_on_before_model` (capability `before_model_inject`).
#[macro_export]
macro_rules! export_before_model {
    ($body:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_before_model() -> i32 {
            ($body)()
        }
    };
}

/// Export `hyper_ext_on_stop` (capability `stop_gate`).
#[macro_export]
macro_rules! export_stop {
    ($body:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_stop() -> i32 {
            ($body)()
        }
    };
}

/// Export `hyper_ext_on_pre_compact` (observe-only).
#[macro_export]
macro_rules! export_pre_compact {
    ($body:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_pre_compact() -> i32 {
            ($body)()
        }
    };
}

/// Register guest tools via `tool_count` / `describe_tool` / `invoke_tool`.
///
/// Tool short-names are Rust identifiers (`echo` → name `"echo"`).
/// `schema` is optional (defaults to [`EMPTY_OBJECT_SCHEMA`](crate::EMPTY_OBJECT_SCHEMA)).
/// `invoke` is `fn(&str) -> i32` (args JSON → return code; use [`tool_result`](crate::tool_result)).
///
/// ```ignore
/// extension_tools! {
///     echo {
///         description: "Echo args JSON",
///         schema: r#"{"type":"object","properties":{}}"#,
///         invoke: |args| {
///             tool_result(args);
///             allow()
///         }
///     }
/// }
/// ```
#[macro_export]
macro_rules! extension_tools {
    ($(
        $tool_name:ident {
            description: $description:expr
            $(, schema: $schema:expr)?
            , invoke: $invoke:expr $(,)?
        }
    ),+ $(,)?) => {
        const __HYPER_EXT_TOOL_META: &[(&str, &str, &str)] = &[
            $(
                (
                    stringify!($tool_name),
                    $description,
                    $crate::extension_tools!(@schema $($schema)?),
                ),
            )+
        ];

        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_tool_count() -> i32 {
            __HYPER_EXT_TOOL_META.len() as i32
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_describe_tool() -> i32 {
            let i = $crate::tool_index();
            if i < 0 {
                return 1;
            }
            let i = i as usize;
            if i >= __HYPER_EXT_TOOL_META.len() {
                return 1;
            }
            let (name, desc, schema) = __HYPER_EXT_TOOL_META[i];
            $crate::describe_tool(name, desc, schema);
            0
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_invoke_tool() -> i32 {
            let name = $crate::tool_name();
            let args = $crate::tool_input_json();
            let _ = &args;
            $(
                if name == stringify!($tool_name) {
                    return ($invoke)(args.as_str());
                }
            )+
            let _ = name;
            $crate::deny("unknown wasm tool")
        }
    };

    (@schema) => {
        $crate::EMPTY_OBJECT_SCHEMA
    };
    (@schema $schema:expr) => {
        $schema
    };
}

/// One-shot guest skeleton: abi + session + optional handlers + optional tools.
///
/// Prefer this over hand-written `#[no_mangle]` exports. Uses **only**
/// declarative macros (no proc-macro).
///
/// ```ignore
/// hyper_extension! {
///     pre_tool_use: || {
///         if input_contains("rm -rf") { deny("no") } else { allow() }
///     },
///     before_agent_start: || {
///         inject_context("hi");
///         allow()
///     },
///     tools: {
///         echo {
///             description: "echo",
///             invoke: |args| { tool_result(args); allow() }
///         }
///     }
/// }
/// ```
#[macro_export]
macro_rules! hyper_extension {
    // Internal arms first so they are not swallowed by the catch-all entry.
    // --- parse named fields (order-independent) ---
    (@parse
        session_start: $ss:tt
        session_end: $se:tt
        pre_tool_use: $pt:tt
        before_agent_start: $ba:tt
        before_model: $bm:tt
        stop: $st:tt
        pre_compact: $pc:tt
        tools: $tools:tt
        ; session_start: $v:expr $(, $($rest:tt)*)?
    ) => {
        $crate::hyper_extension!(@parse
            session_start: ($v)
            session_end: $se
            pre_tool_use: $pt
            before_agent_start: $ba
            before_model: $bm
            stop: $st
            pre_compact: $pc
            tools: $tools
            ; $($($rest)*)?
        );
    };
    (@parse
        session_start: $ss:tt
        session_end: $se:tt
        pre_tool_use: $pt:tt
        before_agent_start: $ba:tt
        before_model: $bm:tt
        stop: $st:tt
        pre_compact: $pc:tt
        tools: $tools:tt
        ; session_end: $v:expr $(, $($rest:tt)*)?
    ) => {
        $crate::hyper_extension!(@parse
            session_start: $ss
            session_end: ($v)
            pre_tool_use: $pt
            before_agent_start: $ba
            before_model: $bm
            stop: $st
            pre_compact: $pc
            tools: $tools
            ; $($($rest)*)?
        );
    };
    (@parse
        session_start: $ss:tt
        session_end: $se:tt
        pre_tool_use: $pt:tt
        before_agent_start: $ba:tt
        before_model: $bm:tt
        stop: $st:tt
        pre_compact: $pc:tt
        tools: $tools:tt
        ; pre_tool_use: $v:expr $(, $($rest:tt)*)?
    ) => {
        $crate::hyper_extension!(@parse
            session_start: $ss
            session_end: $se
            pre_tool_use: ($v)
            before_agent_start: $ba
            before_model: $bm
            stop: $st
            pre_compact: $pc
            tools: $tools
            ; $($($rest)*)?
        );
    };
    (@parse
        session_start: $ss:tt
        session_end: $se:tt
        pre_tool_use: $pt:tt
        before_agent_start: $ba:tt
        before_model: $bm:tt
        stop: $st:tt
        pre_compact: $pc:tt
        tools: $tools:tt
        ; before_agent_start: $v:expr $(, $($rest:tt)*)?
    ) => {
        $crate::hyper_extension!(@parse
            session_start: $ss
            session_end: $se
            pre_tool_use: $pt
            before_agent_start: ($v)
            before_model: $bm
            stop: $st
            pre_compact: $pc
            tools: $tools
            ; $($($rest)*)?
        );
    };
    (@parse
        session_start: $ss:tt
        session_end: $se:tt
        pre_tool_use: $pt:tt
        before_agent_start: $ba:tt
        before_model: $bm:tt
        stop: $st:tt
        pre_compact: $pc:tt
        tools: $tools:tt
        ; before_model: $v:expr $(, $($rest:tt)*)?
    ) => {
        $crate::hyper_extension!(@parse
            session_start: $ss
            session_end: $se
            pre_tool_use: $pt
            before_agent_start: $ba
            before_model: ($v)
            stop: $st
            pre_compact: $pc
            tools: $tools
            ; $($($rest)*)?
        );
    };
    (@parse
        session_start: $ss:tt
        session_end: $se:tt
        pre_tool_use: $pt:tt
        before_agent_start: $ba:tt
        before_model: $bm:tt
        stop: $st:tt
        pre_compact: $pc:tt
        tools: $tools:tt
        ; stop: $v:expr $(, $($rest:tt)*)?
    ) => {
        $crate::hyper_extension!(@parse
            session_start: $ss
            session_end: $se
            pre_tool_use: $pt
            before_agent_start: $ba
            before_model: $bm
            stop: ($v)
            pre_compact: $pc
            tools: $tools
            ; $($($rest)*)?
        );
    };
    (@parse
        session_start: $ss:tt
        session_end: $se:tt
        pre_tool_use: $pt:tt
        before_agent_start: $ba:tt
        before_model: $bm:tt
        stop: $st:tt
        pre_compact: $pc:tt
        tools: $tools:tt
        ; pre_compact: $v:expr $(, $($rest:tt)*)?
    ) => {
        $crate::hyper_extension!(@parse
            session_start: $ss
            session_end: $se
            pre_tool_use: $pt
            before_agent_start: $ba
            before_model: $bm
            stop: $st
            pre_compact: ($v)
            tools: $tools
            ; $($($rest)*)?
        );
    };
    (@parse
        session_start: $ss:tt
        session_end: $se:tt
        pre_tool_use: $pt:tt
        before_agent_start: $ba:tt
        before_model: $bm:tt
        stop: $st:tt
        pre_compact: $pc:tt
        tools: $tools:tt
        ; tools: { $($tool_tt:tt)* } $(, $($rest:tt)*)?
    ) => {
        $crate::hyper_extension!(@parse
            session_start: $ss
            session_end: $se
            pre_tool_use: $pt
            before_agent_start: $ba
            before_model: $bm
            stop: $st
            pre_compact: $pc
            tools: ({ $($tool_tt)* })
            ; $($($rest)*)?
        );
    };

    // --- done: expand ---
    (@parse
        session_start: ($ss:expr)
        session_end: ($se:expr)
        pre_tool_use: $pt:tt
        before_agent_start: $ba:tt
        before_model: $bm:tt
        stop: $st:tt
        pre_compact: $pc:tt
        tools: $tools:tt
        ;
    ) => {
        $crate::extension_boilerplate!(session_start: $ss, session_end: $se);
        $crate::hyper_extension!(@maybe_export pre_tool_use $pt);
        $crate::hyper_extension!(@maybe_export before_agent_start $ba);
        $crate::hyper_extension!(@maybe_export before_model $bm);
        $crate::hyper_extension!(@maybe_export stop $st);
        $crate::hyper_extension!(@maybe_export pre_compact $pc);
        $crate::hyper_extension!(@maybe_tools $tools);
    };

    (@maybe_export pre_tool_use ()) => {};
    (@maybe_export pre_tool_use ($body:expr)) => {
        $crate::export_pre_tool_use!($body);
    };
    (@maybe_export before_agent_start ()) => {};
    (@maybe_export before_agent_start ($body:expr)) => {
        $crate::export_before_agent_start!($body);
    };
    (@maybe_export before_model ()) => {};
    (@maybe_export before_model ($body:expr)) => {
        $crate::export_before_model!($body);
    };
    (@maybe_export stop ()) => {};
    (@maybe_export stop ($body:expr)) => {
        $crate::export_stop!($body);
    };
    (@maybe_export pre_compact ()) => {};
    (@maybe_export pre_compact ($body:expr)) => {
        $crate::export_pre_compact!($body);
    };

    (@maybe_tools ()) => {};
    (@maybe_tools ({ $($tool_tt:tt)* })) => {
        $crate::extension_tools! { $($tool_tt)* }
    };

    // --- public entry (must be last: matches any token tree) ---
    ( $($rest:tt)* ) => {
        $crate::hyper_extension!(@parse
            session_start: (|| 0i32)
            session_end: (|| 0i32)
            pre_tool_use: ()
            before_agent_start: ()
            before_model: ()
            stop: ()
            pre_compact: ()
            tools: ()
            ; $($rest)*
        );
    };
}

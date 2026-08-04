//! Procedural macros for Hyper WASM extension guests.
//!
//! The public macros are re-exported by `xai-grok-extension-sdk`; guest
//! authors should depend on that SDK rather than this crate directly.

use std::collections::HashMap;

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::spanned::Spanned;
use syn::{
    Attribute, Error, FnArg, Ident, Item, ItemFn, ItemMod, LitStr, ReturnType, Signature, Type,
    parse_macro_input,
};

/// Turn an inline module containing annotated ordinary functions into a Hyper
/// WASM guest while keeping those functions visible to rust-analyzer.
///
/// Use `#[hyper_hook(pre_tool_use)]` for lifecycle handlers and
/// `#[hyper_tool(description = "…", schema = "…")]` for guest tools.
#[proc_macro_attribute]
pub fn hyper_plugin(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = TokenStream2::from(attr);
    let module = parse_macro_input!(item as ItemMod);
    match expand_hyper_plugin(attr, module) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Marker consumed by [`hyper_plugin`]. Using it outside a `#[hyper_plugin]`
/// inline module is an error.
#[proc_macro_attribute]
pub fn hyper_hook(_attr: TokenStream, item: TokenStream) -> TokenStream {
    marker_outside_plugin(
        item,
        "`#[hyper_hook(...)]` must be inside an inline module annotated with `#[hyper_plugin]`",
    )
}

/// Marker consumed by [`hyper_plugin`]. Using it outside a `#[hyper_plugin]`
/// inline module is an error.
#[proc_macro_attribute]
pub fn hyper_tool(_attr: TokenStream, item: TokenStream) -> TokenStream {
    marker_outside_plugin(
        item,
        "`#[hyper_tool(...)]` must be inside an inline module annotated with `#[hyper_plugin]`",
    )
}

fn marker_outside_plugin(item: TokenStream, message: &str) -> TokenStream {
    let item = TokenStream2::from(item);
    let error = Error::new(Span::call_site(), message).into_compile_error();
    quote! {
        #item
        #error
    }
    .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum HookKind {
    SessionStart,
    SessionEnd,
    PreToolUse,
    PostToolUse,
    BeforeAgentStart,
    BeforeModel,
    Stop,
    PreCompact,
}

impl HookKind {
    fn parse(ident: &Ident) -> syn::Result<Self> {
        match ident.to_string().as_str() {
            "session_start" => Ok(Self::SessionStart),
            "session_end" => Ok(Self::SessionEnd),
            "pre_tool_use" => Ok(Self::PreToolUse),
            "post_tool_use" => Ok(Self::PostToolUse),
            "before_agent_start" => Ok(Self::BeforeAgentStart),
            "before_model" => Ok(Self::BeforeModel),
            "stop" => Ok(Self::Stop),
            "pre_compact" => Ok(Self::PreCompact),
            _ => Err(Error::new(
                ident.span(),
                "unknown Hyper hook; expected one of: session_start, session_end, pre_tool_use, post_tool_use, before_agent_start, before_model, stop, pre_compact",
            )),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::BeforeAgentStart => "before_agent_start",
            Self::BeforeModel => "before_model",
            Self::Stop => "stop",
            Self::PreCompact => "pre_compact",
        }
    }
}

struct ToolArgs {
    name: Option<LitStr>,
    description: LitStr,
    schema: Option<LitStr>,
}

struct Tool {
    function: Ident,
    name: LitStr,
    description: LitStr,
    schema: Option<LitStr>,
}

fn expand_hyper_plugin(attr: TokenStream2, mut module: ItemMod) -> syn::Result<TokenStream2> {
    if !attr.is_empty() {
        return Err(Error::new_spanned(
            attr,
            "`#[hyper_plugin]` does not accept arguments",
        ));
    }

    let Some((_, items)) = module.content.as_mut() else {
        return Err(Error::new(
            module.ident.span(),
            "`#[hyper_plugin]` requires an inline module (`mod plugin { ... }`)",
        ));
    };

    let mut hooks = HashMap::<HookKind, (Ident, Span)>::new();
    let mut tools = Vec::<Tool>::new();
    let mut tool_names = HashMap::<String, Span>::new();
    let mut errors = None;

    for item in items.iter_mut() {
        let Item::Fn(function) = item else {
            continue;
        };
        process_function(
            function,
            &mut hooks,
            &mut tools,
            &mut tool_names,
            &mut errors,
        );
    }

    if let Some(error) = errors {
        return Err(error);
    }

    let sdk = sdk_path();
    let generated = generate_exports(&sdk, &hooks, &tools);
    let generated: syn::File = syn::parse2(generated)?;
    items.extend(generated.items);

    Ok(quote!(#module))
}

fn process_function(
    function: &mut ItemFn,
    hooks: &mut HashMap<HookKind, (Ident, Span)>,
    tools: &mut Vec<Tool>,
    tool_names: &mut HashMap<String, Span>,
    errors: &mut Option<Error>,
) {
    if function.sig.ident.to_string().starts_with("hyper_ext_") {
        push_error(
            errors,
            Error::new(
                function.sig.ident.span(),
                "`#[hyper_plugin]` owns `hyper_ext_*` ABI exports; use an annotated ordinary function instead",
            ),
        );
    }

    let mut hook_marker = None;
    let mut tool_marker = None;
    let mut retained = Vec::with_capacity(function.attrs.len());

    for attribute in std::mem::take(&mut function.attrs) {
        if is_marker(&attribute, "hyper_hook") {
            match parse_hook_attribute(&attribute) {
                Ok(kind) => {
                    if hook_marker.replace((kind, attribute.span())).is_some() {
                        push_error(
                            errors,
                            Error::new(
                                attribute.span(),
                                "a function may have only one `hyper_hook` marker",
                            ),
                        );
                    }
                }
                Err(error) => push_error(errors, error),
            }
        } else if is_marker(&attribute, "hyper_tool") {
            match parse_tool_attribute(&attribute) {
                Ok(args) => {
                    if tool_marker.replace((args, attribute.span())).is_some() {
                        push_error(
                            errors,
                            Error::new(
                                attribute.span(),
                                "a function may have only one `hyper_tool` marker",
                            ),
                        );
                    }
                }
                Err(error) => push_error(errors, error),
            }
        } else {
            retained.push(attribute);
        }
    }
    function.attrs = retained;

    if hook_marker.is_some() && tool_marker.is_some() {
        push_error(
            errors,
            Error::new(
                function.sig.ident.span(),
                "a function cannot be both a Hyper lifecycle hook and a Hyper tool",
            ),
        );
        return;
    }

    if let Some((kind, marker_span)) = hook_marker {
        if let Err(error) = validate_hook_signature(&function.sig, kind) {
            push_error(errors, error);
        }
        if let Some((_, first_span)) = hooks.get(&kind) {
            let mut error = Error::new(marker_span, format!("duplicate `{}` hook", kind.name()));
            error.combine(Error::new(*first_span, "first hook declared here"));
            push_error(errors, error);
        } else {
            hooks.insert(kind, (function.sig.ident.clone(), marker_span));
        }
    }

    if let Some((args, marker_span)) = tool_marker {
        if let Err(error) = validate_tool_signature(&function.sig) {
            push_error(errors, error);
        }
        let name = args.name.unwrap_or_else(|| {
            LitStr::new(
                function.sig.ident.to_string().as_str(),
                function.sig.ident.span(),
            )
        });
        if let Err(error) = validate_tool_name(&name) {
            push_error(errors, error);
        }
        let name_value = name.value();
        if let Some(first_span) = tool_names.get(&name_value) {
            let mut error = Error::new(
                marker_span,
                format!("duplicate Hyper tool name `{name_value}`"),
            );
            error.combine(Error::new(*first_span, "first tool declared here"));
            push_error(errors, error);
        } else {
            tool_names.insert(name_value, marker_span);
        }
        tools.push(Tool {
            function: function.sig.ident.clone(),
            name,
            description: args.description,
            schema: args.schema,
        });
    }
}

fn parse_hook_attribute(attribute: &Attribute) -> syn::Result<HookKind> {
    let ident = attribute.parse_args::<Ident>()?;
    HookKind::parse(&ident)
}

fn parse_tool_attribute(attribute: &Attribute) -> syn::Result<ToolArgs> {
    let mut name = None;
    let mut description = None;
    let mut schema = None;

    attribute.parse_nested_meta(|meta| {
        if meta.path.is_ident("name") {
            if name.is_some() {
                return Err(meta.error("duplicate `name`"));
            }
            name = Some(meta.value()?.parse::<LitStr>()?);
            Ok(())
        } else if meta.path.is_ident("description") {
            if description.is_some() {
                return Err(meta.error("duplicate `description`"));
            }
            description = Some(meta.value()?.parse::<LitStr>()?);
            Ok(())
        } else if meta.path.is_ident("schema") {
            if schema.is_some() {
                return Err(meta.error("duplicate `schema`"));
            }
            schema = Some(meta.value()?.parse::<LitStr>()?);
            Ok(())
        } else {
            Err(meta
                .error("unknown `hyper_tool` option; expected `name`, `description`, or `schema`"))
        }
    })?;

    let description = description.ok_or_else(|| {
        Error::new(
            attribute.span(),
            "`#[hyper_tool]` requires `description = \"...\"`",
        )
    })?;
    if description.value().is_empty() {
        return Err(Error::new(
            description.span(),
            "Hyper tool description must not be empty",
        ));
    }

    Ok(ToolArgs {
        name,
        description,
        schema,
    })
}

fn validate_hook_signature(signature: &Signature, kind: HookKind) -> syn::Result<()> {
    validate_common_signature(signature)?;
    if !signature.inputs.is_empty() {
        return Err(Error::new(
            signature.inputs.span(),
            format!("`{}` hooks must take no arguments", kind.name()),
        ));
    }
    validate_i32_return(signature, "Hyper hook")
}

fn validate_tool_signature(signature: &Signature) -> syn::Result<()> {
    validate_common_signature(signature)?;
    if signature.inputs.len() != 1 {
        return Err(Error::new(
            signature.inputs.span(),
            "Hyper tools must have the signature `fn(args: &str) -> i32`",
        ));
    }
    let Some(FnArg::Typed(argument)) = signature.inputs.first() else {
        return Err(Error::new(
            signature.inputs.span(),
            "Hyper tools must have the signature `fn(args: &str) -> i32`",
        ));
    };
    if !is_str_reference(&argument.ty) {
        return Err(Error::new(
            argument.ty.span(),
            "Hyper tool argument must be `&str`",
        ));
    }
    validate_i32_return(signature, "Hyper tool")
}

fn validate_common_signature(signature: &Signature) -> syn::Result<()> {
    if let Some(token) = signature.constness {
        return Err(Error::new(token.span(), "Hyper handlers cannot be `const`"));
    }
    if let Some(token) = signature.asyncness {
        return Err(Error::new(token.span(), "Hyper handlers cannot be `async`"));
    }
    if let Some(token) = signature.unsafety {
        return Err(Error::new(
            token.span(),
            "Hyper handlers cannot be `unsafe`",
        ));
    }
    if let Some(abi) = &signature.abi {
        return Err(Error::new(
            abi.span(),
            "write an ordinary Rust function; the macro generates the `extern \"C\"` ABI wrapper",
        ));
    }
    if !signature.generics.params.is_empty() || signature.generics.where_clause.is_some() {
        return Err(Error::new(
            signature.generics.span(),
            "Hyper handlers cannot be generic",
        ));
    }
    if let Some(variadic) = &signature.variadic {
        return Err(Error::new(
            variadic.span(),
            "Hyper handlers cannot be variadic",
        ));
    }
    Ok(())
}

fn validate_i32_return(signature: &Signature, subject: &str) -> syn::Result<()> {
    match &signature.output {
        ReturnType::Type(_, ty) if is_named_type(ty, "i32") => Ok(()),
        _ => Err(Error::new(
            signature.output.span(),
            format!("{subject} must explicitly return `i32`"),
        )),
    }
}

fn is_str_reference(ty: &Type) -> bool {
    match ty {
        Type::Reference(reference) if reference.mutability.is_none() => {
            is_named_type(&reference.elem, "str")
        }
        Type::Group(group) => is_str_reference(&group.elem),
        Type::Paren(paren) => is_str_reference(&paren.elem),
        _ => false,
    }
}

fn is_named_type(ty: &Type, expected: &str) -> bool {
    match ty {
        Type::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
            path.path.segments[0].ident == expected && path.path.segments[0].arguments.is_empty()
        }
        Type::Group(group) => is_named_type(&group.elem, expected),
        Type::Paren(paren) => is_named_type(&paren.elem, expected),
        _ => false,
    }
}

fn validate_tool_name(name: &LitStr) -> syn::Result<()> {
    let value = name.value();
    if value.is_empty() || value.len() > 64 {
        return Err(Error::new(
            name.span(),
            "Hyper tool name must contain 1 to 64 bytes",
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(Error::new(
            name.span(),
            "Hyper tool name may contain only ASCII letters, digits, `_`, `-`, or `.`",
        ));
    }
    Ok(())
}

fn is_marker(attribute: &Attribute, expected: &str) -> bool {
    attribute
        .path()
        .segments
        .last()
        .is_some_and(|segment| segment.ident == expected)
}

fn push_error(errors: &mut Option<Error>, error: Error) {
    if let Some(errors) = errors {
        errors.combine(error);
    } else {
        *errors = Some(error);
    }
}

fn sdk_path() -> TokenStream2 {
    match crate_name("xai-grok-extension-sdk") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(name.replace('-', "_").as_str(), Span::call_site());
            quote!(::#ident)
        }
        Err(_) => quote!(::xai_grok_extension_sdk),
    }
}

fn generate_exports(
    sdk: &TokenStream2,
    hooks: &HashMap<HookKind, (Ident, Span)>,
    tools: &[Tool],
) -> TokenStream2 {
    let session_start = hook_call_or_zero(hooks, HookKind::SessionStart);
    let session_end = hook_call_or_zero(hooks, HookKind::SessionEnd);
    let pre_tool_use = optional_hook_export(
        hooks,
        HookKind::PreToolUse,
        Ident::new("hyper_ext_on_pre_tool_use", Span::call_site()),
    );
    let post_tool_use = optional_hook_export(
        hooks,
        HookKind::PostToolUse,
        Ident::new("hyper_ext_on_post_tool_use", Span::call_site()),
    );
    let before_agent_start = optional_hook_export(
        hooks,
        HookKind::BeforeAgentStart,
        Ident::new("hyper_ext_on_before_agent_start", Span::call_site()),
    );
    let before_model = optional_hook_export(
        hooks,
        HookKind::BeforeModel,
        Ident::new("hyper_ext_on_before_model", Span::call_site()),
    );
    let stop = optional_hook_export(
        hooks,
        HookKind::Stop,
        Ident::new("hyper_ext_on_stop", Span::call_site()),
    );
    let pre_compact = optional_hook_export(
        hooks,
        HookKind::PreCompact,
        Ident::new("hyper_ext_on_pre_compact", Span::call_site()),
    );
    let tool_exports = generate_tool_exports(sdk, tools);

    quote! {
        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_abi_version() -> i32 {
            #sdk::CORE_ABI_VERSION
        }

        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_session_start() -> i32 {
            #session_start
        }

        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_session_end() -> i32 {
            #session_end
        }

        #pre_tool_use
        #post_tool_use
        #before_agent_start
        #before_model
        #stop
        #pre_compact
        #tool_exports
    }
}

fn hook_call_or_zero(hooks: &HashMap<HookKind, (Ident, Span)>, kind: HookKind) -> TokenStream2 {
    hooks
        .get(&kind)
        .map_or_else(|| quote!(0i32), |(function, _)| quote!(#function()))
}

fn optional_hook_export(
    hooks: &HashMap<HookKind, (Ident, Span)>,
    kind: HookKind,
    export: Ident,
) -> TokenStream2 {
    let Some((function, _)) = hooks.get(&kind) else {
        return TokenStream2::new();
    };
    quote! {
        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn #export() -> i32 {
            #function()
        }
    }
}

fn generate_tool_exports(sdk: &TokenStream2, tools: &[Tool]) -> TokenStream2 {
    if tools.is_empty() {
        return TokenStream2::new();
    }

    let metadata = tools.iter().map(|tool| {
        let name = &tool.name;
        let description = &tool.description;
        let schema = tool.schema.as_ref().map_or_else(
            || quote!(#sdk::EMPTY_OBJECT_SCHEMA),
            |schema| quote!(#schema),
        );
        quote!((#name, #description, #schema),)
    });
    let invoke = tools.iter().map(|tool| {
        let name = &tool.name;
        let function = &tool.function;
        quote! {
            if name == #name {
                return #function(args.as_str());
            }
        }
    });

    quote! {
        #[doc(hidden)]
        const __HYPER_EXT_TOOL_META: &[(&str, &str, &str)] = &[
            #(#metadata)*
        ];

        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_tool_count() -> i32 {
            __HYPER_EXT_TOOL_META.len() as i32
        }

        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_describe_tool() -> i32 {
            let index = #sdk::tool_index();
            if index < 0 {
                return 1;
            }
            let index = index as usize;
            if index >= __HYPER_EXT_TOOL_META.len() {
                return 1;
            }
            let (name, description, schema) = __HYPER_EXT_TOOL_META[index];
            #sdk::describe_tool(name, description, schema);
            0
        }

        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_invoke_tool() -> i32 {
            let name = #sdk::tool_name();
            let args = #sdk::tool_input_json();
            #(#invoke)*
            #sdk::deny("unknown wasm tool")
        }
    }
}

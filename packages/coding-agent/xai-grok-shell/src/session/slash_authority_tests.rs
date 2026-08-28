use super::*;
use crate::session::slash_commands::BUILTIN_COMMANDS;

fn text_block(text: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text))
}

#[test]
fn model_authored_resolution_uses_exact_canonical_metadata() {
    assert!(matches!(
        resolve(
            InputAuthority::ModelAuthoredUntrusted,
            &[text_block("/compact preserve auth")],
            BUILTIN_COMMANDS,
        ),
        AuthorityResolution::StaticBuiltin(BuiltinAction::Compact {
            user_context: Some(context),
        }) if context == "preserve auth"
    ));

    for text in ["/Compact", "/COMPACT", "/yolo", "/context", "/feedback"] {
        assert!(matches!(
            resolve(
                InputAuthority::ModelAuthoredUntrusted,
                &[text_block(text)],
                BUILTIN_COMMANDS,
            ),
            AuthorityResolution::ModelAuthoredSkillCandidate { .. }
        ));
    }
}

#[test]
fn runtime_control_is_inert_and_human_intent_continues_to_dynamic_resolution() {
    assert!(matches!(
        resolve(
            InputAuthority::RuntimeControl,
            &[text_block("/compact")],
            BUILTIN_COMMANDS,
        ),
        AuthorityResolution::Inert
    ));
    assert!(matches!(
        resolve(
            InputAuthority::RuntimeControl,
            &[text_block("/available-skill")],
            BUILTIN_COMMANDS,
        ),
        AuthorityResolution::Inert
    ));
    assert!(matches!(
        resolve(
            InputAuthority::HumanIntent,
            &[text_block("/Compact keep aliases")],
            BUILTIN_COMMANDS,
        ),
        AuthorityResolution::HumanIntent {
            command_name: "Compact",
            args: "keep aliases",
        }
    ));
}

use super::*;
use xai_grok_shell::util::config::{ConsentAnswer, ConsentGate};

const ACCOUNT: &str = "user@example.com";
const NOTICE_ID: &str = "tos-2026-08";
const TOS_URL: &str = "https://x.ai/legal/terms-of-service";

fn gate() -> ConsentGate {
    ConsentGate {
        id: NOTICE_ID.to_owned(),
        version: Some(3),
        title: Some("Updated terms".to_owned()),
        body: Some(format!(
            "Review our Terms of Service at {TOS_URL} before continuing."
        )),
        ..Default::default()
    }
}

fn inputs<'a>(
    gate: Option<&'a ConsentGate>,
    answers: &'a BTreeMap<String, ConsentAnswer>,
) -> ConsentInputs<'a> {
    ConsentInputs {
        gate,
        answered_this_run: None,
        answers,
        account: Some(ACCOUNT),
        minimal: false,
    }
}

fn no_answers() -> BTreeMap<String, ConsentAnswer> {
    BTreeMap::new()
}

fn answered(notice_id: &str, version: i32) -> BTreeMap<String, ConsentAnswer> {
    BTreeMap::from([(
        notice_id.to_owned(),
        ConsentAnswer {
            version,
            account: Some(ACCOUNT.to_owned()),
            ..Default::default()
        },
    )])
}

#[test]
fn valid_gate_arms_pending() {
    let gate = gate();

    let state = consent_verdict(&inputs(Some(&gate), &no_answers()));

    let ConsentState::Pending { notice, .. } = state else {
        panic!("expected pending");
    };
    assert_eq!(notice.version, 3);
    assert_eq!(notice.accept_label, "Got it");
    assert!(notice.body.contains("Terms of Service"));
}

#[test]
fn absent_gate_is_done() {
    assert!(matches!(
        consent_verdict(&inputs(None, &no_answers())),
        ConsentState::Done
    ));
}

/// A payload the validator refuses must leave the client usable, not block every session on it.
#[test]
fn a_refused_gate_fails_open() {
    let mut gate = gate();
    gate.body = Some(String::new());

    assert!(matches!(
        consent_verdict(&inputs(Some(&gate), &no_answers())),
        ConsentState::Done
    ));
}

/// The disk write is a spawned task, so the in-run answer is what stops a re-arm before it lands.
#[test]
fn an_answer_from_this_run_suppresses() {
    let gate = gate();
    let answers = no_answers();
    let mut i = inputs(Some(&gate), &answers);
    i.answered_this_run = Some((NOTICE_ID, 3));

    assert!(matches!(consent_verdict(&i), ConsentState::Done));
}

#[test]
fn empty_body_refuses() {
    let mut gate = gate();
    gate.body = Some("   ".to_owned());

    assert_eq!(
        ConsentNotice::try_from_remote(&gate),
        Err(ConsentArmRefusal::EmptyBody)
    );
}

#[test]
fn missing_version_refuses() {
    let mut gate = gate();
    gate.version = None;

    assert_eq!(
        ConsentNotice::try_from_remote(&gate),
        Err(ConsentArmRefusal::MissingVersion)
    );
}

#[test]
fn implausible_version_refuses() {
    let mut gate = gate();
    gate.version = Some(999_999);

    assert_eq!(
        ConsentNotice::try_from_remote(&gate),
        Err(ConsentArmRefusal::ImplausibleVersion(999_999))
    );
}

#[test]
fn escapes_and_reordering_characters_are_stripped_from_the_body() {
    let mut gate = gate();
    gate.body = Some("before\u{1b}[31m\u{202e}\u{200b}after".to_owned());

    let notice = ConsentNotice::try_from_remote(&gate).expect("valid");

    assert_eq!(notice.body, "before[31mafter");
}

/// Not conditional on the server ack, or a missing backend would re-ask someone who answered.
#[test]
fn an_unacked_answer_at_or_above_the_version_suppresses() {
    let gate = gate();
    let answers = answered(NOTICE_ID, 3);
    assert!(!answers[NOTICE_ID].acked);

    let verdict = consent_verdict(&inputs(Some(&gate), &answers));

    assert!(matches!(verdict, ConsentState::Done));
}

#[test]
fn older_answer_does_not_suppress_newer_notice() {
    let gate = gate();

    let verdict = consent_verdict(&inputs(Some(&gate), &answered(NOTICE_ID, 2)));

    assert!(matches!(verdict, ConsentState::Pending { .. }));
}

/// Version counters run per notice id, so a high answer to one must not cover another.
#[test]
fn answer_to_a_different_notice_does_not_suppress() {
    let gate = gate();

    let verdict = consent_verdict(&inputs(Some(&gate), &answered("consumer-tos-2026-08", 9)));

    assert!(matches!(verdict, ConsentState::Pending { .. }));
}

#[test]
fn answer_from_another_user_does_not_suppress() {
    let gate = gate();
    let mut answers = answered(NOTICE_ID, 3);
    answers.get_mut(NOTICE_ID).unwrap().account = Some("someone-else@example.com".to_owned());

    let verdict = consent_verdict(&inputs(Some(&gate), &answers));

    assert!(matches!(verdict, ConsentState::Pending { .. }));
}

#[test]
fn minimal_mode_fails_open() {
    let gate = gate();
    let answers = no_answers();
    let mut i = inputs(Some(&gate), &answers);
    i.minimal = true;

    assert!(matches!(consent_verdict(&i), ConsentState::Done));
}

#[test]
fn a_body_taller_than_a_standard_terminal_refuses() {
    let mut gate = gate();
    gate.body = Some("line\n".repeat(40));

    assert!(matches!(
        ConsentNotice::try_from_remote(&gate),
        Err(ConsentArmRefusal::BodyTooTall(_))
    ));
}

#[test]
fn an_unusable_id_refuses() {
    for id in ["", "has spaces", &"x".repeat(100), "quote\"inject"] {
        let mut gate = gate();
        gate.id = id.to_owned();

        assert_eq!(
            ConsentNotice::try_from_remote(&gate),
            Err(ConsentArmRefusal::UnusableId),
            "{id:?} must not arm the gate",
        );
    }
}

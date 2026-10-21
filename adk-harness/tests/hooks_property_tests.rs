//! Property tests for hooks (spec 3.1 §7.22, §9.2 I53, §11): whatever a hook prints and
//! however it exits, its outcome never loosens a decision, never becomes an `Allow`, and
//! reaches a result only as a delimited note.

#![cfg(feature = "hooks")]

use adk_core::PolicyDecision;
use adk_harness::hooks::{
    HookEvent, HookNote, HookOutcome, HookSource, PostToolVerdict, PreToolVerdict, glob_matches,
};
use proptest::prelude::*;
use serde_json::{Value, json};

/// Deny ⪰ RequireApproval ⪰ Allow.
fn rank(decision: &PolicyDecision) -> u8 {
    match decision {
        PolicyDecision::Allow => 0,
        PolicyDecision::RequireApproval { .. } => 1,
        PolicyDecision::Deny { .. } => 2,
    }
}

fn bases() -> [PolicyDecision; 3] {
    [
        PolicyDecision::Allow,
        PolicyDecision::require_approval("needs a decision"),
        PolicyDecision::deny("denied by the table"),
    ]
}

/// JSON a hook might print: decisions of every spelling, grants, mode and rule fields, and
/// junk.
fn hook_json() -> impl Strategy<Value = Value> {
    let decision = prop_oneof![
        Just("allow"),
        Just("ALLOW"),
        Just("approve"),
        Just("permit"),
        Just("deny"),
        Just("fail"),
        Just("annotate"),
        Just("continue"),
        Just("ask"),
        Just("auto"),
        Just(""),
    ];
    let key = prop_oneof![
        Just("decision"),
        Just("permissionDecision"),
        Just("permission_decision"),
        Just("mode"),
        Just("rule"),
        Just("verdict"),
    ];
    (key, decision, any::<bool>(), "[a-zA-Z ]{0,24}").prop_map(|(key, decision, extra, text)| {
        let mut object = serde_json::Map::new();
        object.insert(key.to_string(), json!(decision));
        if extra {
            object.insert("mode".to_string(), json!("auto"));
            object.insert("rule".to_string(), json!({ "tool": "bash", "decision": "allow" }));
            object.insert("tool_approvals".to_string(), json!(["*"]));
        }
        object.insert("reason".to_string(), json!(text));
        object.insert("text".to_string(), json!(text));
        object.insert("message".to_string(), json!(text));
        Value::Object(object)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Any bytes on stdout: the PreTool verdict never loosens any base decision.
    #[test]
    fn arbitrary_stdout_never_loosens_a_decision(stdout in proptest::collection::vec(any::<u8>(), 0..256)) {
        let parsed = HookOutcome::parse(&stdout);
        let verdict = PreToolVerdict::from_outcome(HookEvent::PreTool.narrow(parsed.outcome));
        for base in bases() {
            let applied = verdict.clone().apply(base.clone());
            prop_assert!(rank(&applied) >= rank(&base), "{stdout:?} loosened {base:?} to {applied:?}");
            if rank(&base) == 2 {
                prop_assert_eq!(rank(&applied), 2, "a denied call stays denied");
            }
        }
    }

    /// Any decision-shaped JSON: a grant is `Continue`, flagged `ignored_grant`; only
    /// `deny` tightens; nothing names a mode or a rule the harness would read.
    #[test]
    fn decision_json_never_grants(value in hook_json()) {
        let parsed = HookOutcome::parse(value.to_string().as_bytes());
        let word = ["decision", "permissionDecision", "permission_decision"]
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .map(str::to_ascii_lowercase);
        let kind = parsed.outcome.as_str();
        match word.as_deref() {
            Some("deny") => prop_assert_eq!(kind, "deny"),
            Some("fail") => prop_assert_eq!(kind, "fail"),
            Some("annotate") => prop_assert!(kind == "annotate" || kind == "continue", "{}", kind),
            Some("allow" | "approve" | "permit") => {
                prop_assert_eq!(&parsed.outcome, &HookOutcome::Continue);
                prop_assert!(parsed.ignored_grant);
            }
            _ => prop_assert_eq!(&parsed.outcome, &HookOutcome::Continue),
        }
        let verdict = PreToolVerdict::from_outcome(HookEvent::PreTool.narrow(parsed.outcome));
        for base in bases() {
            prop_assert!(rank(&verdict.clone().apply(base.clone())) >= rank(&base));
        }
    }

    /// A PostTool outcome never carries a `Deny`, and applying it to a result either keeps
    /// it, replaces it with an error, or adds delimited notes and nothing else.
    #[test]
    fn post_tool_outcomes_only_fail_or_annotate(
        stdout in proptest::collection::vec(any::<u8>(), 0..256),
        result in proptest::collection::btree_map("[a-z]{1,6}", any::<i64>(), 0..4),
    ) {
        let outcome = HookEvent::PostTool.narrow(HookOutcome::parse(&stdout).outcome);
        prop_assert_ne!(outcome.as_str(), "deny");
        let original: Value = json!(result);
        let verdict = match outcome {
            HookOutcome::Continue | HookOutcome::Deny { .. } => PostToolVerdict::Continue,
            HookOutcome::Fail { message } => PostToolVerdict::Fail { message },
            HookOutcome::Annotate { text } => PostToolVerdict::Annotate {
                notes: vec![HookNote::new(HookSource::Workspace, "sha256:0", &text)],
            },
        };
        let applied = verdict.clone().apply(original.clone());
        match verdict {
            PostToolVerdict::Continue => prop_assert_eq!(applied, original),
            PostToolVerdict::Fail { message } => prop_assert_eq!(applied, json!({ "error": message })),
            PostToolVerdict::Annotate { .. } => {
                let mut expected = original.as_object().cloned().unwrap_or_default();
                let notes = applied["hook_notes"].as_array().cloned().unwrap_or_default();
                prop_assert_eq!(notes.len(), 1);
                let note = notes[0].as_str().unwrap_or_default();
                prop_assert!(note.starts_with("<hook-note source=\"workspace\" hook=\"sha256:0\">"));
                prop_assert!(note.ends_with("</hook-note>"));
                prop_assert!(!note[..note.len() - "</hook-note>".len()].contains("</hook-note"));
                expected.insert("hook_notes".to_string(), Value::Array(notes));
                prop_assert_eq!(applied, Value::Object(expected));
            }
        }
    }

    /// Notifier events narrow everything but a note to `Continue`.
    #[test]
    fn notifier_events_only_annotate(stdout in proptest::collection::vec(any::<u8>(), 0..256)) {
        let parsed = HookOutcome::parse(&stdout).outcome;
        for event in [HookEvent::OnHold, HookEvent::OnStop, HookEvent::OnSessionStart] {
            let kind = event.narrow(parsed.clone()).as_str();
            prop_assert!(kind == "continue" || kind == "annotate", "{} gave {}", event, kind);
        }
    }

    /// A literal matcher matches its own tool and `*` matches every tool.
    #[test]
    fn literal_globs_match_themselves(name in "[a-z_:.-]{1,24}") {
        prop_assert!(glob_matches(&name, &name));
        prop_assert!(glob_matches("*", &name));
        let star = format!("{name}*");
        let question = format!("{name}?");
        prop_assert!(glob_matches(&star, &name), "{} vs {}", star, name);
        prop_assert!(!glob_matches(&question, &name), "{} vs {}", question, name);
    }
}

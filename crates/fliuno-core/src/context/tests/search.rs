use super::*;
use crate::context::search::{DEFAULT_SEARCH_TURNS, MAX_SEARCH_TURN_BYTES};

#[test]
fn search_can_page_past_empty_windows_and_reports_skipped_large_turns() {
    let (mut kernel, session) = setup();
    let matching = complete(&mut kernel, &session.id, "old", "FOUND");
    for _ in 0..DEFAULT_SEARCH_TURNS - 1 {
        complete(&mut kernel, &session.id, "unrelated", "other answer");
    }
    complete(
        &mut kernel,
        &session.id,
        "oversized found",
        &"x".repeat(MAX_SEARCH_TURN_BYTES as usize),
    );
    let mut params = SessionHistorySearchParams {
        session_id: session.id,
        query: " found ".into(),
        role: None,
        through_sequence: None,
        before_sequence: None,
        limit: None,
        scan_limit: None,
    };
    let first = kernel.search_session_history(params.clone()).unwrap();
    assert!(first.matches.is_empty());
    assert_eq!(first.scanned_turns, DEFAULT_SEARCH_TURNS);
    assert_eq!(first.skipped_oversized_turns, 1);
    let wide = kernel
        .search_session_history(SessionHistorySearchParams {
            through_sequence: Some(first.through_sequence),
            scan_limit: Some(MAX_HISTORY_SEARCH_SCAN_LIMIT),
            ..params.clone()
        })
        .unwrap();
    assert_eq!(wide.matches[0].task_id, matching.id);
    assert_eq!(wide.scanned_turns, DEFAULT_SEARCH_TURNS + 1);
    assert_eq!(wide.skipped_oversized_turns, 1);
    assert_eq!(wide.next_before_sequence, None);
    let narrow = kernel
        .search_session_history(SessionHistorySearchParams {
            scan_limit: Some(1),
            ..params.clone()
        })
        .unwrap();
    assert!(narrow.matches.is_empty());
    assert_eq!(narrow.scanned_turns, 1);
    assert_eq!(narrow.skipped_oversized_turns, 1);
    assert!(narrow.next_before_sequence.unwrap() > first.next_before_sequence.unwrap());
    let capture = kernel
        .session_context(
            &params.session_id,
            ContextOptions {
                selection: ContextSelection::MatchRecent,
                history_query: Some(ContextHistoryQuery {
                    query: params.query.clone(),
                    role: None,
                }),
                ..ContextOptions::default()
            },
        )
        .unwrap();
    assert!(
        capture.turns.is_empty(),
        "capture must not scan past the first window"
    );
    assert_eq!(capture.omitted_turns, u64::from(DEFAULT_SEARCH_TURNS) + 1);
    params.through_sequence = Some(first.through_sequence);
    params.before_sequence = Some(first.next_before_sequence.unwrap());
    let second = kernel.search_session_history(params).unwrap();
    assert_eq!(second.matches.len(), 1);
    assert_eq!(second.matches[0].task_id, matching.id);
    assert_eq!(second.scanned_turns, 1);
    assert_eq!(second.next_before_sequence, None);
}

#[test]
fn match_recent_rejects_conflicting_modes_and_over_budget_envelopes() {
    let (kernel, session) = setup();
    for opts in [
        json!({"selection":"match_recent"}),
        json!({"historyQuery":{"query":"x"}}),
        json!({"selection":"match_recent","mode":"isolated","historyQuery":{"query":"x"}}),
        json!({"selection":"match_recent","taskIds":["id"],"historyQuery":{"query":"x"}}),
        json!({"selection":"match_recent","historyQuery":{"query":" "}}),
        json!({"selection":"match_recent","historyQuery":{"query":"x".repeat(257)}}),
        json!({"selection":"match_recent","maxBytes":1024,"historyQuery":{"query":"\0".repeat(256)}}),
    ] {
        let options = serde_json::from_value(opts).unwrap();
        assert!(matches!(
            kernel.session_context(&session.id, options),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn borrowed_search_preserves_unicode_escapes_and_message_boundaries() {
    use crate::context::search::HistoryMatcher;

    let stored = json!({
        "messages":[
            {"role":"user","text":"AUDIT \"Report\"\tA\0B"},
            {"role":"assistant","text":"前缀 KELVIN İSTANBUL ΟΣ ΑΣΑ"},
            {"role":"tool_result","callId":"hidden","output":{"text":"only-in-tool"}}
        ],
        "artifacts":[{"id":"artifact","title":"only-in-title","mediaType":"text/plain"}]
    })
    .to_string();
    for (query, role, expected) in [
        (
            "audit \"report\"\ta\0b",
            Some(SessionHistoryRole::User),
            true,
        ),
        ("audit", Some(SessionHistoryRole::Assistant), false),
        ("kelvin", None, true),
        ("i\u{307}stanbul", Some(SessionHistoryRole::Assistant), true),
        ("ος", None, true),
        ("οσ", None, false),
        ("ασα", None, true),
        ("b前缀", None, false),
        ("only-in-tool", None, false),
        ("only-in-title", None, false),
    ] {
        assert_eq!(
            HistoryMatcher::new(query, role).matches(&stored).unwrap(),
            expected,
            "{query:?}"
        );
    }
}

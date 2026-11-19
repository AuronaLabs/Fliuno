use super::*;
use crate::context::search::{MAX_SEARCH_TURN_BYTES, MAX_SEARCH_TURNS};

#[test]
fn search_can_page_past_empty_windows_and_reports_skipped_large_turns() {
    let (mut kernel, session) = setup();
    let matching = complete(&mut kernel, &session.id, "old", "FOUND");
    for _ in 0..MAX_SEARCH_TURNS - 1 {
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
    };
    let first = kernel.search_session_history(params.clone()).unwrap();
    assert!(first.matches.is_empty());
    assert_eq!(first.scanned_turns, MAX_SEARCH_TURNS);
    assert_eq!(first.skipped_oversized_turns, 1);
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
    assert_eq!(capture.omitted_turns, u64::from(MAX_SEARCH_TURNS) + 1);
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

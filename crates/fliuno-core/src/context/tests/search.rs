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
        through_sequence: None,
        before_sequence: None,
        limit: None,
    };
    let first = kernel.search_session_history(params.clone()).unwrap();
    assert!(first.matches.is_empty());
    assert_eq!(first.scanned_turns, MAX_SEARCH_TURNS);
    assert_eq!(first.skipped_oversized_turns, 1);
    params.through_sequence = Some(first.through_sequence);
    params.before_sequence = Some(first.next_before_sequence.unwrap());
    let second = kernel.search_session_history(params).unwrap();
    assert_eq!(second.matches.len(), 1);
    assert_eq!(second.matches[0].task_id, matching.id);
    assert_eq!(second.scanned_turns, 1);
    assert_eq!(second.next_before_sequence, None);
}

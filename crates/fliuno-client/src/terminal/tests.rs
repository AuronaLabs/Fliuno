use super::*;
use fliuno_protocol::TerminalSubscriptionEndReason;
use serde_json::{Value, json};

fn snapshot() -> Value {
    json!({"sessionId":"session","terminalId":"terminal","presetId":"probe",
        "status":"running","supervisorConnected":true,"rows":24,"cols":80,
        "startOffset":8,"endOffset":12,"inputBytes":0,"outputClosed":false,
        "expiresAtUnixMs":0})
}

fn notification(id: &str, snapshot: Value, data: &str) -> Notification {
    Notification {
        method: "notifications/terminalOutput".into(),
        params: json!({"subscriptionId":id,"output":{"snapshot":snapshot,
            "from":8,"next":10,"gap":true,"dataBase64":data}}),
    }
}

fn end(after: u64) -> Notification {
    Notification {
        method: "notifications/terminalSubscriptionEnded".into(),
        params: json!({"subscriptionId":"subscription","sessionId":"session",
            "terminalId":"terminal","after":after,"reason":"overflow"}),
    }
}

#[test]
fn feed_routes_other_ids_and_advances_only_on_valid_bytes_and_same_identity() {
    let mut feed = TerminalFeed {
        identity: TerminalSubscription {
            subscription_id: "subscription".into(),
            session_id: "session".into(),
            terminal_id: "terminal".into(),
            after: 2,
        },
        limit: 2,
        ended: false,
    };
    assert!(matches!(
        feed.observe(notification("other", snapshot(), "bad")),
        Ok(TerminalFeedEvent::Unrelated(_))
    ));
    let mut wrong_session = snapshot();
    wrong_session["sessionId"] = json!("someone-else");
    for bad in [
        notification("subscription", wrong_session, "G/8="),
        notification("subscription", snapshot(), "AAAA"),
        notification("subscription", snapshot(), "!?=="),
    ] {
        assert!(matches!(
            feed.observe(bad),
            Err(Error::InvalidTerminalOutput)
        ));
        assert_eq!(feed.cursor(), 2);
    }
    let TerminalFeedEvent::Output(page) = feed
        .observe(notification("subscription", snapshot(), "G/8="))
        .unwrap()
    else {
        panic!("expected PTY bytes");
    };
    assert_eq!((page.from, page.next, page.gap), (8, 10, true));
    assert_eq!(page.bytes, [0x1b, 0xff]);
    assert!(matches!(
        feed.observe(end(12)),
        Err(Error::InvalidTerminalSubscription)
    ));
    assert_eq!(feed.cursor(), 10);
    let TerminalFeedEvent::Ended(end) = feed.observe(end(10)).unwrap() else {
        panic!("expected a subscription end");
    };
    assert_eq!(end.reason, TerminalSubscriptionEndReason::Overflow);
    assert!(matches!(
        feed.observe(notification("subscription", snapshot(), "G/8=")),
        Err(Error::InvalidTerminalSubscription)
    ));
}

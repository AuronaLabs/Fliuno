use super::{fixture, read_request};
use fliuno_config::{ModelCatalogDraft, ModelProtocol, ModelSettings, Provider, ReasoningEffort};
use fliuno_protocol::{
    ModelCatalogAccess, ModelCatalogStatus, ModelConfigurationStatus, ModelCredentialsStatus,
    ModelPresence, ModelReasoningEffortStatus,
};
use fliuno_providers::{check_model_connection, preview_model_catalog};
use serde_json::json;
use std::{io::Read, net::TcpListener, time::Duration};

#[test]
fn connection_check_reports_local_and_catalog_results_without_echoing_credentials() {
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("fixture.key");
    let mut settings = ModelSettings {
        provider: Provider::OpenAi,
        protocol: Some(ModelProtocol::ChatCompletions),
        name: Some("selected".into()),
        api_key_file: Some(key.clone()),
        ..Default::default()
    };
    let check = check_model_connection(&settings);
    assert_eq!(check.configuration, ModelConfigurationStatus::Ready);
    assert_eq!(check.credentials, ModelCredentialsStatus::Unavailable);
    assert_eq!(check.catalog, ModelCatalogStatus::NotChecked);

    settings.protocol = Some(ModelProtocol::Messages);
    let check = check_model_connection(&settings);
    assert_eq!(check.configuration, ModelConfigurationStatus::Invalid);
    assert_eq!(check.credentials, ModelCredentialsStatus::NotChecked);
    settings.protocol = Some(ModelProtocol::ChatCompletions);

    std::fs::write(key, "fixture-secret\n").unwrap();
    for (http, access, status) in [
        (
            401,
            ModelCatalogAccess::Unauthorized,
            ModelCatalogStatus::HttpError,
        ),
        (
            403,
            ModelCatalogAccess::Forbidden,
            ModelCatalogStatus::HttpError,
        ),
        (
            404,
            ModelCatalogAccess::Unverified,
            ModelCatalogStatus::Unavailable,
        ),
        (
            200,
            ModelCatalogAccess::Unverified,
            ModelCatalogStatus::InvalidResponse,
        ),
    ] {
        let (base, server) = fixture(vec![(http, json!({"error":"fixture-secret"}))]);
        settings.base_url = Some(base);
        let check = check_model_connection(&settings);
        assert_eq!(check.credentials, ModelCredentialsStatus::Ready);
        assert_eq!(check.catalog_access, access);
        assert_eq!(check.catalog, status);
        assert_eq!(check.http_status, (http != 200).then_some(http));
        assert_eq!(check.model_presence, ModelPresence::NotChecked);
        assert!(
            !serde_json::to_string(&check)
                .unwrap()
                .contains("fixture-secret")
        );
        let [(headers, body)] = &server.join().unwrap()[..] else {
            panic!("expected one request")
        };
        assert!(headers.starts_with("GET /v1/models HTTP/1.1"));
        assert!(body.is_null());
    }
}

#[test]
fn connection_check_reports_advertised_effort_without_inferring_missing_capabilities() {
    use ModelReasoningEffortStatus::{Advertised, NotAdvertised, ProviderDefault, Unknown};

    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("fixture.key");
    std::fs::write(&key, "fixture-key\n").unwrap();
    let mut settings = ModelSettings {
        provider: Provider::Anthropic,
        name: Some("selected".into()),
        api_key_file: Some(key),
        ..Default::default()
    };
    let published = json!({
        "thinking": {"supported": true},
        "effort": {"supported": true, "high": {"supported": true}}
    });
    for (effort, capabilities, advertised, thinking, expected) in [
        (
            Some(ReasoningEffort::High),
            published.clone(),
            Some(vec!["high"]),
            Some(true),
            Advertised,
        ),
        (
            Some(ReasoningEffort::Max),
            published.clone(),
            Some(vec!["high"]),
            Some(true),
            NotAdvertised,
        ),
        (
            None,
            published,
            Some(vec!["high"]),
            Some(true),
            ProviderDefault,
        ),
        (
            Some(ReasoningEffort::High),
            json!({"thinking":{"supported":false}}),
            None,
            Some(false),
            Unknown,
        ),
        (Some(ReasoningEffort::High), json!({}), None, None, Unknown),
    ] {
        let (base, server) = fixture(vec![(
            200,
            json!({
                "data": [{"id":"selected", "capabilities":capabilities}],
                "has_more": false,
            }),
        )]);
        settings.base_url = Some(base);
        settings.reasoning_effort = effort;
        let check = check_model_connection(&settings);
        assert_eq!(check.model_presence, ModelPresence::Present);
        let result = check.reasoning.unwrap();
        assert_eq!(
            result.configured_effort.as_deref(),
            effort.map(ReasoningEffort::as_str)
        );
        assert_eq!(
            result.advertised_efforts,
            advertised.map(|efforts| efforts.into_iter().map(str::to_owned).collect())
        );
        assert_eq!(result.supports_thinking, thinking);
        assert_eq!(result.effort_status, expected);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].0.starts_with("GET /v1/models?limit=100 "));
    }
}

#[test]
fn connection_check_only_marks_a_model_unlisted_after_a_complete_search() {
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("fixture.key");
    std::fs::write(&key, "fixture-key\n").unwrap();
    let mut settings = ModelSettings {
        provider: Provider::Anthropic,
        name: Some("selected".into()),
        api_key_file: Some(key),
        ..Default::default()
    };
    let first = json!({"data":[{"id":"first"}],"has_more":true,"last_id":"first"});
    let (base, server) = fixture(vec![
        (200, first.clone()),
        (200, json!({"data":[{"id":"selected"}],"has_more":false})),
    ]);
    settings.base_url = Some(base);
    let check = check_model_connection(&settings);
    assert_eq!(check.model_presence, ModelPresence::Present);
    assert_eq!(check.catalog_access, ModelCatalogAccess::Accepted);
    assert_eq!(check.pages_checked, 2);
    let requests = server.join().unwrap();
    assert!(
        requests[1]
            .0
            .starts_with("GET /v1/models?limit=100&after_id=first ")
    );

    let (base, server) = fixture(vec![(200, json!({"data":[],"has_more":false}))]);
    settings.base_url = Some(base);
    assert_eq!(
        check_model_connection(&settings).model_presence,
        ModelPresence::NotListed
    );
    server.join().unwrap();

    let pages = (0..4)
        .map(|index| {
            (
                200,
                json!({"data":[],"has_more":true,"last_id":format!("cursor{index}")}),
            )
        })
        .collect();
    let (base, server) = fixture(pages);
    settings.base_url = Some(base);
    let check = check_model_connection(&settings);
    assert_eq!(check.catalog, ModelCatalogStatus::Available);
    assert_eq!(check.model_presence, ModelPresence::Incomplete);
    assert_eq!(check.pages_checked, 4);
    server.join().unwrap();

    let (base, server) = fixture(vec![(200, first.clone()), (200, first)]);
    settings.base_url = Some(base);
    let check = check_model_connection(&settings);
    assert_eq!(check.catalog, ModelCatalogStatus::InvalidResponse);
    assert_eq!(check.model_presence, ModelPresence::Incomplete);
    server.join().unwrap();
}

#[test]
fn connection_check_times_out_without_waiting_for_generation_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}/v1/", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert!(read_request(&mut stream).0.starts_with("GET /v1/models "));
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        let _ = stream.read(&mut [0]);
    });
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("fixture.key");
    std::fs::write(&key, "fixture-key\n").unwrap();
    let check = check_model_connection(&ModelSettings {
        provider: Provider::OpenAi,
        base_url: Some(base_url),
        api_key_file: Some(key),
        ..Default::default()
    });
    assert_eq!(check.configuration, ModelConfigurationStatus::MissingModel);
    assert_eq!(check.catalog, ModelCatalogStatus::TimedOut);
    assert_eq!(check.model_presence, ModelPresence::NotSelected);
    server.join().unwrap();
}

#[test]
fn draft_catalog_does_not_expose_credential_echoed_in_a_successful_page() {
    let (base, worker) = fixture(vec![(
        200,
        json!({
            "has_more": true,
            "last_id": "secret-fixture-key",
            "data": [
                {"id":"secret-fixture-key"},
                {"id":"valid-model", "display_name":"key: secret-fixture-key"}
            ]
        }),
    )]);
    let preview = preview_model_catalog(
        ModelCatalogDraft {
            provider: Provider::Anthropic,
            protocol: ModelProtocol::Messages,
            base_url: Some(base),
            cursor: None,
        },
        "secret-fixture-key",
    )
    .unwrap();
    assert!(
        !serde_json::to_string(&preview)
            .unwrap()
            .contains("secret-fixture-key")
    );
    let page = preview.page.unwrap();
    assert_eq!(page.models.len(), 1);
    assert_eq!(page.models[0].id, "valid-model");
    assert!(page.truncated);
    assert!(page.next_cursor.is_none());
    worker.join().unwrap();
}

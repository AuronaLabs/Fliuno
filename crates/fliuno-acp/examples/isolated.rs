//! A small offline-capable consumer of the isolated ACP adapter.
//! Build the fixture with --features test-fixtures, then pass its absolute path.
use fliuno_acp::{CancellationToken, Client, ClientOptions, Event, PermissionDecision};
use std::{collections::BTreeMap, path::PathBuf};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let program: PathBuf = args
        .next()
        .ok_or("usage: isolated <absolute-native-exe> <prompt>")?
        .into();
    let prompt = args.next().ok_or("prompt is required")?;
    if args.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    let (client, mut events) = Client::connect_isolated(
        fliuno_sandbox::LaunchSpec {
            program_sha256: None,
            assets: vec![],
            program,
            args: Vec::new(),
            env: BTreeMap::new(),
            resources: Default::default(),
            linux_cgroup_parent: None,
        },
        ClientOptions::default(),
    )
    .await?;
    let session = client
        .new_session(client.private_directory().ok_or("missing private cwd")?)
        .await?
        .session_id;
    let cancellation = CancellationToken::new();
    let request = client.prompt_text(&session, prompt, &cancellation);
    tokio::pin!(request);
    let result = loop {
        tokio::select! {
            biased;
            event = events.recv() => match event {
                Some(Event::Update(update)) => println!("{}", serde_json::to_string(&update)?),
                Some(Event::PermissionRequested {id, request, ..}) => {
                    // This example has no approval UI. Deny rather than inventing
                    // consent; a desktop client would show the request to a user.
                    eprintln!("permission denied by example: {}", request.tool_call.tool_call_id);
                    client.respond_permission(&session, &id, PermissionDecision::Deny)?;
                }
                Some(Event::PermissionClosed {..}) => {},
                Some(Event::HostRequest {..} | Event::HostRequestClosed {..}) => unreachable!("filesystem capabilities are disabled"),
                None => break Err(client.failure().unwrap_or(fliuno_acp::Error::Closed)),
            },
            result = &mut request => break result,
        }
    };
    client.close().await;
    println!("stopReason: {:?}", result?.stop_reason);
    Ok(())
}

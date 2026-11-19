//! Run a local HTTP host with Fliuno's offline demo provider.
use fliuno_client::Client;
use fliuno_protocol::{Session, SessionCreateParams, Workspace, WorkspaceOpenParams};
use serde_json::json;
use std::{
    env,
    io::{self, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, oneshot},
};

struct OwnedPeer(Child);

impl Drop for OwnedPeer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args_os().skip(1).map(PathBuf::from).collect();
    let [server, workspace, db] = args.as_slice() else {
        return Err("usage: local_host APP_SERVER WORKSPACE DB".into());
    };
    let run_token = token("FLIUNO_DEMO_RUN_TOKEN")?;
    let review_token = token("FLIUNO_DEMO_REVIEW_TOKEN")?;
    if run_token == review_token {
        return Err("use distinct run and review tokens".into());
    }
    let root = workspace
        .to_str()
        .ok_or("workspace path must be UTF-8")?
        .to_owned();
    let mut peer = OwnedPeer(
        Command::new(server)
            .arg("--db")
            .arg(db)
            .env("FLIUNO_PROVIDER", "demo")
            .env_remove("FLIUNO_DEMO_RUN_TOKEN")
            .env_remove("FLIUNO_DEMO_REVIEW_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut client = Client::connect(
        BufReader::new(peer.0.stdout.take().ok_or("app-server stdout missing")?),
        peer.0.stdin.take().ok_or("app-server stdin missing")?,
        "local-http-demo",
        |_| {},
    )?;
    let workspace: Workspace =
        client.call("workspace/open", &WorkspaceOpenParams { root }, |_| {})?;
    let session: Session = client.call(
        "session/create",
        &SessionCreateParams {
            workspace_id: workspace.id,
            title: Some("AG-UI HTTP demo".into()),
            client_request_id: Some("ag-ui-http-demo".into()),
        },
        |_| {},
    )?;
    let run_session = session.id.clone();
    let review_session = session.id.clone();
    let client = Arc::new(Mutex::new(client));
    let run = move |headers: &axum::http::HeaderMap| {
        authorized(headers, &run_token).then(|| run_session.clone())
    };
    let review = move |headers: &axum::http::HeaderMap| {
        authorized(headers, &review_token).then(|| review_session.clone())
    };
    let app = if env::var_os("FLIUNO_DEMO_INTERRUPTS").is_some() {
        fliuno_ag_ui_http::router_with_interrupts(client, run, review)
    } else {
        fliuno_ag_ui_http::router_with_approvals(client, run, review)
    };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    println!(
        "{}",
        json!({"address":listener.local_addr()?.to_string(),"sessionId":session.id})
    );
    io::stdout().flush()?;
    let (stop, cancelled) = oneshot::channel();
    // EOF or Enter stops the local demo, even while a response awaits approval.
    std::thread::spawn(move || {
        let _ = io::stdin().read_line(&mut String::new());
        let _ = stop.send(());
    });
    tokio::select! {
        result = axum::serve(listener, app) => result?,
        _ = cancelled => {},
    }
    Ok(())
}

fn token(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let value = env::var(name).map_err(|_| format!("set {name}"))?;
    if !(16..=256).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!("{name} must be 16–256 printable ASCII characters").into());
    }
    Ok(format!("Bearer {value}"))
}

fn authorized(headers: &axum::http::HeaderMap, expected: &str) -> bool {
    let mut values = headers.get_all(axum::http::header::AUTHORIZATION).iter();
    values.next().is_some_and(|value| value == expected) && values.next().is_none()
}

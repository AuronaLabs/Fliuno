//! Deterministic loopback wire peer, shared by adapter and executable tests.
//! It is deliberately a fixture, not another production MCP implementation.
#![allow(dead_code)]
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
}

pub struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    hold: bool,
}
impl Response {
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
            headers: Vec::new(),
            hold: false,
        }
    }
    pub fn sse(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: body.into_bytes(),
            headers: Vec::new(),
            hold: false,
        }
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    pub fn hold() -> Self {
        Self {
            hold: true,
            ..Self::json(200, Value::Null)
        }
    }
}

pub struct Fixture {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Fixture {
    pub fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let task_stop = stopped.clone();
        let task_requests = requests.clone();
        let handler = Arc::new(handler);
        let task = thread::spawn(move || {
            let mut connections = Vec::new();
            while !task_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let handler = handler.clone();
                        let requests = task_requests.clone();
                        let stopped = task_stop.clone();
                        connections.push(thread::spawn(move || {
                            let request = read_request(&mut stream);
                            requests.lock().unwrap().push(request.clone());
                            let response = handler(&request);
                            if response.hold {
                                stream.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
                                let mut byte = [0];
                                while !stopped.load(Ordering::SeqCst) {
                                    match stream.read(&mut byte) {
                                        Ok(0) => break,
                                        Ok(_) => {},
                                        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {},
                                        Err(_) => break,
                                    }
                                }
                                return;
                            }
                            let _ = write!(stream, "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n", response.status, response.content_type, response.body.len());
                            for (name, value) in response.headers {
                                let _ = write!(stream, "{name}: {value}\r\n");
                            }
                            let _ = stream.write_all(b"\r\n");
                            let _ = stream.write_all(&response.body);
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                }
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            base,
            requests,
            stopped,
            thread: Some(task),
        }
    }
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
    pub fn wait_for(&self, method: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.requests().iter().any(|r| r.body["method"] == method) {
            assert!(
                Instant::now() < deadline,
                "fixture did not receive {method}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            // Preserve the original assertion when a test is already unwinding.
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    // Accepted sockets inherit the listener's nonblocking mode on Windows.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    assert!(reader.read_line(&mut first).unwrap() > 0);
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap().into();
    let path = parts.next().unwrap().into();
    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.split_once(':').unwrap();
        headers.insert(key.to_ascii_lowercase(), value.trim().into());
    }
    let length: usize = headers
        .get("content-length")
        .map_or(0, |s: &String| s.parse().unwrap());
    assert!(length <= 2 * 1024 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    Request {
        method,
        path,
        headers,
        body,
    }
}

#[derive(Clone, Copy)]
pub enum Version {
    Modern,
    Legacy(&'static str),
}

pub fn reply(request: &Request, version: Version) -> Response {
    if request.method == "GET" {
        return Response::json(405, Value::Null);
    }
    if request.method == "DELETE" {
        return Response::json(200, Value::Null);
    }
    let id = &request.body["id"];
    let result = match request.body["method"].as_str().unwrap() {
        "server/discover" => match version {
            Version::Modern => {
                json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"})
            }
            Version::Legacy(_) => {
                return Response::json(
                    200,
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"legacy fixture"}}),
                );
            }
        },
        "initialize" => {
            let Version::Legacy(version) = version else {
                panic!("modern peer received initialize");
            };
            return Response::json(200, json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":version,"capabilities":{"tools":{}},"serverInfo":{"name":"legacy-fixture","version":"0.1"}}}))
                .header("Mcp-Session-Id", "fixture-session");
        }
        "notifications/initialized" | "notifications/cancelled" => {
            return Response::json(202, Value::Null);
        }
        "tools/list" => {
            json!({"tools":[{"name":"echo","description":"Loopback fixture","inputSchema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"}},"additionalProperties":false}}]})
        }
        "tools/call" => {
            json!({"content":[{"type":"text","text":request.body["params"]["arguments"]["text"]}],"isError":false})
        }
        method => panic!("unexpected fixture method: {method}"),
    };
    Response::json(200, json!({"jsonrpc":"2.0","id":id,"result":result}))
}

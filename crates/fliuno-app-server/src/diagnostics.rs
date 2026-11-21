//! Bounded, opt-in writers for redacted execution diagnostics and OTLP/HTTP.
use crossbeam_channel::{Receiver, Sender};
use fliuno_core::diagnostics::{self, Destinations, Outcome, SpanRecord};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::{BufWriter, Write},
    path::Path,
    thread,
    time::Duration,
};

const BATCH_SIZE: usize = 64;

pub(super) struct Writer {
    stop: Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) fn start(
    path: Option<&Path>,
    endpoint: Option<&str>,
) -> Result<Option<Writer>, Box<dyn std::error::Error>> {
    if path.is_none() && endpoint.is_none() {
        return Ok(None);
    }
    let exporter = endpoint.map(Exporter::new).transpose()?;
    let file = path
        .map(|path| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map(BufWriter::new)
        })
        .transpose()?;
    let destinations = Destinations {
        path: path.map(std::path::absolute).transpose()?,
        endpoint: endpoint.map(str::to_owned),
    };
    let records =
        diagnostics::enable(destinations).ok_or("execution diagnostics are already enabled")?;
    let (stop, stopped) = crossbeam_channel::bounded(1);
    let thread = thread::Builder::new()
        .name("fliuno-diagnostics".into())
        .spawn(move || {
            write_records(file, exporter, records, stopped);
        })?;
    Ok(Some(Writer {
        stop,
        thread: Some(thread),
    }))
}

fn write_records(
    mut file: Option<BufWriter<std::fs::File>>,
    mut exporter: Option<Exporter>,
    records: Receiver<SpanRecord>,
    stopped: Receiver<()>,
) {
    loop {
        let mut batch = Vec::with_capacity(BATCH_SIZE);
        let exit = crossbeam_channel::select_biased! {
            recv(stopped) -> _ => true,
            recv(records) -> record => match record {
                Ok(record) => { batch.push(record); false }
                Err(_) => true,
            },
        };
        // Drain the bounded queue on clean exit, but export at most one batch
        // during shutdown so an unreachable collector cannot hold the host.
        let limit = if exit {
            records.len()
        } else {
            BATCH_SIZE - batch.len()
        };
        batch.extend(records.try_iter().take(limit));
        if let Some(destination) = file.as_mut()
            && (batch
                .iter()
                .try_for_each(|record| {
                    serde_json::to_writer(&mut *destination, record)?;
                    destination
                        .write_all(b"\n")
                        .map_err(serde_json::Error::io)?;
                    Ok::<_, serde_json::Error>(())
                })
                .is_err()
                || destination.flush().is_err())
        {
            eprintln!("fliuno: diagnostics log could not be written");
            file = None;
        }
        if let Some(exporter) = exporter.as_mut() {
            exporter.send(&batch[..batch.len().min(BATCH_SIZE)]);
        }
        if exit {
            break;
        }
    }
}

struct Exporter {
    endpoint: reqwest::Url,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl Exporter {
    fn new(raw: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let endpoint = reqwest::Url::parse(raw)?;
        let local = endpoint
            .host_str()
            .and_then(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|address| address.is_loopback());
        if !(endpoint.scheme() == "https" || local && endpoint.scheme() == "http")
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.host().is_none()
        {
            return Err(
                "OTLP endpoint requires HTTPS or a loopback HTTP address without credentials"
                    .into(),
            );
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            endpoint,
            client,
            runtime,
        })
    }

    fn send(&self, records: &[SpanRecord]) {
        if records.is_empty() {
            return;
        }
        let spans: Vec<_> = records.iter().map(otlp_span).collect();
        let payload = json!({"resourceSpans":[{
            "resource":{"attributes":[{"key":"service.name","value":{"stringValue":"fliuno"}}]},
            "scopeSpans":[{"scope":{"name":"fliuno.execution"},"spans":spans}]
        }]});
        let _ = self.runtime.block_on(async {
            self.client
                .post(self.endpoint.clone())
                .json(&payload)
                .send()
                .await
        });
    }
}

fn otlp_span(record: &SpanRecord) -> Value {
    let mut attributes = Vec::new();
    let mut add_ref = |key, value: &Option<String>| {
        if let Some(value) = value {
            attributes.push(json!({"key":key,"value":{"stringValue":value}}));
        }
    };
    add_ref("fliuno.run.ref", &record.run_ref);
    add_ref("fliuno.task.ref", &record.task_ref);
    for (key, value) in [
        ("fliuno.attempt", record.measurements.attempt.map(u64::from)),
        (
            "gen_ai.usage.input_tokens",
            record.measurements.input_tokens,
        ),
        (
            "gen_ai.usage.output_tokens",
            record.measurements.output_tokens,
        ),
        ("fliuno.cpu.ms", record.measurements.cpu_ms),
        (
            "fliuno.memory.peak_bytes",
            record.measurements.peak_memory_bytes,
        ),
    ] {
        if let Some(value) = value {
            attributes.push(
                json!({"key":key,"value":{"intValue":value.min(i64::MAX as u64).to_string()}}),
            );
        }
    }
    let mut span = json!({"name":record.operation,"traceId":record.trace_id,"spanId":record.span_id,
        "kind":1,"startTimeUnixNano":record.start_unix_ns.to_string(),
        "endTimeUnixNano":record.start_unix_ns.saturating_add(record.duration_ns).to_string(),
        "status":{"code":if matches!(record.outcome, Outcome::Completed) { 1 } else { 2 }},
        "attributes":attributes});
    if let Some(parent) = &record.parent_span_id {
        span["parentSpanId"] = json!(parent);
    }
    span
}

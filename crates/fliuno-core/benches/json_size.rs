use serde_json::{Value, json};
use std::{hint::black_box, time::Instant};

type Result<T> = std::result::Result<T, serde_json::Error>;

#[path = "../src/json_size.rs"]
#[allow(unused_imports)]
mod json_size;

const SAMPLES: usize = 11;

fn payload(repetitions: usize) -> Value {
    let text = "line\nbreak \"quoted\" \\ tab\t nul\u{0} 你好😀".repeat(repetitions);
    let records = (0..16)
        .map(|index| {
            json!({
                "index": index,
                "state": "working",
                "labels": ["provider", "stream", "durable"],
            })
        })
        .collect::<Vec<_>>();

    json!({
        "type": "a2a_status_update",
        "message": {"parts": [{"text": text}]},
        "records": records,
    })
}

fn elapsed_ns_per_op(mut operation: impl FnMut() -> usize, iterations: usize) -> u128 {
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(operation());
    }
    start.elapsed().as_nanos() / iterations as u128
}

fn median(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[SAMPLES / 2]
}

fn compare(value: &Value, iterations: usize) -> (u128, u128) {
    let mut streamed = Vec::with_capacity(SAMPLES);
    let mut buffered = Vec::with_capacity(SAMPLES);
    for _ in 0..32 {
        black_box(json_size::json_size(black_box(value)).unwrap());
        black_box(serde_json::to_vec(black_box(value)).unwrap().len());
    }

    for sample in 0..SAMPLES {
        let stream = || json_size::json_size(black_box(value)).unwrap();
        let buffer = || serde_json::to_vec(black_box(value)).unwrap().len();
        if sample % 2 == 0 {
            streamed.push(elapsed_ns_per_op(stream, iterations));
            buffered.push(elapsed_ns_per_op(buffer, iterations));
        } else {
            buffered.push(elapsed_ns_per_op(buffer, iterations));
            streamed.push(elapsed_ns_per_op(stream, iterations));
        }
    }
    (median(streamed), median(buffered))
}

fn run_case(label: &str, value: &Value, iterations: usize) -> Result<()> {
    let expected = serde_json::to_vec(value)?.len();
    assert_eq!(json_size::json_size(value)?, expected);

    let (streamed, buffered) = compare(value, iterations);
    let ratio = buffered as f64 / streamed as f64;
    println!(
        "{label}: bytes={expected} iterations={iterations} stream_ns/op={streamed} buffered_ns/op={buffered} buffered/stream={ratio:.3}"
    );
    Ok(())
}

fn main() -> Result<()> {
    println!(
        "json_size release benchmark: os={} arch={} samples={SAMPLES}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    run_case("small", &payload(24), 10_000)?;
    run_case("large", &payload(2_048), 500)?;
    Ok(())
}

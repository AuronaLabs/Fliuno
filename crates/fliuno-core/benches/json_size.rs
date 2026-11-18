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

fn compare(value: &Value, iterations: usize) -> (u128, u128, u128) {
    let mut samples: [Vec<u128>; 3] = std::array::from_fn(|_| Vec::with_capacity(SAMPLES));
    let operations: [fn(&Value) -> usize; 3] = [
        |value| json_size::json_size(value).unwrap(),
        |value| serde_json::to_vec(value).unwrap().len(),
        |value| value.to_string().len(),
    ];
    for _ in 0..32 {
        for operation in operations {
            black_box(operation(black_box(value)));
        }
    }
    for sample in 0..SAMPLES {
        for index in 0..operations.len() {
            let index = (sample + index) % operations.len();
            let operation = operations[index];
            samples[index].push(elapsed_ns_per_op(
                || operation(black_box(value)),
                iterations,
            ));
        }
    }
    let [streamed, buffered, display] = samples.map(median);
    (streamed, buffered, display)
}

fn run_case(label: &str, value: &Value, iterations: usize) -> Result<()> {
    let expected = serde_json::to_vec(value)?.len();
    assert_eq!(json_size::json_size(value)?, expected);
    assert_eq!(value.to_string().len(), expected);

    let (streamed, buffered, display) = compare(value, iterations);
    let buffer_ratio = buffered as f64 / streamed as f64;
    let display_ratio = display as f64 / streamed as f64;
    println!(
        "{label}: bytes={expected} iterations={iterations} stream_ns/op={streamed} buffered_ns/op={buffered} display_ns/op={display} buffered/stream={buffer_ratio:.3} display/stream={display_ratio:.3}"
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
    run_case("near-mcp-limit", &payload(19_000), 60)?;
    Ok(())
}

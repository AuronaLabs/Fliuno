//! Line framing and JSON-RPC correlation shared by the client entry points.
use super::{DEFAULT_RESPONSE_BYTES, Error, Notification};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, Write};

#[derive(Serialize)]
struct Request<'a, P: ?Sized> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: &'a P,
}

#[derive(Default)]
struct BoundedRequest {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl Write for BoundedRequest {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Reserve the line delimiter in the app-server's frame limit.
        if bytes.len() >= fliuno_protocol::MAX_APP_REQUEST_BYTES - self.bytes.len() {
            self.overflowed = true;
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode<P: Serialize + ?Sized>(
    id: u64,
    method: &str,
    params: &P,
) -> Result<Vec<u8>, Error> {
    let mut output = BoundedRequest::default();
    serde_json::to_writer(
        &mut output,
        &Request {
            jsonrpc: "2.0",
            id,
            method,
            params,
        },
    )
    .map_err(|_| {
        if output.overflowed {
            Error::RequestTooLarge
        } else {
            Error::RequestEncoding
        }
    })?;
    output.bytes.push(b'\n');
    Ok(output.bytes)
}

pub(super) fn send<P: Serialize + ?Sized>(
    writer: &mut impl Write,
    id: u64,
    method: &str,
    params: &P,
) -> Result<(), Error> {
    writer.write_all(&encode(id, method, params)?)?;
    writer.flush()?;
    Ok(())
}

pub(super) fn read(reader: &mut impl BufRead, buffer: &mut Vec<u8>) -> Result<Value, Error> {
    buffer.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if buffer.is_empty() {
                Err(Error::Closed)
            } else {
                Err(Error::InvalidFrame("missing line delimiter"))
            };
        }
        let delimiter = available.iter().position(|b| *b == b'\n');
        let count = delimiter.map_or(available.len(), |position| position + 1);
        if count > DEFAULT_RESPONSE_BYTES - buffer.len() {
            return Err(Error::ResponseTooLarge);
        }
        buffer.extend_from_slice(&available[..count]);
        reader.consume(count);
        if delimiter.is_some() {
            break;
        }
    }
    let frame: Value =
        serde_json::from_slice(buffer).map_err(|_| Error::InvalidFrame("invalid JSON"))?;
    if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(Error::InvalidFrame("invalid JSON-RPC version"));
    }
    Ok(frame)
}

pub(super) fn notification(frame: Value) -> Result<Notification, Error> {
    if ["id", "result", "error"]
        .iter()
        .any(|key| frame.get(key).is_some())
    {
        return Err(Error::InvalidFrame("expected a notification"));
    }
    serde_json::from_value(frame).map_err(|_| Error::InvalidFrame("invalid notification"))
}

#[derive(Deserialize)]
struct RpcError {
    code: i32,
    message: String,
}

// The outer Result concerns framing and correlation. The inner Result is the
// server's reply and can contain a valid RPC error without losing the stream.
pub(super) fn reply(
    reader: &mut impl BufRead,
    buffer: &mut Vec<u8>,
    id: u64,
    mut on_notification: impl FnMut(Notification),
) -> Result<Result<Value, Error>, Error> {
    loop {
        let Value::Object(mut frame) = read(reader, buffer)? else {
            return Err(Error::InvalidFrame("expected an object"));
        };
        if frame.contains_key("method") {
            on_notification(notification(Value::Object(frame))?);
            continue;
        }
        if frame.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(Error::InvalidFrame(
                "response ID does not match the request",
            ));
        }
        return match (frame.remove("result"), frame.remove("error")) {
            (Some(result), None) => Ok(Ok(result)),
            (None, Some(error)) => {
                let RpcError { code, message } = serde_json::from_value(error)
                    .map_err(|_| Error::InvalidFrame("invalid RPC error"))?;
                Ok(Err(Error::Rpc { code, message }))
            }
            _ => Err(Error::InvalidFrame("expected one of result or error")),
        };
    }
}

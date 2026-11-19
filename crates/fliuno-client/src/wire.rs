//! Line framing and JSON-RPC correlation shared by the client entry points.
use super::{DEFAULT_RESPONSE_BYTES, Error, Notification};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use std::borrow::Cow;
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

#[derive(Deserialize)]
pub(super) struct Frame<'a> {
    #[serde(borrow)]
    jsonrpc: Cow<'a, str>,
    #[serde(borrow, default, deserialize_with = "present")]
    method: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present")]
    id: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present")]
    result: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present")]
    error: Option<&'a RawValue>,
    #[serde(borrow)]
    params: Option<&'a RawValue>,
}

// Presence matters even when the JSON field is null. In particular, a null
// result is valid and a null ID cannot identify a notification.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<&'de RawValue>, D::Error> {
    <&RawValue>::deserialize(deserializer).map(Some)
}

pub(super) fn read<'a>(
    reader: &mut impl BufRead,
    buffer: &'a mut Vec<u8>,
) -> Result<Frame<'a>, Error> {
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
    if buffer.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(Error::InvalidFrame("expected an object"));
    }
    let frame: Frame<'a> = serde_json::from_slice(buffer)
        .map_err(|_| Error::InvalidFrame("invalid JSON or envelope"))?;
    if frame.jsonrpc != "2.0" {
        return Err(Error::InvalidFrame("invalid JSON-RPC version"));
    }
    Ok(frame)
}

pub(super) fn notification(frame: Frame<'_>) -> Result<Notification, Error> {
    if frame.id.is_some() || frame.result.is_some() || frame.error.is_some() {
        return Err(Error::InvalidFrame("expected a notification"));
    }
    let method = frame
        .method
        .ok_or(Error::InvalidFrame("missing notification method"))?;
    let params = frame
        .params
        .map(|raw| serde_json::from_str(raw.get()))
        .transpose()
        .map_err(|_| Error::InvalidFrame("invalid notification params"))?
        .unwrap_or_default();
    let method = serde_json::from_str(method.get())
        .map_err(|_| Error::InvalidFrame("invalid notification method"))?;
    Ok(Notification { method, params })
}

#[derive(Deserialize)]
struct RpcError {
    code: i32,
    message: String,
}

// The outer Result concerns framing and correlation. The inner Result is the
// server's reply and can contain a valid RPC error without losing the stream.
pub(super) fn reply<T: DeserializeOwned>(
    reader: &mut impl BufRead,
    buffer: &mut Vec<u8>,
    id: u64,
    mut on_notification: impl FnMut(Notification),
) -> Result<Result<T, Error>, Error> {
    loop {
        let frame = read(reader, buffer)?;
        if frame.method.is_some() {
            on_notification(notification(frame)?);
            continue;
        }
        let correlated = frame
            .id
            .and_then(|raw| serde_json::from_str::<u64>(raw.get()).ok());
        if correlated != Some(id) {
            return Err(Error::InvalidFrame(
                "response ID does not match the request",
            ));
        }
        return match (frame.result, frame.error) {
            (Some(result), None) => {
                Ok(serde_json::from_str(result.get()).map_err(|_| Error::InvalidResult))
            }
            (None, Some(error)) => {
                let RpcError { code, message } = serde_json::from_str(error.get())
                    .map_err(|_| Error::InvalidFrame("invalid RPC error"))?;
                Ok(Err(Error::Rpc { code, message }))
            }
            _ => Err(Error::InvalidFrame("expected one of result or error")),
        };
    }
}

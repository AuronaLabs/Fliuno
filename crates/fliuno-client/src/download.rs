//! Stream immutable artifact bytes with a caller-supplied length and digest.
use crate::{Client, Error, Notification};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use fliuno_protocol::{
    ArtifactChunk, ArtifactDigest, ArtifactDigestBatch, ArtifactDigestBatchParams,
    ArtifactDigestEntry, ArtifactDownloadBatch, ArtifactDownloadBatchEntry,
    ArtifactDownloadBatchParams, ArtifactDownloadStart, ArtifactGetParams, ArtifactReadParams,
    MAX_ARTIFACT_READ_BYTES,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{self, BufRead, Read, Seek, SeekFrom, Write},
};

impl<R: BufRead, W: Write> Client<R, W> {
    /// Fetch the digest of the stored body. A caller with a pinned digest can
    /// pass it directly to [`Self::download_artifact`].
    pub fn artifact_digest(
        &mut self,
        artifact: &ArtifactGetParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<ArtifactDigest, Error> {
        self.call("artifacts/digest", artifact, on_notification)
    }

    /// Fetch recorded digests in request order without hashing legacy bodies.
    /// Use `artifact_digest` for an unrecorded item before downloading it.
    pub fn artifact_digests(
        &mut self,
        request: &ArtifactDigestBatchParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<ArtifactDigestBatch, Error> {
        let batch: ArtifactDigestBatch =
            self.call("artifacts/getDigests", request, on_notification)?;
        if batch.items.len() != request.artifacts.len() {
            return Err(Error::InvalidResult);
        }
        for item in &batch.items {
            if let ArtifactDigestEntry::Ready { digest } = item {
                validate_sha256(&digest.sha256)?;
            }
        }
        Ok(batch)
    }

    /// Obtain the saved digest and first range of each item in request order.
    /// Callers should negotiate `artifactDownloadStartBatch` before using this method.
    pub fn open_artifact_downloads(
        &mut self,
        request: &ArtifactDownloadBatchParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<ArtifactDownloadBatch, Error> {
        let batch: ArtifactDownloadBatch =
            self.call("artifacts/openDownloads", request, on_notification)?;
        if batch.items.len() != request.artifacts.len() {
            return Err(Error::InvalidResult);
        }
        let mut seen = HashMap::with_capacity(batch.items.len());
        for (artifact, item) in request.artifacts.iter().zip(&batch.items) {
            let key = (&artifact.task_id, &artifact.artifact_id);
            if let Some(previous) = seen.insert(key, item) {
                if previous != item {
                    return Err(Error::InvalidResult);
                }
                continue;
            }
            if let ArtifactDownloadBatchEntry::Ready { download } = item {
                validate_sha256(&download.digest.sha256)?;
                validate_chunk(
                    &download.first_chunk,
                    0,
                    download.digest.content_bytes,
                    request.limit,
                )?;
            }
        }
        Ok(batch)
    }

    /// Stream the artifact into `output` and verify its final SHA-256.
    /// Use a staging writer if a partial or unverified download must not be
    /// visible at its destination. The caller owns publication and flushing.
    pub fn download_artifact(
        &mut self,
        artifact: &ArtifactGetParams,
        expected: &ArtifactDigest,
        output: &mut impl Write,
        on_notification: impl FnMut(Notification),
    ) -> Result<(), Error> {
        // Preserve the server identity/length check for a fresh empty download.
        Download::new(expected)?.receive(self, artifact, output, on_notification)
    }

    /// Start a download without a pinned digest. Persist the received digest in
    /// `record_digest` before writing any bytes so an interrupted download can
    /// resume with [`Self::resume_artifact_download`]. An error from the callback
    /// leaves the output untouched. The caller owns flushing and publication.
    pub fn download_artifact_with_digest(
        &mut self,
        artifact: &ArtifactGetParams,
        output: &mut impl Write,
        record_digest: impl FnOnce(&ArtifactDigest) -> io::Result<()>,
        mut on_notification: impl FnMut(Notification),
    ) -> Result<ArtifactDigest, Error> {
        if self
            .info()
            .capabilities
            .get("artifactDownloadStart")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            let start: ArtifactDownloadStart =
                self.call("artifacts/openDownload", artifact, &mut on_notification)?;
            validate_chunk(
                &start.first_chunk,
                0,
                start.digest.content_bytes,
                MAX_ARTIFACT_READ_BYTES,
            )?;
            return self.download_artifact_from_start(
                artifact,
                start,
                output,
                record_digest,
                on_notification,
            );
        }
        let digest = self.artifact_digest(artifact, &mut on_notification)?;
        let download = Download::new(&digest)?;
        record_digest(&digest).map_err(Error::ArtifactCheckpoint)?;
        download.receive(self, artifact, output, on_notification)?;
        Ok(digest)
    }

    /// Consume a verified range shape from a batch and fetch any remaining bytes.
    /// Pass the artifact identity from the same request position. The digest is
    /// recorded before writing into the caller's staging writer.
    pub fn download_artifact_from_start(
        &mut self,
        artifact: &ArtifactGetParams,
        start: ArtifactDownloadStart,
        output: &mut impl Write,
        record_digest: impl FnOnce(&ArtifactDigest) -> io::Result<()>,
        on_notification: impl FnMut(Notification),
    ) -> Result<ArtifactDigest, Error> {
        let ArtifactDownloadStart {
            digest,
            first_chunk,
        } = start;
        let prefix_bytes = u32::try_from(first_chunk.next_offset)
            .ok()
            .filter(|bytes| {
                *bytes <= MAX_ARTIFACT_READ_BYTES && (*bytes != 0 || digest.content_bytes == 0)
            })
            .ok_or(Error::InvalidArtifactChunk)?;
        let mut download = Download::new(&digest)?;
        let count = download.decode(first_chunk, prefix_bytes)?;
        record_digest(&digest).map_err(Error::ArtifactCheckpoint)?;
        download.write_chunk(count, output)?;
        if download.after == digest.content_bytes {
            download.verify()?;
        } else {
            download.receive(self, artifact, output, on_notification)?;
        }
        Ok(digest)
    }

    /// Continue an interrupted download into an exclusively owned staging file.
    /// Rehash the existing prefix locally and request only the missing bytes.
    /// Use the same artifact identity and pinned digest across reconnects. A
    /// complete staging file is verified locally without issuing a request.
    ///
    /// The current file length determines the offset, regardless of the initial
    /// file position. Failures can leave unverified bytes in the staging file.
    /// Discard the staging data on a digest mismatch. The caller owns cleanup,
    /// flushing, and publication after verification.
    pub fn resume_artifact_download(
        &mut self,
        artifact: &ArtifactGetParams,
        expected: &ArtifactDigest,
        staging: &mut (impl Read + Write + Seek),
        on_notification: impl FnMut(Notification),
    ) -> Result<(), Error> {
        let mut download = Download::new(expected)?;
        download.hash_prefix(staging)?;
        if download.after == expected.content_bytes {
            return download.verify();
        }
        download.receive(self, artifact, staging, on_notification)
    }
}

fn validate_sha256(sha256: &str) -> Result<(), Error> {
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::InvalidArtifactDigest);
    }
    Ok(())
}

fn validate_chunk(
    chunk: &ArtifactChunk,
    offset: u64,
    length: u64,
    limit: u32,
) -> Result<usize, Error> {
    if offset > length || limit > MAX_ARTIFACT_READ_BYTES {
        return Err(Error::InvalidArtifactChunk);
    }
    let count = (length - offset).min(u64::from(limit)) as usize;
    if chunk.content_bytes != length
        || chunk.offset != offset
        || chunk.next_offset != offset + count as u64
        || chunk.data_base64.len() != count.div_ceil(3) * 4
    {
        return Err(Error::InvalidArtifactChunk);
    }
    Ok(count)
}

struct Download<'a> {
    expected: &'a ArtifactDigest,
    after: u64,
    sha256: Sha256,
    buffer: Vec<u8>,
}

impl<'a> Download<'a> {
    fn new(expected: &'a ArtifactDigest) -> Result<Self, Error> {
        validate_sha256(&expected.sha256)?;
        let buffer_bytes = expected
            .content_bytes
            .min(u64::from(MAX_ARTIFACT_READ_BYTES)) as usize;
        Ok(Self {
            expected,
            after: 0,
            sha256: Sha256::new(),
            // The Base64 decoder's estimate rounds up to a full group.
            buffer: vec![0; buffer_bytes.div_ceil(3) * 3],
        })
    }

    fn hash_prefix(&mut self, staging: &mut (impl Read + Seek)) -> Result<(), Error> {
        let length = staging
            .seek(SeekFrom::End(0))
            .map_err(Error::ArtifactStagingRead)?;
        if length > self.expected.content_bytes {
            return Err(Error::InvalidArtifactPrefix);
        }
        staging.rewind().map_err(Error::ArtifactStagingRead)?;
        while self.after < length {
            let count = (length - self.after).min(u64::from(MAX_ARTIFACT_READ_BYTES)) as usize;
            let bytes = &mut self.buffer[..count];
            staging
                .read_exact(bytes)
                .map_err(Error::ArtifactStagingRead)?;
            self.sha256.update(bytes);
            self.after += count as u64;
        }
        // The read leaves the staging file positioned at the end for appending.
        Ok(())
    }

    fn receive<R: BufRead, W: Write>(
        mut self,
        client: &mut Client<R, W>,
        artifact: &ArtifactGetParams,
        output: &mut impl Write,
        mut on_notification: impl FnMut(Notification),
    ) -> Result<(), Error> {
        let mut request = ArtifactReadParams {
            task_id: artifact.task_id.clone(),
            artifact_id: artifact.artifact_id.clone(),
            after: self.after,
            limit: MAX_ARTIFACT_READ_BYTES,
        };
        loop {
            let chunk: ArtifactChunk =
                client.call("artifacts/read", &request, &mut on_notification)?;
            let count = self.decode(chunk, request.limit)?;
            self.write_chunk(count, output)?;
            if self.after == self.expected.content_bytes {
                return self.verify();
            }
            request.after = self.after;
        }
    }

    fn decode(&mut self, chunk: ArtifactChunk, limit: u32) -> Result<usize, Error> {
        let count = validate_chunk(&chunk, self.after, self.expected.content_bytes, limit)?;
        let decoded = STANDARD
            .decode_slice(chunk.data_base64, &mut self.buffer)
            .map_err(|_| Error::InvalidArtifactChunk)?;
        if decoded != count {
            return Err(Error::InvalidArtifactChunk);
        }
        Ok(decoded)
    }

    fn write_chunk(&mut self, count: usize, output: &mut impl Write) -> Result<(), Error> {
        let bytes = &self.buffer[..count];
        output.write_all(bytes).map_err(Error::ArtifactOutput)?;
        self.sha256.update(bytes);
        self.after += count as u64;
        Ok(())
    }

    fn verify(self) -> Result<(), Error> {
        if !format!("{:x}", self.sha256.finalize()).eq_ignore_ascii_case(&self.expected.sha256) {
            return Err(Error::ArtifactDigestMismatch);
        }
        Ok(())
    }
}

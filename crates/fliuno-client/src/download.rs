//! Stream immutable artifact bytes with a caller-supplied length and digest.
use crate::{Client, Error, Notification};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use fliuno_protocol::{
    ArtifactChunk, ArtifactDigest, ArtifactDigestBatch, ArtifactDigestBatchParams,
    ArtifactDigestEntry, ArtifactDownloadStart, ArtifactGetParams, ArtifactReadParams,
    MAX_ARTIFACT_READ_BYTES,
};
use sha2::{Digest, Sha256};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};

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
        let (digest, first_chunk) = if self
            .info()
            .capabilities
            .get("artifactDownloadStart")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            let start: ArtifactDownloadStart =
                self.call("artifacts/openDownload", artifact, &mut on_notification)?;
            (start.digest, Some(start.first_chunk))
        } else {
            (self.artifact_digest(artifact, &mut on_notification)?, None)
        };
        let mut download = Download::new(&digest)?;
        let first = first_chunk
            .map(|chunk| download.decode(chunk))
            .transpose()?;
        record_digest(&digest).map_err(Error::ArtifactCheckpoint)?;
        if let Some(count) = first {
            download.write_chunk(count, output)?;
            if download.after == digest.content_bytes {
                download.verify()?;
                return Ok(digest);
            }
        }
        download.receive(self, artifact, output, on_notification)?;
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
            let count = self.decode(chunk)?;
            self.write_chunk(count, output)?;
            if self.after == self.expected.content_bytes {
                return self.verify();
            }
            request.after = self.after;
        }
    }

    fn decode(&mut self, chunk: ArtifactChunk) -> Result<usize, Error> {
        let count =
            (self.expected.content_bytes - self.after).min(u64::from(MAX_ARTIFACT_READ_BYTES));
        if chunk.content_bytes != self.expected.content_bytes
            || chunk.offset != self.after
            || chunk.next_offset != self.after + count
            || chunk.data_base64.len() != (count as usize).div_ceil(3) * 4
        {
            return Err(Error::InvalidArtifactChunk);
        }
        let decoded = STANDARD
            .decode_slice(chunk.data_base64, &mut self.buffer)
            .map_err(|_| Error::InvalidArtifactChunk)?;
        if decoded != count as usize {
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

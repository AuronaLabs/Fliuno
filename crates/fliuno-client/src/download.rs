//! Stream immutable artifact bytes with a caller-supplied length and digest.
use crate::{Client, Error, Notification};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use fliuno_protocol::{
    ArtifactChunk, ArtifactDigest, ArtifactGetParams, ArtifactReadParams, MAX_ARTIFACT_READ_BYTES,
};
use sha2::{Digest, Sha256};
use std::io::{BufRead, Write};

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

    /// Stream the artifact into `output` and verify its final SHA-256.
    /// Use a staging writer if a partial or unverified download must not be
    /// visible at its destination. The caller owns publication and flushing.
    pub fn download_artifact(
        &mut self,
        artifact: &ArtifactGetParams,
        expected: &ArtifactDigest,
        output: &mut impl Write,
        mut on_notification: impl FnMut(Notification),
    ) -> Result<(), Error> {
        if expected.sha256.len() != 64 || !expected.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::InvalidArtifactDigest);
        }
        let mut request = ArtifactReadParams {
            task_id: artifact.task_id.clone(),
            artifact_id: artifact.artifact_id.clone(),
            after: 0,
            limit: MAX_ARTIFACT_READ_BYTES,
        };
        let mut sha256 = Sha256::new();
        let buffer_bytes = expected.content_bytes.min(u64::from(request.limit)) as usize;
        // The decoder's slice size estimate rounds up to a full Base64 group.
        let mut buffer = vec![0; buffer_bytes.div_ceil(3) * 3];
        loop {
            let chunk: ArtifactChunk =
                self.call("artifacts/read", &request, &mut on_notification)?;
            let count = (expected.content_bytes - request.after).min(u64::from(request.limit));
            let end = request.after + count;
            if chunk.content_bytes != expected.content_bytes
                || chunk.offset != request.after
                || chunk.next_offset != end
                || chunk.data_base64.len() != (count as usize).div_ceil(3) * 4
            {
                return Err(Error::InvalidArtifactChunk);
            }
            let decoded = STANDARD
                .decode_slice(chunk.data_base64, &mut buffer)
                .map_err(|_| Error::InvalidArtifactChunk)?;
            if decoded != count as usize {
                return Err(Error::InvalidArtifactChunk);
            }
            let bytes = &buffer[..decoded];
            output.write_all(bytes).map_err(Error::ArtifactOutput)?;
            sha256.update(bytes);
            if end == expected.content_bytes {
                break;
            }
            request.after = end;
        }
        if !format!("{:x}", sha256.finalize()).eq_ignore_ascii_case(&expected.sha256) {
            return Err(Error::ArtifactDigestMismatch);
        }
        Ok(())
    }
}

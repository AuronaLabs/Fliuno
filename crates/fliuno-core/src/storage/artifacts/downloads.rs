//! Open several download prefixes in one bounded read transaction.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::{HashMap, hash_map::Entry};

impl Kernel {
    pub fn open_artifact_downloads(
        &self,
        request: &ArtifactDownloadBatchParams,
    ) -> Result<ArtifactDownloadBatch> {
        let count = request.artifacts.len();
        if !(1..=MAX_ARTIFACT_DOWNLOAD_BATCH).contains(&count) {
            return Err(Error::Invalid(format!(
                "artifacts must contain 1 to {MAX_ARTIFACT_DOWNLOAD_BATCH} entries"
            )));
        }
        if !(1..=MAX_ARTIFACT_READ_BYTES).contains(&request.limit)
            || count * request.limit as usize > MAX_ARTIFACT_DOWNLOAD_BATCH_BYTES
        {
            return Err(Error::Invalid(format!(
                "limit must be 1..{MAX_ARTIFACT_READ_BYTES} with at most {MAX_ARTIFACT_DOWNLOAD_BATCH_BYTES} bytes across all entries"
            )));
        }

        let tx = self.db.unchecked_transaction()?;
        let digests = self.lookup_artifact_digests(&request.artifacts)?;
        let mut seen = HashMap::new();
        let mut items = Vec::with_capacity(count);
        for (artifact, digest) in request.artifacts.iter().zip(digests) {
            let item = match digest {
                ArtifactDigestEntry::Ready { digest } => {
                    let key = (&artifact.task_id, &artifact.artifact_id);
                    let download = match seen.entry(key) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => {
                            let (length, data) = self.read_artifact_chunk(
                                &artifact.task_id,
                                &artifact.artifact_id,
                                0,
                                request.limit,
                            )?;
                            if length != digest.content_bytes {
                                return Err(rusqlite::Error::InvalidQuery.into());
                            }
                            entry.insert(ArtifactDownloadStart {
                                first_chunk: ArtifactChunk {
                                    offset: 0,
                                    next_offset: data.len() as u64,
                                    content_bytes: length,
                                    data_base64: STANDARD.encode(data),
                                },
                                digest,
                            })
                        }
                    };
                    ArtifactDownloadBatchEntry::Ready {
                        download: download.clone(),
                    }
                }
                ArtifactDigestEntry::Unrecorded { content_bytes } => {
                    ArtifactDownloadBatchEntry::Unrecorded { content_bytes }
                }
                ArtifactDigestEntry::Missing => ArtifactDownloadBatchEntry::Missing,
            };
            items.push(item);
        }
        tx.commit()?;
        Ok(ArtifactDownloadBatch { items })
    }
}

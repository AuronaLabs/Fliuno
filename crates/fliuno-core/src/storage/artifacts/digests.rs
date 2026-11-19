//! Bounded metadata lookups; never reads blob pages or backfills checksums.
use super::*;

impl Kernel {
    pub fn get_artifact_digests(
        &self,
        request: &ArtifactDigestBatchParams,
    ) -> Result<ArtifactDigestBatch> {
        if !(1..=MAX_ARTIFACT_DIGEST_BATCH).contains(&request.artifacts.len()) {
            return Err(Error::Invalid(format!(
                "artifacts must contain 1 to {MAX_ARTIFACT_DIGEST_BATCH} entries"
            )));
        }
        let valid_id = |id: &str| {
            (1..=MAX_ARTIFACT_DIGEST_ID_BYTES).contains(&id.len())
                && id.bytes().all(|byte| byte.is_ascii_graphic())
        };
        if request
            .artifacts
            .iter()
            .any(|target| !valid_id(&target.task_id) || !valid_id(&target.artifact_id))
        {
            return Err(Error::Invalid(format!(
                "task and artifact IDs must be 1 to {MAX_ARTIFACT_DIGEST_ID_BYTES} visible ASCII bytes"
            )));
        }

        // Drive the indexed lookups from the bounded array. It supplies the
        // result slots even for absent IDs, without a per-request SQL shape.
        let mut statement = self.db.prepare_cached(
            "SELECT requested.key, a.id IS NOT NULL,
                json_extract(a.data,'$.contentBytes'),json_extract(a.data,'$.sha256')
             FROM json_each(?1) AS requested
             LEFT JOIN artifacts AS a ON a.id=requested.value ->> '$.artifactId'
                AND a.task_id=requested.value ->> '$.taskId'",
        )?;
        let targets = serde_json::to_string(&request.artifacts)?;
        let mut rows = statement.query([targets])?;
        let mut items = vec![ArtifactDigestEntry::Missing; request.artifacts.len()];
        while let Some(row) = rows.next()? {
            if !row.get::<_, bool>(1)? {
                continue;
            }
            let index = usize::from(row.get::<_, u8>(0)?);
            let content_bytes =
                u64::try_from(row.get::<_, i64>(2)?).map_err(|_| rusqlite::Error::InvalidQuery)?;
            items[index] = match row.get::<_, Option<String>>(3)? {
                Some(sha256) => ArtifactDigestEntry::Ready {
                    digest: ArtifactDigest {
                        content_bytes,
                        sha256,
                    },
                },
                None => ArtifactDigestEntry::Unrecorded { content_bytes },
            };
        }
        Ok(ArtifactDigestBatch { items })
    }
}

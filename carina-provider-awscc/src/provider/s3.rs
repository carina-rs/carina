//! S3-specific operations.
//!
//! Handles S3 bucket operations that go beyond the Cloud Control API,
//! such as emptying a bucket before deletion (force_delete).

use carina_core::provider::{ProviderError, ProviderResult};

use super::AwsccProvider;

impl AwsccProvider {
    /// Create an S3 client from the stored config
    pub(crate) fn s3_client(&self) -> aws_sdk_s3::Client {
        aws_sdk_s3::Client::new(&self.aws_config)
    }

    /// Empty an S3 bucket by deleting all objects and versions
    pub(crate) async fn empty_s3_bucket(&self, bucket_name: &str) -> ProviderResult<()> {
        let s3 = self.s3_client();

        // Delete all object versions (handles versioned and non-versioned buckets)
        let mut key_marker: Option<String> = None;
        let mut version_id_marker: Option<String> = None;

        loop {
            let mut req = s3.list_object_versions().bucket(bucket_name).max_keys(1000);
            if let Some(ref km) = key_marker {
                req = req.key_marker(km);
            }
            if let Some(ref vim) = version_id_marker {
                req = req.version_id_marker(vim);
            }

            let response = req
                .send()
                .await
                .map_err(|e| ProviderError::new("Failed to list object versions").with_cause(e))?;

            let mut objects_to_delete = Vec::new();

            // Collect versions
            for version in response.versions() {
                if let Some(key) = version.key() {
                    let mut id = aws_sdk_s3::types::ObjectIdentifier::builder().key(key);
                    if let Some(vid) = version.version_id() {
                        id = id.version_id(vid);
                    }
                    objects_to_delete.push(id.build().map_err(|e| {
                        ProviderError::new("Failed to build ObjectIdentifier").with_cause(e)
                    })?);
                }
            }

            // Collect delete markers
            for marker in response.delete_markers() {
                if let Some(key) = marker.key() {
                    let mut id = aws_sdk_s3::types::ObjectIdentifier::builder().key(key);
                    if let Some(vid) = marker.version_id() {
                        id = id.version_id(vid);
                    }
                    objects_to_delete.push(id.build().map_err(|e| {
                        ProviderError::new("Failed to build ObjectIdentifier").with_cause(e)
                    })?);
                }
            }

            // Batch delete (max 1000 per request)
            if !objects_to_delete.is_empty() {
                let delete = aws_sdk_s3::types::Delete::builder()
                    .set_objects(Some(objects_to_delete))
                    .quiet(true)
                    .build()
                    .map_err(|e| {
                        ProviderError::new("Failed to build Delete request").with_cause(e)
                    })?;

                s3.delete_objects()
                    .bucket(bucket_name)
                    .delete(delete)
                    .send()
                    .await
                    .map_err(|e| ProviderError::new("Failed to delete objects").with_cause(e))?;
            }

            if response.is_truncated() == Some(true) {
                key_marker = response.next_key_marker().map(|s| s.to_string());
                version_id_marker = response.next_version_id_marker().map(|s| s.to_string());
            } else {
                break;
            }
        }

        Ok(())
    }
}

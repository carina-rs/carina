use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::utils::{convert_enum_value, extract_enum_value};

use crate::AwsProvider;

impl AwsProvider {
    /// Read an S3 bucket
    pub(crate) async fn read_s3_bucket(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let Some(name) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        match self.s3_client.head_bucket().bucket(name).send().await {
            Ok(_) => {
                let mut attributes = HashMap::new();
                attributes.insert("bucket".to_string(), Value::String(name.to_string()));

                // Get versioning status
                self.read_s3_bucket_versioning(id, name, &mut attributes)
                    .await?;

                // Get object ownership
                self.read_s3_bucket_ownership_controls(id, name, &mut attributes)
                    .await?;

                // Get Object Lock status
                self.read_s3_bucket_object_lock(id, name, &mut attributes)
                    .await?;

                // Get ACL
                self.read_s3_bucket_acl(id, name, &mut attributes).await?;

                // Get tags
                self.read_s3_bucket_tags(id, name, &mut attributes).await?;

                // S3 bucket identifier is the bucket name
                Ok(State::existing(id.clone(), attributes).with_identifier(name))
            }
            Err(err) => {
                // Handle bucket not found
                use aws_sdk_s3::error::SdkError;

                let error_kind = match &err {
                    SdkError::ServiceError(service_err) => {
                        let status = service_err.raw().status().as_u16();
                        classify_head_bucket_status(status, service_err.err().is_not_found())
                    }
                    _ => HeadBucketErrorKind::Other,
                };

                match error_kind {
                    HeadBucketErrorKind::NotFound => Ok(State::not_found(id.clone())),
                    HeadBucketErrorKind::AccessDenied => Err(ProviderError::new(format!(
                        "Access denied for bucket '{}'. This may indicate insufficient IAM \
                         permissions or the bucket is owned by a different AWS account.",
                        name
                    ))
                    .for_resource(id.clone())),
                    HeadBucketErrorKind::Other => Err(ProviderError::new("Failed to read bucket")
                        .with_cause(err)
                        .for_resource(id.clone())),
                }
            }
        }
    }

    /// Create an S3 bucket
    pub(crate) async fn create_s3_bucket(&self, resource: Resource) -> ProviderResult<State> {
        let bucket_name = match resource.get_attr("bucket") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("Bucket name is required").for_resource(resource.id.clone())
                );
            }
        };

        // Get region (use Provider's region if not specified)
        let region = match resource.get_attr("region") {
            Some(Value::String(s)) => {
                // Convert from aws.Region.ap_northeast_1 format to ap-northeast-1 format
                convert_enum_value(s)
            }
            _ => self.region.clone(),
        };

        // Create bucket
        let mut req = self.s3_client.create_bucket().bucket(&bucket_name);

        // Specify LocationConstraint for regions other than us-east-1
        if region != "us-east-1" {
            use aws_sdk_s3::types::{BucketLocationConstraint, CreateBucketConfiguration};
            let constraint = BucketLocationConstraint::from(region.as_str());
            let config = CreateBucketConfiguration::builder()
                .location_constraint(constraint)
                .build();
            req = req.create_bucket_configuration(config);
        }

        // Set ObjectLockEnabledForBucket on create
        if let Some(Value::Bool(val)) = resource.get_attr("object_lock_enabled_for_bucket") {
            req = req.object_lock_enabled_for_bucket(*val);
        }

        // Set ObjectOwnership on create
        if let Some(Value::String(val)) = resource.get_attr("object_ownership") {
            use aws_sdk_s3::types::ObjectOwnership;
            let normalized = extract_enum_value(val);
            req = req.object_ownership(ObjectOwnership::from(normalized));
        }

        // Set ACL on create (convert_enum_value converts underscores back to hyphens)
        if let Some(Value::String(val)) = resource.get_attr("acl") {
            use aws_sdk_s3::types::BucketCannedAcl;
            let normalized = convert_enum_value(val);
            req = req.acl(BucketCannedAcl::from(normalized.as_str()));
        }
        if let Some(Value::String(val)) = resource.get_attr("grant_full_control") {
            req = req.grant_full_control(val);
        }
        if let Some(Value::String(val)) = resource.get_attr("grant_read") {
            req = req.grant_read(val);
        }
        if let Some(Value::String(val)) = resource.get_attr("grant_read_acp") {
            req = req.grant_read_acp(val);
        }
        if let Some(Value::String(val)) = resource.get_attr("grant_write") {
            req = req.grant_write(val);
        }
        if let Some(Value::String(val)) = resource.get_attr("grant_write_acp") {
            req = req.grant_write_acp(val);
        }

        req.send().await.map_err(|e| {
            ProviderError::new("Failed to create bucket")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        // Configure versioning
        let attrs = resource.resolved_attributes();
        self.write_s3_bucket_versioning(&resource.id, &bucket_name, &attrs)
            .await?;

        // Set tags
        self.write_s3_bucket_tags(&resource.id, &bucket_name, &attrs)
            .await?;

        // Return state after creation
        self.read_s3_bucket(&resource.id, Some(&bucket_name)).await
    }

    /// Update an S3 bucket
    pub(crate) async fn update_s3_bucket(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        let bucket_name = identifier.to_string();

        // Update versioning status
        let attrs = to.resolved_attributes();
        self.write_s3_bucket_versioning(&id, &bucket_name, &attrs)
            .await?;

        // Update object ownership
        self.write_s3_bucket_ownership_controls(&id, &bucket_name, &attrs)
            .await?;

        // Update ACL
        self.write_s3_bucket_acl(&id, &bucket_name, &attrs).await?;

        // Update tags: if tags were removed entirely, delete all tags
        if from.attributes.contains_key("tags") && !to.attributes.contains_key("tags") {
            self.s3_client
                .delete_bucket_tagging()
                .bucket(&bucket_name)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to delete bucket tags")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        } else {
            self.write_s3_bucket_tags(&id, &bucket_name, &attrs).await?;
        }

        self.read_s3_bucket(&id, Some(&bucket_name)).await
    }

    /// Delete an S3 bucket, honoring lifecycle.force_delete
    pub(crate) async fn delete_s3_bucket(
        &self,
        id: ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> ProviderResult<()> {
        // If force_delete is enabled, empty the bucket before deletion
        if lifecycle.force_delete {
            self.empty_s3_bucket(&id, identifier).await?;
        }

        self.s3_client
            .delete_bucket()
            .bucket(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete bucket")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }

    /// Empty an S3 bucket by deleting all objects and versions
    async fn empty_s3_bucket(&self, id: &ResourceId, bucket_name: &str) -> ProviderResult<()> {
        let mut key_marker: Option<String> = None;
        let mut version_id_marker: Option<String> = None;

        loop {
            let mut req = self
                .s3_client
                .list_object_versions()
                .bucket(bucket_name)
                .max_keys(1000);
            if let Some(ref km) = key_marker {
                req = req.key_marker(km);
            }
            if let Some(ref vim) = version_id_marker {
                req = req.version_id_marker(vim);
            }

            let response = req.send().await.map_err(|e| {
                ProviderError::new("Failed to list object versions")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

            let mut objects_to_delete = Vec::new();

            // Collect versions
            for version in response.versions() {
                if let Some(key) = version.key() {
                    let mut oid = aws_sdk_s3::types::ObjectIdentifier::builder().key(key);
                    if let Some(vid) = version.version_id() {
                        oid = oid.version_id(vid);
                    }
                    objects_to_delete.push(oid.build().map_err(|e| {
                        ProviderError::new("Failed to build ObjectIdentifier")
                            .with_cause(e)
                            .for_resource(id.clone())
                    })?);
                }
            }

            // Collect delete markers
            for marker in response.delete_markers() {
                if let Some(key) = marker.key() {
                    let mut oid = aws_sdk_s3::types::ObjectIdentifier::builder().key(key);
                    if let Some(vid) = marker.version_id() {
                        oid = oid.version_id(vid);
                    }
                    objects_to_delete.push(oid.build().map_err(|e| {
                        ProviderError::new("Failed to build ObjectIdentifier")
                            .with_cause(e)
                            .for_resource(id.clone())
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
                        ProviderError::new("Failed to build Delete request")
                            .with_cause(e)
                            .for_resource(id.clone())
                    })?;

                self.s3_client
                    .delete_objects()
                    .bucket(bucket_name)
                    .delete(delete)
                    .send()
                    .await
                    .map_err(|e| {
                        ProviderError::new("Failed to delete objects")
                            .with_cause(e)
                            .for_resource(id.clone())
                    })?;
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

    /// Read S3 bucket ownership controls
    async fn read_s3_bucket_ownership_controls(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &mut HashMap<String, Value>,
    ) -> ProviderResult<()> {
        match self
            .s3_client
            .get_bucket_ownership_controls()
            .bucket(identifier)
            .send()
            .await
        {
            Ok(output) => {
                if let Some(controls) = output.ownership_controls()
                    && let Some(rule) = controls.rules().first()
                {
                    let value = rule.object_ownership().as_str().to_string();
                    attributes.insert("object_ownership".to_string(), Value::String(value));
                }
            }
            Err(err) => {
                if !is_s3_not_configured_error(&err, "OwnershipControlsNotFoundError") {
                    return Err(
                        ProviderError::new("Failed to read bucket ownership controls")
                            .with_cause(err)
                            .for_resource(id.clone()),
                    );
                }
            }
        }
        Ok(())
    }

    /// Write S3 bucket ownership controls
    async fn write_s3_bucket_ownership_controls(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &HashMap<String, Value>,
    ) -> ProviderResult<()> {
        if let Some(Value::String(val)) = attributes.get("object_ownership") {
            use aws_sdk_s3::types::{ObjectOwnership, OwnershipControls, OwnershipControlsRule};
            let normalized = extract_enum_value(val);
            let rule = OwnershipControlsRule::builder()
                .object_ownership(ObjectOwnership::from(normalized))
                .build()
                .map_err(|e| {
                    ProviderError::new("Failed to build ownership controls rule")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
            let controls = OwnershipControls::builder()
                .rules(rule)
                .build()
                .map_err(|e| {
                    ProviderError::new("Failed to build ownership controls")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
            self.s3_client
                .put_bucket_ownership_controls()
                .bucket(identifier)
                .ownership_controls(controls)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to put bucket ownership controls")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }
        Ok(())
    }

    /// Read S3 bucket Object Lock status
    async fn read_s3_bucket_object_lock(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &mut HashMap<String, Value>,
    ) -> ProviderResult<()> {
        match self
            .s3_client
            .get_object_lock_configuration()
            .bucket(identifier)
            .send()
            .await
        {
            Ok(output) => {
                let enabled = output
                    .object_lock_configuration()
                    .and_then(|config| config.object_lock_enabled())
                    .is_some();
                attributes.insert(
                    "object_lock_enabled_for_bucket".to_string(),
                    Value::Bool(enabled),
                );
            }
            Err(err) => {
                if is_s3_not_configured_error(&err, "ObjectLockConfigurationNotFoundError") {
                    attributes.insert(
                        "object_lock_enabled_for_bucket".to_string(),
                        Value::Bool(false),
                    );
                } else {
                    return Err(
                        ProviderError::new("Failed to read object lock configuration")
                            .with_cause(err)
                            .for_resource(id.clone()),
                    );
                }
            }
        }
        Ok(())
    }

    /// Read S3 bucket ACL
    async fn read_s3_bucket_acl(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &mut HashMap<String, Value>,
    ) -> ProviderResult<()> {
        let output = self
            .s3_client
            .get_bucket_acl()
            .bucket(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to read bucket ACL")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        let owner_id = output
            .owner()
            .and_then(|o| o.id())
            .unwrap_or("")
            .to_string();

        let grants = output.grants();

        // Classify grants by permission, collecting grantee strings
        let mut full_control: Vec<String> = Vec::new();
        let mut read: Vec<String> = Vec::new();
        let mut read_acp: Vec<String> = Vec::new();
        let mut write: Vec<String> = Vec::new();
        let mut write_acp: Vec<String> = Vec::new();

        for grant in grants {
            let Some(grantee) = grant.grantee() else {
                continue;
            };
            let Some(permission) = grant.permission() else {
                continue;
            };

            // Build grantee string in header format
            let grantee_str = if let Some(uri) = grantee.uri() {
                format!("uri=\"{}\"", uri)
            } else if let Some(id) = grantee.id() {
                format!("id=\"{}\"", id)
            } else if let Some(email) = grantee.email_address() {
                format!("emailAddress=\"{}\"", email)
            } else {
                continue;
            };

            // Skip owner's FULL_CONTROL (it's implicit)
            let is_owner = grantee.id().is_some_and(|id| id == owner_id);

            use aws_sdk_s3::types::Permission;
            match permission {
                Permission::FullControl => {
                    if !is_owner {
                        full_control.push(grantee_str);
                    }
                }
                Permission::Read => read.push(grantee_str),
                Permission::ReadAcp => read_acp.push(grantee_str),
                Permission::Write => write.push(grantee_str),
                Permission::WriteAcp => write_acp.push(grantee_str),
                _ => {}
            }
        }

        // Try to infer canned ACL
        let canned_acl = infer_canned_acl(&full_control, &read, &read_acp, &write, &write_acp);

        if let Some(acl) = canned_acl {
            // When a canned ACL is inferred, only set `acl` — the grant fields
            // are the expansion of the canned ACL and would cause false diffs.
            attributes.insert("acl".to_string(), Value::String(acl.to_string()));
        } else {
            // No canned ACL matched — set individual grant fields
            if !full_control.is_empty() {
                attributes.insert(
                    "grant_full_control".to_string(),
                    Value::String(full_control.join(", ")),
                );
            }
            if !read.is_empty() {
                attributes.insert("grant_read".to_string(), Value::String(read.join(", ")));
            }
            if !read_acp.is_empty() {
                attributes.insert(
                    "grant_read_acp".to_string(),
                    Value::String(read_acp.join(", ")),
                );
            }
            if !write.is_empty() {
                attributes.insert("grant_write".to_string(), Value::String(write.join(", ")));
            }
            if !write_acp.is_empty() {
                attributes.insert(
                    "grant_write_acp".to_string(),
                    Value::String(write_acp.join(", ")),
                );
            }
        }
        Ok(())
    }

    /// Write S3 bucket ACL
    async fn write_s3_bucket_acl(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &HashMap<String, Value>,
    ) -> ProviderResult<()> {
        let acl = extract_string_attr(attributes, "acl");
        let grant_full_control = extract_string_attr(attributes, "grant_full_control");
        let grant_read = extract_string_attr(attributes, "grant_read");
        let grant_read_acp = extract_string_attr(attributes, "grant_read_acp");
        let grant_write = extract_string_attr(attributes, "grant_write");
        let grant_write_acp = extract_string_attr(attributes, "grant_write_acp");

        let has_acl = acl.is_some()
            || grant_full_control.is_some()
            || grant_read.is_some()
            || grant_read_acp.is_some()
            || grant_write.is_some()
            || grant_write_acp.is_some();

        if !has_acl {
            return Ok(());
        }

        use aws_sdk_s3::types::BucketCannedAcl;
        let mut req = self.s3_client.put_bucket_acl().bucket(identifier);

        if let Some(val) = acl {
            let normalized = convert_enum_value(val);
            req = req.acl(BucketCannedAcl::from(normalized.as_str()));
        }
        if let Some(val) = grant_full_control {
            req = req.grant_full_control(val);
        }
        if let Some(val) = grant_read {
            req = req.grant_read(val);
        }
        if let Some(val) = grant_read_acp {
            req = req.grant_read_acp(val);
        }
        if let Some(val) = grant_write {
            req = req.grant_write(val);
        }
        if let Some(val) = grant_write_acp {
            req = req.grant_write_acp(val);
        }

        req.send().await.map_err(|e| {
            ProviderError::new("Failed to put bucket ACL")
                .with_cause(e)
                .for_resource(id.clone())
        })?;

        Ok(())
    }

    /// Read S3 bucket tags
    async fn read_s3_bucket_tags(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &mut HashMap<String, Value>,
    ) -> ProviderResult<()> {
        match self
            .s3_client
            .get_bucket_tagging()
            .bucket(identifier)
            .send()
            .await
        {
            Ok(output) => {
                let mut tag_map = HashMap::new();
                for tag in output.tag_set() {
                    tag_map.insert(
                        tag.key().to_string(),
                        Value::String(tag.value().to_string()),
                    );
                }
                if !tag_map.is_empty() {
                    attributes.insert("tags".to_string(), Value::Map(tag_map));
                }
            }
            Err(err) => {
                if !is_s3_not_configured_error(&err, "NoSuchTagSet") {
                    return Err(ProviderError::new("Failed to read bucket tagging")
                        .with_cause(err)
                        .for_resource(id.clone()));
                }
            }
        }
        Ok(())
    }

    /// Write S3 bucket tags
    pub(crate) async fn write_s3_bucket_tags(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &HashMap<String, Value>,
    ) -> ProviderResult<()> {
        if let Some(Value::Map(tag_map)) = attributes.get("tags") {
            use aws_sdk_s3::types::{Tag, Tagging};
            let tags: Vec<Tag> = tag_map
                .iter()
                .filter_map(|(k, v)| {
                    if let Value::String(val) = v {
                        Some(Tag::builder().key(k).value(val).build().ok()?)
                    } else {
                        None
                    }
                })
                .collect();

            let tagging = Tagging::builder()
                .set_tag_set(Some(tags))
                .build()
                .map_err(|e| {
                    ProviderError::new("Failed to build tagging")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;

            self.s3_client
                .put_bucket_tagging()
                .bucket(identifier)
                .tagging(tagging)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to put bucket tags")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }

        Ok(())
    }
}

/// Result of classifying an S3 HeadBucket error.
#[derive(Debug, PartialEq)]
enum HeadBucketErrorKind {
    /// Bucket does not exist (301 redirect to wrong region, or 404 not found).
    NotFound,
    /// Access denied (403) - could be permissions issue or bucket owned by another account.
    AccessDenied,
    /// Other error that should be propagated.
    Other,
}

/// Classify an HTTP status code from a HeadBucket error.
///
/// This is extracted as a pure function to enable unit testing.
fn classify_head_bucket_status(status: u16, is_not_found_error: bool) -> HeadBucketErrorKind {
    if is_not_found_error || status == 301 || status == 404 {
        HeadBucketErrorKind::NotFound
    } else if status == 403 {
        HeadBucketErrorKind::AccessDenied
    } else {
        HeadBucketErrorKind::Other
    }
}

/// Check if an S3 SDK error is a "not configured" error that should be silently ignored.
fn is_s3_not_configured_error<E: aws_sdk_s3::error::ProvideErrorMetadata>(
    err: &aws_sdk_s3::error::SdkError<E>,
    expected_code: &str,
) -> bool {
    use aws_sdk_s3::error::SdkError;
    match err {
        SdkError::ServiceError(service_err) => service_err.err().code() == Some(expected_code),
        _ => false,
    }
}

fn extract_string_attr<'a>(attributes: &'a HashMap<String, Value>, key: &str) -> Option<&'a str> {
    match attributes.get(key) {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

const ALL_USERS_URI: &str = "http://acs.amazonaws.com/groups/global/AllUsers";
const AUTH_USERS_URI: &str = "http://acs.amazonaws.com/groups/global/AuthenticatedUsers";

/// Infer a canned ACL from the grant lists.
/// Returns None if the grants don't match any known canned ACL pattern.
pub(crate) fn infer_canned_acl(
    full_control: &[String],
    read: &[String],
    read_acp: &[String],
    write: &[String],
    write_acp: &[String],
) -> Option<&'static str> {
    // AllUsers URI is used for both READ and WRITE permission checks
    let all_users = format!("uri=\"{}\"", ALL_USERS_URI);
    let auth_users_read = format!("uri=\"{}\"", AUTH_USERS_URI);

    // private: no non-owner grants
    if full_control.is_empty()
        && read.is_empty()
        && read_acp.is_empty()
        && write.is_empty()
        && write_acp.is_empty()
    {
        return Some("private");
    }

    // public-read: AllUsers READ, nothing else
    if full_control.is_empty()
        && read.len() == 1
        && read[0] == all_users
        && read_acp.is_empty()
        && write.is_empty()
        && write_acp.is_empty()
    {
        return Some("public-read");
    }

    // public-read-write: AllUsers READ + WRITE, nothing else
    if full_control.is_empty()
        && read.len() == 1
        && read[0] == all_users
        && read_acp.is_empty()
        && write.len() == 1
        && write[0] == all_users
        && write_acp.is_empty()
    {
        return Some("public-read-write");
    }

    // authenticated-read: AuthenticatedUsers READ, nothing else
    if full_control.is_empty()
        && read.len() == 1
        && read[0] == auth_users_read
        && read_acp.is_empty()
        && write.is_empty()
        && write_acp.is_empty()
    {
        return Some("authenticated-read");
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_canned_acl_private() {
        let result = infer_canned_acl(&[], &[], &[], &[], &[]);
        assert_eq!(result, Some("private"));
    }

    #[test]
    fn test_infer_canned_acl_public_read() {
        let all_users = format!("uri=\"{}\"", ALL_USERS_URI);
        let result = infer_canned_acl(&[], &[all_users], &[], &[], &[]);
        assert_eq!(result, Some("public-read"));
    }

    #[test]
    fn test_infer_canned_acl_public_read_write() {
        let all_users_for_read = format!("uri=\"{}\"", ALL_USERS_URI);
        let all_users_for_write = format!("uri=\"{}\"", ALL_USERS_URI);
        let result = infer_canned_acl(&[], &[all_users_for_read], &[], &[all_users_for_write], &[]);
        assert_eq!(result, Some("public-read-write"));
    }

    #[test]
    fn test_infer_canned_acl_authenticated_read() {
        let auth_users_read = format!("uri=\"{}\"", AUTH_USERS_URI);
        let result = infer_canned_acl(&[], &[auth_users_read], &[], &[], &[]);
        assert_eq!(result, Some("authenticated-read"));
    }

    #[test]
    fn test_infer_canned_acl_unknown() {
        let custom = vec!["id=\"abc123\"".to_string()];
        let result = infer_canned_acl(&custom, &[], &[], &[], &[]);
        assert_eq!(result, None);
    }

    #[test]
    fn test_classify_head_bucket_status_403_is_access_denied() {
        // 403 should be classified as AccessDenied, not NotFound
        assert_eq!(
            classify_head_bucket_status(403, false),
            HeadBucketErrorKind::AccessDenied
        );
    }

    #[test]
    fn test_classify_head_bucket_status_404_is_not_found() {
        assert_eq!(
            classify_head_bucket_status(404, false),
            HeadBucketErrorKind::NotFound
        );
    }

    #[test]
    fn test_classify_head_bucket_status_301_is_not_found() {
        assert_eq!(
            classify_head_bucket_status(301, false),
            HeadBucketErrorKind::NotFound
        );
    }

    #[test]
    fn test_classify_head_bucket_status_sdk_not_found_error() {
        // When the SDK itself reports is_not_found, it should be NotFound
        assert_eq!(
            classify_head_bucket_status(400, true),
            HeadBucketErrorKind::NotFound
        );
    }

    #[test]
    fn test_classify_head_bucket_status_other() {
        assert_eq!(
            classify_head_bucket_status(500, false),
            HeadBucketErrorKind::Other
        );
    }
}

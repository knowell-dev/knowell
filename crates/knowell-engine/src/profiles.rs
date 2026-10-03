//! Organization-scoped reads of persisted embedding-profile metadata.
//!
//! Catalogue reads use the same current caller grants and token scopes as
//! engine tools. They neither inspect configured providers nor construct an
//! embedder, register sources or alter profiles and index generations.

use knowell_auth::{Action, Resource};
use knowell_core::Name;
use knowell_mcp::{Caller, Timestamp, ToolError};
use knowell_store::ProfileId;
use knowell_store::embeddings::{self, EmbeddingProfile};
use serde::Serialize;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::engine::Engine;
use crate::error::store_tool;

/// Persisted metadata of an immutable profile in the engine's organization.
///
/// These fields describe registration only, without implying that a provider
/// is configured, an index is ready or any view currently uses the profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileMetadata {
    /// Profile UUID, independent of its name.
    pub id: Uuid,
    /// Profile name, unique within the organization.
    pub name: Name,
    /// Stored provider identifier; no credential reference or resolved value.
    pub provider: String,
    /// Stored model identifier.
    pub model: String,
    /// Number of vector dimensions.
    pub dimensions: u32,
    /// Stored prepared-input format version.
    pub input_format_version: String,
    /// Registration time as RFC 3339 UTC, retaining stored fractional seconds.
    pub created_at: Timestamp,
}

/// An exact profile name or UUID; UUID-shaped names remain names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileSelector {
    /// Name within the engine's organization.
    Name(Name),
    /// UUID; profiles belonging to another organization are not returned.
    Id(Uuid),
}

fn metadata(profile: EmbeddingProfile) -> Result<ProfileMetadata, ToolError> {
    let created_at = profile
        .created_at
        .checked_to_offset(UtcOffset::UTC)
        .ok_or_else(|| ToolError::internal("stored profile registration time is invalid"))?
        .format(&Rfc3339)
        .map_err(|_| ToolError::internal("stored profile registration time is invalid"))?;
    let created_at = Timestamp::new(created_at)
        .map_err(|_| ToolError::internal("stored profile registration time is invalid"))?;
    Ok(ProfileMetadata {
        id: profile.id.0,
        name: profile.name,
        provider: profile.provider,
        model: profile.model,
        dimensions: profile.dimensions,
        input_format_version: profile.input_format_version,
        created_at,
    })
}

impl Engine {
    async fn require_profile_read(&self, caller: &Caller) -> Result<(), ToolError> {
        let access = self.inner.access.resolve(caller).await?;
        if !access.allows(Action::ReadCode, &Resource::Organization) {
            return Err(ToolError::permission_denied(
                "organization code read permission is required to inspect embedding profiles",
            ));
        }
        Ok(())
    }

    /// Lists the organization's stored profiles in bytewise name order.
    ///
    /// Requires current organization-wide `ReadCode` permission, including
    /// the caller's token scopes, before looking up any profile. Workspace
    /// and project grants alone do not authorize this catalogue. The empty
    /// catalogue is returned as an empty list; no profile is substituted.
    /// No provider, source, index-status or activity lookup is performed.
    pub async fn list_embedding_profiles(
        &self,
        caller: &Caller,
    ) -> Result<Vec<ProfileMetadata>, ToolError> {
        self.require_profile_read(caller).await?;
        let mut conn = self.inner.store.acquire().await.map_err(store_tool)?;
        embeddings::list_profiles(&mut conn, self.inner.organization)
            .await
            .map_err(store_tool)?
            .into_iter()
            .map(metadata)
            .collect()
    }

    /// Gets one exact stored profile after organization-wide authorization.
    ///
    /// The selector is never inferred or substituted. Missing names, missing
    /// UUIDs and UUIDs in another organization all return `None`. Current
    /// grants and token scopes are resolved before the store is queried.
    /// Reading the metadata does not contact a provider or change any index.
    pub async fn get_embedding_profile(
        &self,
        caller: &Caller,
        selector: ProfileSelector,
    ) -> Result<Option<ProfileMetadata>, ToolError> {
        self.require_profile_read(caller).await?;
        let mut conn = self.inner.store.acquire().await.map_err(store_tool)?;
        let profile = match selector {
            ProfileSelector::Name(name) => {
                embeddings::find_profile(&mut conn, self.inner.organization, &name).await
            }
            ProfileSelector::Id(id) => {
                embeddings::get_profile_in_organization(
                    &mut conn,
                    self.inner.organization,
                    ProfileId(id),
                )
                .await
            }
        }
        .map_err(store_tool)?;
        profile
            .filter(|profile| profile.organization == self.inner.organization)
            .map(metadata)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use knowell_store::OrganizationId;
    use time::macros::datetime;

    use super::*;

    fn profile() -> EmbeddingProfile {
        EmbeddingProfile {
            id: ProfileId(Uuid::nil()),
            organization: OrganizationId(Uuid::nil()),
            name: Name::new("synthetic-profile").expect("fixture name"),
            provider: "synthetic-provider".into(),
            model: "synthetic-model".into(),
            dimensions: 768,
            input_format_version: "synthetic-v1".into(),
            created_at: datetime!(2026-10-03 12:30:45.123456789 +03:00),
        }
    }

    #[test]
    fn registration_time_keeps_fractional_precision_and_normalizes_to_utc() {
        let result = metadata(profile()).expect("fixture metadata");
        assert_eq!(result.created_at.as_str(), "2026-10-03T09:30:45.123456789Z");
        let json = serde_json::to_value(result).expect("fixture serialization");
        assert_eq!(json["created_at"], "2026-10-03T09:30:45.123456789Z");
    }

    #[test]
    fn unrepresentable_registration_time_is_an_explicit_safe_error() {
        let mut profile = profile();
        profile.created_at = profile.created_at.replace_year(-1).expect("fixture year");
        let error = metadata(profile).expect_err("negative year is not RFC 3339");
        assert_eq!(error.kind(), "internal");
        assert!(!error.client_message("synthetic-request").contains("-0001"));
    }
}

//! App deployment workspace RPC methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployError {
    UrlConflict,
    UrlModeration,
    IdempotencyConflict,
    NotFound,
    PermissionDenied,
    DeploymentNotInBuildingState,
    UnsupportedProjectType,
    ProviderUnavailable,
    /// The user already owns the maximum number of projects (apps); distinct
    /// from the generic `ResourceExhausted` so clients can tell "too many
    /// apps" apart from "deploying too fast".
    ProjectLimitExceeded,
    /// The user exceeded the per-minute deploy rate limit; retry after the
    /// window passes. Distinct from the generic `ResourceExhausted` so clients
    /// can render the retry hint.
    RateLimited,
    ArchiveTooLarge,
    Internal,
    Unauthenticated,
    InvalidArgument,
    ResourceExhausted,
    DeadlineExceeded,
    AlreadyExists,
    FailedPrecondition,
}
impl DeployError {
    /// Every kind, for exhaustive iteration in tests.
    pub const ALL: [DeployError; 18] = [
        Self::UrlConflict,
        Self::UrlModeration,
        Self::IdempotencyConflict,
        Self::NotFound,
        Self::PermissionDenied,
        Self::DeploymentNotInBuildingState,
        Self::UnsupportedProjectType,
        Self::ProviderUnavailable,
        Self::ProjectLimitExceeded,
        Self::RateLimited,
        Self::ArchiveTooLarge,
        Self::Internal,
        Self::Unauthenticated,
        Self::InvalidArgument,
        Self::ResourceExhausted,
        Self::DeadlineExceeded,
        Self::AlreadyExists,
        Self::FailedPrecondition,
    ];
    /// The `RpcError.code` discriminant carried on the workspace RPC envelope.
    pub fn wire_code(self) -> &'static str {
        match self {
            Self::UrlConflict => "deploy_url_conflict",
            Self::UrlModeration => "deploy_url_moderation",
            Self::IdempotencyConflict => "deploy_idempotency_conflict",
            Self::NotFound => "deploy_not_found",
            Self::PermissionDenied => "deploy_permission_denied",
            Self::DeploymentNotInBuildingState => "deploy_not_in_building_state",
            Self::UnsupportedProjectType => "deploy_unsupported_project_type",
            Self::ProviderUnavailable => "deploy_provider_unavailable",
            Self::ProjectLimitExceeded => "deploy_project_limit_exceeded",
            Self::RateLimited => "deploy_rate_limited",
            Self::ArchiveTooLarge => "deploy_archive_too_large",
            Self::Internal => "deploy_internal",
            Self::Unauthenticated => "deploy_unauthenticated",
            Self::InvalidArgument => "deploy_invalid_argument",
            Self::ResourceExhausted => "deploy_resource_exhausted",
            Self::DeadlineExceeded => "deploy_deadline_exceeded",
            Self::AlreadyExists => "deploy_already_exists",
            Self::FailedPrecondition => "deploy_failed_precondition",
        }
    }
    /// Parse a `RpcError.code` discriminant back into a kind, or `None` when the
    /// code is not a deploy error code.
    pub fn from_wire_code(code: &str) -> Option<Self> {
        Some(match code {
            "deploy_url_conflict" => Self::UrlConflict,
            "deploy_url_moderation" => Self::UrlModeration,
            "deploy_idempotency_conflict" => Self::IdempotencyConflict,
            "deploy_not_found" => Self::NotFound,
            "deploy_permission_denied" => Self::PermissionDenied,
            "deploy_not_in_building_state" => Self::DeploymentNotInBuildingState,
            "deploy_unsupported_project_type" => Self::UnsupportedProjectType,
            "deploy_provider_unavailable" => Self::ProviderUnavailable,
            "deploy_project_limit_exceeded" => Self::ProjectLimitExceeded,
            "deploy_rate_limited" => Self::RateLimited,
            "deploy_archive_too_large" => Self::ArchiveTooLarge,
            "deploy_internal" => Self::Internal,
            "deploy_unauthenticated" => Self::Unauthenticated,
            "deploy_invalid_argument" => Self::InvalidArgument,
            "deploy_resource_exhausted" => Self::ResourceExhausted,
            "deploy_deadline_exceeded" => Self::DeadlineExceeded,
            "deploy_already_exists" => Self::AlreadyExists,
            "deploy_failed_precondition" => Self::FailedPrecondition,
            _ => return None,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::DeployError;
    #[test]
    fn deploy_error_kind_wire_code_round_trips() {
        for kind in DeployError::ALL {
            assert_eq!(
                DeployError::from_wire_code(kind.wire_code()),
                Some(kind),
                "round-trip failed for {kind:?}"
            );
        }
    }
    #[test]
    fn deploy_error_kind_rejects_unknown_code() {
        assert_eq!(DeployError::from_wire_code("hub_error"), None);
        assert_eq!(DeployError::from_wire_code(""), None);
    }
}

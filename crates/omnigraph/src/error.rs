use thiserror::Error;

pub use omnigraph_storage::{StorageFailure, StorageFailureKind};

pub type Result<T> = std::result::Result<T, OmniError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestErrorKind {
    BadRequest,
    NotFound,
    Conflict,
    Internal,
}

/// Structured details for a manifest-level conflict. Set on the `details`
/// field of `ManifestError` when callers need to match on the specific
/// concurrency-control failure rather than parse a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestConflictDetails {
    /// A caller-supplied per-table expected version did not match the
    /// manifest's current latest non-tombstoned version for that table.
    ExpectedVersionMismatch {
        table_key: String,
        expected: u64,
        actual: u64,
    },
    /// A logical authority value captured during write preparation changed
    /// before the manifest visibility decision. Unlike a touched-table
    /// version mismatch, this may name a read-only dependency such as the
    /// target branch's graph head or schema identity.
    ReadSetChanged {
        member: String,
        expected: Option<String>,
        actual: Option<String>,
    },
    /// Lance's row-level CAS rejected the publish because a concurrent writer
    /// landed a row with the same `object_id`. Distinct from
    /// `ExpectedVersionMismatch`: the caller's expectations (if any) still
    /// hold against the new manifest state, so the publisher will retry.
    RowLevelCasContention,
}

#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct ManifestError {
    pub kind: ManifestErrorKind,
    pub message: String,
    pub details: Option<ManifestConflictDetails>,
}

impl ManifestError {
    pub fn new(kind: ManifestErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: ManifestConflictDetails) -> Self {
        self.details = Some(details);
        self
    }
}

#[derive(Debug, Clone)]
pub struct MergeConflict {
    pub table_key: String,
    pub row_id: Option<String>,
    pub kind: MergeConflictKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeConflictKind {
    DivergentInsert,
    DivergentUpdate,
    DeleteVsUpdate,
    OrphanEdge,
    UniqueViolation,
    CardinalityViolation,
    ValueConstraintViolation,
}

#[derive(Debug, Error)]
pub enum OmniError {
    #[error("{0}")]
    Compiler(#[from] omnigraph_compiler::error::CompilerError),
    #[error("{0}")]
    Storage(StorageFailure),
    /// Lance rejected a stale transaction as semantically retryable. Kept
    /// typed at the storage boundary so RFC-023 can distinguish an
    /// effect-free key fence from an arbitrary I/O or execution failure
    /// without parsing upstream error text.
    #[error("retryable storage commit conflict: {0}")]
    RetryableCommitConflict(String),
    #[error("query: {0}")]
    DataFusion(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Manifest(ManifestError),
    #[error("merge conflicts: {0:?}")]
    MergeConflicts(Vec<MergeConflict>),
    /// A strict keyed insert found that the logical row id already exists in
    /// the pinned table image, or lost a concurrent exact-id insertion race
    /// before any effect from this attempt became visible.  This is distinct
    /// from a stale read set: retrying the same strict insert must not silently
    /// turn it into an upsert.
    #[error("key conflict in table '{table_key}'")]
    KeyConflict {
        table_key: String,
        /// Exact id observed by pinned preflight or the required fresh probe
        /// after an effect-free substrate conflict. Optional on the wire only
        /// for backward compatibility with older producers.
        key: Option<String>,
    },
    /// A write was rejected before recovery was armed because its bounded
    /// physical plan would exceed an explicit safety ceiling. This is a
    /// retryable input-shaping error, not a partial-success signal.
    #[error("resource limit exceeded for {resource}: actual {actual}, limit {limit}")]
    ResourceLimitExceeded {
        resource: String,
        limit: u64,
        actual: u64,
    },
    /// A caller attempted to admit an external Blob URI that is malformed or
    /// outside this graph handle's immutable base allowlist. The URI must be a
    /// normalized, credential-free spelling (or a redacted placeholder): this
    /// error crosses HTTP/CLI boundaries and must never echo URI credentials.
    #[error("external blob URI '{uri}' is not allowed: {reason}")]
    ExternalBlobPolicy { uri: String, reason: String },
    /// An allowed external Blob source could not be probed or read before the
    /// write's first durable effect. Kept distinct from input-policy failures
    /// so transports can report a dependency failure instead of an opaque 500
    /// or a misleading malformed-request response.
    #[error("external blob source '{uri}' is unavailable: {reason}")]
    ExternalBlobSource { uri: String, reason: String },
    /// Persisted table or Blob state contradicted the logical Blob contract.
    /// This is a typed integrity failure rather than a generic storage string so
    /// callers never reinterpret corrupt identity, metadata, or descriptors as
    /// null or ordinary absence.
    #[error("blob integrity violation: {reason}")]
    BlobIntegrity { reason: String },
    /// A managed Blob range used reversed or out-of-bounds coordinates.
    #[error("blob range [{start}, {end}) is not satisfiable for a value of length {length}")]
    BlobRangeNotSatisfiable { start: u64, end: u64, length: u64 },
    /// A durable recovery intent overlaps this write. Its physical effects may
    /// already have landed, or it may still be armed before its first effect;
    /// either way the sidecar named by `operation_id` must be resolved before
    /// the caller retries. Treating this as ordinary OCC would let a writer
    /// advance around unresolved commit ownership.
    #[error("recovery required for operation {operation_id}: {reason}")]
    RecoveryRequired {
        operation_id: String,
        reason: String,
    },
    /// A caller-supplied write precondition named a branch head commit that
    /// is no longer (or never was) the branch's current head. The write had
    /// no effect. Distinct from `ReadSetChanged`: that is the engine's own
    /// authority check and may be reprepared, while this is the caller's
    /// compare-and-swap token, so it is terminal — retrying against a newer
    /// head would silently discard the condition the caller asked for.
    /// `actual` is `None` on a branch with no commits.
    #[error(
        "precondition failed on branch '{branch}': expected head '{expected}' but current is {}",
        actual.as_deref().unwrap_or("<absent>")
    )]
    PreconditionFailed {
        branch: String,
        expected: String,
        actual: Option<String>,
    },
    /// Engine-layer policy enforcement (MR-722). Wraps either a policy
    /// denial ("you can't do that") or a policy-evaluation failure
    /// ("the policy engine itself blew up"). The HTTP layer maps
    /// denials to 403 and evaluation failures to 500; CLI and embedded
    /// callers can match on this variant directly.
    #[error("policy: {0}")]
    Policy(String),
    /// `Omnigraph::init` was called against a URI that already holds a
    /// manifest or schema artifacts from a previous init. Strict mode (the
    /// default) fails fast with this error before touching disk so an existing
    /// graph's metadata cannot be overwritten or destroyed.
    /// `InitOptions { force: true }` is limited to orphan schema artifacts at
    /// a root with no manifest; it never overwrites an initialized graph.
    #[error(
        "graph already initialized or initialization metadata exists at '{uri}'; --force may replace only orphan schema files after proving that no __manifest exists"
    )]
    AlreadyInitialized { uri: String },
    /// The authoritative `__manifest` Create commit completed, but a later
    /// read-back or validation step failed. The schema artifacts are retained:
    /// deleting them would strand the committed graph behind a missing
    /// contract. Callers may inspect the typed source, but must not interpret
    /// this outcome as proof that an ordinary open will succeed.
    #[error(
        "graph initialization at '{uri}' committed its manifest, but finalization failed; schema artifacts were preserved; inspect or open the graph before taking further action: {source}"
    )]
    InitializationCommitted {
        uri: String,
        #[source]
        source: Box<OmniError>,
    },
    /// Physical graph initialization returned an error and the follow-up exact
    /// genesis probe failed, so the engine cannot prove which table or
    /// manifest Creates committed. Cleanup and retry are unsafe until an
    /// operator has inspected the root. Both typed causes are retained because
    /// they describe different failure boundaries.
    #[error(
        "graph initialization at '{uri}' has an indeterminate physical outcome; schema artifacts and '__init_claim.json' were preserved; do not retry initialization or delete the root until it is inspected (create error: {source}; exact-genesis probe error: {probe})"
    )]
    InitializationIndeterminate {
        uri: String,
        #[source]
        source: Box<OmniError>,
        probe: Box<OmniError>,
    },
    /// A durable initialization-ownership claim already exists. It may belong
    /// to a live initializer or be residue from a stopped attempt, so another
    /// initialization attempt must not overwrite or remove it speculatively.
    #[error(
        "graph initialization at '{uri}' is claimed by '__init_claim.json'; another initializer may still be running or a prior initializer may have stopped; quiesce all initializers before manually removing the claim, then retry init (use --force only when orphan schema files remain)"
    )]
    InitializationClaimed { uri: String },
}

impl From<omnigraph_storage::StorageError> for OmniError {
    fn from(error: omnigraph_storage::StorageError) -> Self {
        match error {
            omnigraph_storage::StorageError::Internal(message) => Self::manifest_internal(message),
            omnigraph_storage::StorageError::Backend(failure) => Self::Storage(failure),
            omnigraph_storage::StorageError::ResourceLimit {
                resource,
                limit,
                actual,
                ..
            } => Self::ResourceLimitExceeded {
                resource,
                limit,
                actual,
            },
        }
    }
}

impl OmniError {
    /// Convert a Lance failure at a graph-storage boundary. This is named
    /// instead of a blanket `From` implementation so every call site must
    /// choose storage, domain, or engine-internal semantics.
    pub fn storage(error: lance::Error) -> Self {
        let kind = classify_lance_error(&error);
        Self::Storage(StorageFailure::new(kind, format!("storage: {error}")))
    }

    /// Convert a Lance failure while retaining the operation's historical
    /// context. The resulting message is already complete.
    pub fn storage_context(context: impl std::fmt::Display, error: lance::Error) -> Self {
        let kind = classify_lance_error(&error);
        Self::Storage(StorageFailure::new(
            kind,
            format!("storage: {context}: {error}"),
        ))
    }

    /// Classify an engine-owned Namespace condition without first wrapping it
    /// in Lance's location-bearing error. This retains the exact historical
    /// `storage: <Namespace error>` diagnostic at those call sites.
    pub(crate) fn storage_namespace(error: lance_namespace::NamespaceError) -> Self {
        let kind = classify_namespace_code(error.code());
        Self::Storage(StorageFailure::new(kind, format!("storage: {error}")))
    }

    pub fn storage_failure(&self) -> Option<&StorageFailure> {
        match self {
            Self::Storage(failure) => Some(failure),
            _ => None,
        }
    }

    /// Arrow failures at manifest/batch machinery are engine shape or
    /// computation failures, not storage conditions.
    pub(crate) fn arrow_internal(error: arrow_schema::ArrowError) -> Self {
        Self::manifest_internal(error.to_string())
    }

    /// Preserve typed storage evidence carried through DataFusion execution;
    /// otherwise retain the user query/execution category.
    pub(crate) fn datafusion(error: datafusion::error::DataFusionError) -> Self {
        match find_lance_source_kind(&error, 0) {
            Some(kind) => Self::Storage(StorageFailure::new(kind, format!("storage: {error}"))),
            None => Self::DataFusion(error.to_string()),
        }
    }

    /// Add operation context without discarding an existing typed category.
    pub(crate) fn with_context(self, context: impl std::fmt::Display) -> Self {
        match self {
            Self::Storage(mut failure) => {
                failure.message = match failure.message.strip_prefix("storage: ") {
                    Some(message) => format!("storage: {context}: {message}"),
                    None => format!("{context}: {}", failure.message),
                };
                Self::Storage(failure)
            }
            Self::Manifest(mut error) => {
                error.message = format!("{context}: {}", error.message);
                Self::Manifest(error)
            }
            other => other,
        }
    }

    pub fn key_conflict(table_key: impl Into<String>, key: impl Into<String>) -> Self {
        Self::KeyConflict {
            table_key: table_key.into(),
            key: Some(key.into()),
        }
    }

    pub(crate) fn resource_limit(resource: impl Into<String>, limit: u64, actual: u64) -> Self {
        Self::ResourceLimitExceeded {
            resource: resource.into(),
            limit,
            actual,
        }
    }

    pub(crate) fn external_blob_policy(uri: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::ExternalBlobPolicy {
            uri: uri.into(),
            reason: reason.into(),
        }
    }

    pub(crate) fn external_blob_source(uri: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::ExternalBlobSource {
            uri: uri.into(),
            reason: reason.into(),
        }
    }

    pub(crate) fn blob_integrity(reason: impl Into<String>) -> Self {
        Self::BlobIntegrity {
            reason: reason.into(),
        }
    }

    pub(crate) fn is_retryable_commit_conflict(&self) -> bool {
        matches!(self, Self::RetryableCommitConflict(_))
    }

    pub(crate) fn is_read_set_changed(&self) -> bool {
        matches!(
            self,
            Self::Manifest(ManifestError {
                details: Some(ManifestConflictDetails::ReadSetChanged { .. }),
                ..
            })
        )
    }

    pub fn manifest(message: impl Into<String>) -> Self {
        Self::Manifest(ManifestError::new(ManifestErrorKind::BadRequest, message))
    }

    pub fn manifest_not_found(message: impl Into<String>) -> Self {
        Self::Manifest(ManifestError::new(ManifestErrorKind::NotFound, message))
    }

    pub fn manifest_conflict(message: impl Into<String>) -> Self {
        Self::Manifest(ManifestError::new(ManifestErrorKind::Conflict, message))
    }

    pub fn manifest_internal(message: impl Into<String>) -> Self {
        Self::Manifest(ManifestError::new(ManifestErrorKind::Internal, message))
    }

    pub fn manifest_expected_version_mismatch(
        table_key: impl Into<String>,
        expected: u64,
        actual: u64,
    ) -> Self {
        let table_key = table_key.into();
        let message = format!(
            "stale view of '{}': expected manifest table version {} but current is {} — refresh and retry",
            table_key, expected, actual
        );
        Self::Manifest(
            ManifestError::new(ManifestErrorKind::Conflict, message).with_details(
                ManifestConflictDetails::ExpectedVersionMismatch {
                    table_key,
                    expected,
                    actual,
                },
            ),
        )
    }

    pub fn manifest_row_level_cas_contention(message: impl Into<String>) -> Self {
        Self::Manifest(
            ManifestError::new(ManifestErrorKind::Conflict, message)
                .with_details(ManifestConflictDetails::RowLevelCasContention),
        )
    }

    pub fn manifest_read_set_changed(
        member: impl Into<String>,
        expected: Option<String>,
        actual: Option<String>,
    ) -> Self {
        let member = member.into();
        let message = format!(
            "write authority '{}' changed during preparation (expected {}, current {}) — reprepare from the current branch state",
            member,
            expected.as_deref().unwrap_or("<absent>"),
            actual.as_deref().unwrap_or("<absent>"),
        );
        Self::Manifest(
            ManifestError::new(ManifestErrorKind::Conflict, message).with_details(
                ManifestConflictDetails::ReadSetChanged {
                    member,
                    expected,
                    actual,
                },
            ),
        )
    }

    pub fn precondition_failed(
        branch: impl Into<String>,
        expected: impl Into<String>,
        actual: Option<String>,
    ) -> Self {
        Self::PreconditionFailed {
            branch: branch.into(),
            expected: expected.into(),
            actual,
        }
    }

    pub fn recovery_required(operation_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::RecoveryRequired {
            operation_id: operation_id.into(),
            reason: reason.into(),
        }
    }
}

fn classify_lance_error(error: &lance::Error) -> StorageFailureKind {
    classify_lance_error_at_depth(error, 0)
}

fn classify_lance_error_at_depth(error: &lance::Error, depth: usize) -> StorageFailureKind {
    if depth >= omnigraph_storage::MAX_STORAGE_SOURCE_DEPTH {
        return StorageFailureKind::Unknown;
    }
    match error {
        lance::Error::Timeout { .. } => StorageFailureKind::Transient,
        lance::Error::DiskCapExceeded { .. }
        | lance::Error::InvalidInput { .. }
        | lance::Error::InvalidTableLocation { .. }
        | lance::Error::InvalidRef { .. }
        | lance::Error::NotSupported { .. }
        | lance::Error::FieldNotFound { .. }
        | lance::Error::Unprocessable { .. } => StorageFailureKind::Configuration,
        lance::Error::DatasetNotFound { .. }
        | lance::Error::NotFound { .. }
        | lance::Error::RefNotFound { .. }
        | lance::Error::VersionNotFound { .. }
        | lance::Error::IndexNotFound { .. } => StorageFailureKind::NotFound,
        lance::Error::DatasetAlreadyExists { .. }
        | lance::Error::CommitConflict { .. }
        | lance::Error::IncompatibleTransaction { .. }
        | lance::Error::RetryableCommitConflict { .. }
        | lance::Error::TooMuchWriteContention { .. }
        | lance::Error::RefConflict { .. }
        | lance::Error::VersionConflict { .. }
        | lance::Error::Fenced { .. } => StorageFailureKind::Precondition,
        lance::Error::CorruptFile { .. }
        | lance::Error::SchemaMismatch { .. }
        | lance::Error::Internal { .. }
        | lance::Error::Arrow { .. }
        | lance::Error::Schema { .. } => StorageFailureKind::Permanent,
        lance::Error::Execution { .. }
        | lance::Error::Index { .. }
        | lance::Error::Cleanup { .. }
        | lance::Error::Cloned { .. }
        | lance::Error::PrerequisiteFailed { .. }
        | lance::Error::Stop => StorageFailureKind::Unknown,
        lance::Error::IO { source, .. } | lance::Error::External { source } => {
            classify_lance_source_at_depth(source.as_ref(), depth + 1)
        }
        lance::Error::Wrapped { error, .. } => {
            classify_lance_source_at_depth(error.as_ref(), depth + 1)
        }
        lance::Error::Namespace { source, .. } => source
            .downcast_ref::<lance_namespace::NamespaceError>()
            .map(|error| classify_namespace_code(error.code()))
            .unwrap_or_else(|| classify_lance_source_at_depth(source.as_ref(), depth + 1)),
    }
}

fn classify_lance_source_at_depth(
    source: &(dyn std::error::Error + 'static),
    depth: usize,
) -> StorageFailureKind {
    if depth >= omnigraph_storage::MAX_STORAGE_SOURCE_DEPTH {
        return StorageFailureKind::Unknown;
    }
    if let Some(error) = source.downcast_ref::<lance::Error>() {
        return classify_lance_error_at_depth(error, depth);
    }
    if let Some(error) = source.downcast_ref::<object_store::Error>() {
        return omnigraph_storage::classify_object_store_error_at_depth(error, depth);
    }
    if let Some(error) = source.downcast_ref::<std::io::Error>() {
        return omnigraph_storage::classify_io_error_at_depth(error, depth);
    }
    source
        .source()
        .map(|inner| classify_lance_source_at_depth(inner, depth + 1))
        .unwrap_or(StorageFailureKind::Unknown)
}

fn find_lance_source_kind(
    source: &(dyn std::error::Error + 'static),
    depth: usize,
) -> Option<StorageFailureKind> {
    if depth >= omnigraph_storage::MAX_STORAGE_SOURCE_DEPTH {
        return None;
    }
    if let Some(error) = source.downcast_ref::<lance::Error>() {
        return Some(classify_lance_error_at_depth(error, depth));
    }
    if let Some(error) = source.downcast_ref::<object_store::Error>() {
        return Some(omnigraph_storage::classify_object_store_error_at_depth(
            error, depth,
        ));
    }
    if let Some(error) = source.downcast_ref::<std::io::Error>() {
        return Some(omnigraph_storage::classify_io_error_at_depth(error, depth));
    }
    source
        .source()
        .and_then(|inner| find_lance_source_kind(inner, depth + 1))
}

fn classify_namespace_code(code: lance_namespace::ErrorCode) -> StorageFailureKind {
    use lance_namespace::ErrorCode;

    match code {
        ErrorCode::ServiceUnavailable | ErrorCode::Throttling => StorageFailureKind::Transient,
        ErrorCode::NamespaceNotFound
        | ErrorCode::TableNotFound
        | ErrorCode::TableIndexNotFound
        | ErrorCode::TableTagNotFound
        | ErrorCode::TransactionNotFound
        | ErrorCode::TableVersionNotFound
        | ErrorCode::TableColumnNotFound
        | ErrorCode::TableBranchNotFound => StorageFailureKind::NotFound,
        ErrorCode::Unsupported
        | ErrorCode::InvalidInput
        | ErrorCode::PermissionDenied
        | ErrorCode::Unauthenticated
        | ErrorCode::TableSchemaValidationError => StorageFailureKind::Configuration,
        ErrorCode::NamespaceAlreadyExists
        | ErrorCode::TableAlreadyExists
        | ErrorCode::TableIndexAlreadyExists
        | ErrorCode::TableTagAlreadyExists
        | ErrorCode::TableBranchAlreadyExists
        | ErrorCode::ConcurrentModification
        | ErrorCode::NamespaceNotEmpty
        | ErrorCode::InvalidTableState => StorageFailureKind::Precondition,
        ErrorCode::Internal => StorageFailureKind::Permanent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_lance_kind(error: lance::Error, expected: StorageFailureKind) {
        assert_eq!(classify_lance_error(&error), expected, "{error}");
    }

    #[test]
    fn lance_variant_families_are_exhaustively_classified() {
        use lance::Error;

        assert_lance_kind(Error::timeout("timeout"), StorageFailureKind::Transient);

        for error in [
            Error::disk_cap_exceeded(1, 2),
            Error::invalid_input("invalid"),
            Error::InvalidTableLocation {
                message: "invalid location".to_string(),
            },
            Error::InvalidRef {
                message: "invalid ref".to_string(),
            },
            Error::not_supported("unsupported"),
            Error::field_not_found("field", vec!["other".to_string()]),
            Error::unprocessable("unprocessable"),
        ] {
            assert_lance_kind(error, StorageFailureKind::Configuration);
        }

        for error in [
            Error::dataset_not_found("dataset", Box::new(std::io::Error::other("missing"))),
            Error::not_found("object"),
            Error::RefNotFound {
                message: "missing ref".to_string(),
            },
            Error::VersionNotFound {
                message: "missing version".to_string(),
            },
            Error::index_not_found("index"),
        ] {
            assert_lance_kind(error, StorageFailureKind::NotFound);
        }

        for error in [
            Error::dataset_already_exists("dataset"),
            Error::commit_conflict_source(1, Box::new(std::io::Error::other("conflict"))),
            Error::incompatible_transaction_source(Box::new(std::io::Error::other("incompatible"))),
            Error::retryable_commit_conflict_source(1, Box::new(std::io::Error::other("conflict"))),
            Error::too_much_write_contention("contention"),
            Error::RefConflict {
                message: "ref conflict".to_string(),
            },
            Error::version_conflict("version conflict", 1, 0),
            Error::fenced_by_peer("fenced"),
        ] {
            assert_lance_kind(error, StorageFailureKind::Precondition);
        }

        for error in [
            Error::corrupt_file_named("file", "corrupt"),
            Error::schema_mismatch("mismatch"),
            Error::internal("internal"),
            Error::arrow("arrow"),
            Error::schema("schema"),
        ] {
            assert_lance_kind(error, StorageFailureKind::Permanent);
        }

        for error in [
            Error::execution("execution"),
            Error::index("index"),
            Error::Cleanup {
                message: "cleanup".to_string(),
            },
            Error::cloned("cloned"),
            Error::prerequisite_failed("prerequisite"),
            Error::Stop,
        ] {
            assert_lance_kind(error, StorageFailureKind::Unknown);
        }
    }

    #[test]
    fn lance_opaque_wrappers_recover_only_typed_source_evidence() {
        let timeout = || {
            Box::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"))
                as Box<dyn std::error::Error + Send + Sync>
        };
        assert_lance_kind(
            lance::Error::io_source(timeout()),
            StorageFailureKind::Transient,
        );
        assert_lance_kind(
            lance::Error::wrapped(timeout()),
            StorageFailureKind::Transient,
        );
        assert_lance_kind(
            lance::Error::external(timeout()),
            StorageFailureKind::Transient,
        );
        assert_lance_kind(
            lance::Error::io_source(Box::new(std::fmt::Error)),
            StorageFailureKind::Unknown,
        );
    }

    #[test]
    fn all_lance_namespace_codes_have_the_rfc_mapping() {
        use lance_namespace::ErrorCode;

        let expected = [
            StorageFailureKind::Configuration,
            StorageFailureKind::NotFound,
            StorageFailureKind::Precondition,
            StorageFailureKind::Precondition,
            StorageFailureKind::NotFound,
            StorageFailureKind::Precondition,
            StorageFailureKind::NotFound,
            StorageFailureKind::Precondition,
            StorageFailureKind::NotFound,
            StorageFailureKind::Precondition,
            StorageFailureKind::NotFound,
            StorageFailureKind::NotFound,
            StorageFailureKind::NotFound,
            StorageFailureKind::Configuration,
            StorageFailureKind::Precondition,
            StorageFailureKind::Configuration,
            StorageFailureKind::Configuration,
            StorageFailureKind::Transient,
            StorageFailureKind::Permanent,
            StorageFailureKind::Precondition,
            StorageFailureKind::Configuration,
            StorageFailureKind::Transient,
            StorageFailureKind::NotFound,
            StorageFailureKind::Precondition,
        ];

        for (raw, expected) in (0_u32..=23).zip(expected) {
            let code = ErrorCode::from_u32(raw).expect("all Lance 10 codes must exist");
            assert_eq!(classify_namespace_code(code), expected, "{code}");
            let namespace = lance_namespace::NamespaceError::from_code(raw, "typed namespace");
            let historical_message = format!("storage: {namespace}");
            let classified = OmniError::storage_namespace(namespace);
            assert_eq!(classified.to_string(), historical_message);
            assert_eq!(
                classified.storage_failure().map(|failure| failure.kind),
                Some(expected)
            );
            let namespace = lance_namespace::NamespaceError::from_code(raw, "typed namespace");
            let lance: lance::Error = namespace.into();
            assert_lance_kind(lance, expected);
        }
    }

    #[test]
    fn direct_and_contextual_lance_messages_are_complete_and_exact() {
        let direct = lance::Error::timeout("direct timeout");
        let direct_text = direct.to_string();
        let direct = OmniError::storage(direct);
        assert_eq!(direct.to_string(), format!("storage: {direct_text}"));
        assert_eq!(
            direct.storage_failure().unwrap().message,
            direct.to_string()
        );

        let contextual = lance::Error::timeout("context timeout");
        let contextual_text = contextual.to_string();
        let contextual = OmniError::storage_context("nearest", contextual);
        assert_eq!(
            contextual.to_string(),
            format!("storage: nearest: {contextual_text}")
        );
        assert_eq!(
            contextual.storage_failure().unwrap().kind,
            StorageFailureKind::Transient
        );
    }

    #[test]
    fn datafusion_user_errors_and_nested_storage_errors_remain_distinct() {
        let user = OmniError::datafusion(datafusion::error::DataFusionError::Plan(
            "bad user query".to_string(),
        ));
        assert!(matches!(user, OmniError::DataFusion(_)));

        let nested = lance::Error::io_source(Box::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "transport timeout",
        )));
        let nested = OmniError::datafusion(datafusion::error::DataFusionError::External(Box::new(
            nested,
        )));
        assert_eq!(
            nested.storage_failure().map(|failure| failure.kind),
            Some(StorageFailureKind::Transient)
        );
    }

    #[test]
    fn arrow_and_blob_contradictions_are_not_storage_failures() {
        let arrow = OmniError::arrow_internal(arrow_schema::ArrowError::ComputeError(
            "invalid batch computation".to_string(),
        ));
        assert!(matches!(
            arrow,
            OmniError::Manifest(ManifestError {
                kind: ManifestErrorKind::Internal,
                ..
            })
        ));

        let blob = OmniError::blob_integrity("persisted descriptor contradiction");
        assert!(matches!(blob, OmniError::BlobIntegrity { .. }));
    }
}

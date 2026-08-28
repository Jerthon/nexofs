//! Máquinas de estado de item, operação e conflito. SPEC §12, §13, §14.1, §18.

use serde::{Deserialize, Serialize};

/// SPEC §12.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HydrationState {
    Placeholder,
    DownloadQueued,
    Downloading,
    Hydrated,
    Partial,
    Evicted,
    HydrationError,
}

/// SPEC §12.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PinState {
    OnlineOnly,
    AvailableLocally,
    Pinned,
}

/// SPEC §12.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SyncState {
    Clean,
    Dirty,
    UploadQueued,
    Uploading,
    AwaitingNetwork,
    AwaitingAuthentication,
    Conflict,
    Error,
    DeletedLocally,
}

/// SPEC §13.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OperationState {
    Pending,
    Running,
    WaitingRetry,
    WaitingNetwork,
    WaitingAuthentication,
    BlockedByConflict,
    Completed,
    Cancelled,
    FailedPermanent,
}

/// SPEC §13.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OperationType {
    UploadFile,
    CreateDirectory,
    MoveItem,
    RenameItem,
    DeleteItem,
    RestoreItem,
    HydrateItem,
    PinTree,
    RefreshChanges,
    ReconcileNamespace,
}

/// SPEC §14.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CursorState {
    Uninitialized,
    Valid,
    Refreshing,
    Expired,
    Rebuilding,
    Error,
}

/// SPEC §18.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConflictType {
    ContentChangedBothSides,
    RemoteDeletedLocalModified,
    LocalDeletedRemoteModified,
    RenameCollision,
    MoveCollision,
    CaseCollision,
    LocalOnlyRemoteCollision,
    ParentDeleted,
    UnsupportedName,
}

/// SPEC §18.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConflictResolution {
    KeepLocal,
    KeepRemote,
    KeepBoth,
    SaveLocalElsewhere,
    DismissTemporarily,
}

/// SPEC §17.3 — resultado da avaliação de exclusão para um caminho.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SyncDisposition {
    NormalSync,
    LocalOnly,
    RemotePlaceholder,
    IgnoreChanges,
}

use crate::error::AppError;
use crate::ipc::validate_safe_integer;
use crate::project::model::{
    AssetManifest, MediaClip, ProjectDocument, ProjectProfile, TextItem, Track, Transition,
    MAX_HISTORY_ENTRIES,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use ts_rs::TS;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid_argument(message)
}

fn validate_token(value: &str, field: &str, max_len: usize) -> Result<(), AppError> {
    if value.trim().is_empty()
        || value.len() > max_len
        || value.contains('\r')
        || value.contains('\n')
        || value.as_bytes().contains(&0)
    {
        return Err(invalid(format!("{field} is invalid")));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum EntityKind {
    Document,
    Asset,
    Track,
    Clip,
    TextItem,
    Transition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct EntityRef {
    pub kind: EntityKind,
    pub id: String,
}

impl EntityRef {
    pub fn new(kind: EntityKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    pub fn validate(&self) -> Result<(), AppError> {
        // Entity IDs are validated by ProjectDocument, while this method also
        // protects standalone history records from being empty or multiline.
        validate_token(&self.id, "history.entity.id", 256)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
#[ts(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum EntityState {
    Document {
        name: String,
        profile: ProjectProfile,
    },
    Asset(AssetManifest),
    Track(Track),
    Clip(MediaClip),
    TextItem(TextItem),
    Transition(Transition),
}

impl EntityState {
    pub fn kind(&self) -> EntityKind {
        match self {
            Self::Document { .. } => EntityKind::Document,
            Self::Asset(_) => EntityKind::Asset,
            Self::Track(_) => EntityKind::Track,
            Self::Clip(_) => EntityKind::Clip,
            Self::TextItem(_) => EntityKind::TextItem,
            Self::Transition(_) => EntityKind::Transition,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Document { .. } => "document",
            Self::Asset(value) => &value.id,
            Self::Track(value) => &value.id,
            Self::Clip(value) => &value.id,
            Self::TextItem(value) => &value.id,
            Self::Transition(value) => &value.id,
        }
    }

    fn validate(&self) -> Result<(), AppError> {
        match self {
            Self::Document { name, profile } => {
                validate_token(name, "history.document.name", 1024)?;
                profile.validate()
            }
            Self::Asset(value) => value.validate(),
            Self::Track(value) => value.validate(),
            // Entity-local validation is completed by the document graph. The
            // typed payload still must have the right stable identifier here.
            Self::Clip(value) => value
                .id
                .parse::<uuid::Uuid>()
                .map(|_| ())
                .map_err(|_| invalid("history.clip.id must be a UUID")),
            Self::TextItem(value) => value
                .id
                .parse::<uuid::Uuid>()
                .map(|_| ())
                .map_err(|_| invalid("history.textItem.id must be a UUID")),
            Self::Transition(value) => value.validate_ids(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct EntityDelta {
    pub entity: EntityRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub before: Option<EntityState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub after: Option<EntityState>,
    /// Array positions are part of an affected-entity delta because track and
    /// clip order is meaningful. They are absent for document metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub before_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub after_index: Option<u32>,
}

impl EntityDelta {
    pub fn validate(&self) -> Result<(), AppError> {
        self.entity.validate()?;
        if self.before.is_none() && self.after.is_none() {
            return Err(invalid("A history delta must change an entity"));
        }
        for state in [&self.before, &self.after].into_iter().flatten() {
            if state.kind() != self.entity.kind || state.id() != self.entity.id {
                return Err(invalid("History delta entity and payload do not match"));
            }
            state.validate()?;
        }
        if self.entity.kind == EntityKind::Document {
            if self.before_index.is_some() || self.after_index.is_some() {
                return Err(invalid(
                    "Document metadata deltas cannot have array indexes",
                ));
            }
        } else {
            if self.before.is_some() != self.before_index.is_some()
                || self.after.is_some() != self.after_index.is_some()
            {
                return Err(invalid(
                    "Entity collection deltas need an index for each present state",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct TransactionDelta {
    pub changes: Vec<EntityDelta>,
}

impl TransactionDelta {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn affected_entities(&self) -> Vec<EntityRef> {
        self.changes
            .iter()
            .map(|change| change.entity.clone())
            .collect()
    }

    pub fn validate(&self) -> Result<(), AppError> {
        let mut seen = HashSet::with_capacity(self.changes.len());
        for change in &self.changes {
            change.validate()?;
            let key = (change.entity.kind, change.entity.id.as_str());
            if !seen.insert(key) {
                return Err(invalid("A transaction delta contains a duplicate entity"));
            }
        }
        Ok(())
    }

    /// Build a deterministic affected-entity delta between two revisions.
    pub fn between(before: &ProjectDocument, after: &ProjectDocument) -> Result<Self, AppError> {
        before.diff(after)
    }

    pub fn apply_forward(&self, document: &mut ProjectDocument) -> Result<(), AppError> {
        self.apply(document, true)
    }

    pub fn apply_backward(&self, document: &mut ProjectDocument) -> Result<(), AppError> {
        self.apply(document, false)
    }

    /// Apply one side of this delta atomically. The candidate is validated
    /// before replacing the caller's document, so an invalid or stale replay
    /// cannot partially mutate the active state.
    pub fn apply(&self, document: &mut ProjectDocument, forward: bool) -> Result<(), AppError> {
        self.validate()?;
        document.validate()?;
        let mut candidate = document.clone();
        self.apply_in_place(&mut candidate, forward)?;
        *document = candidate;
        Ok(())
    }

    /// Apply a validated delta to a caller-owned staged document. This is
    /// crate-private because a failure may leave the candidate partially
    /// changed; the store discards that candidate on every error.
    pub(crate) fn apply_validated(
        &self,
        document: &mut ProjectDocument,
        forward: bool,
    ) -> Result<(), AppError> {
        self.validate()?;
        self.apply_in_place(document, forward)
    }

    fn apply_in_place(
        &self,
        document: &mut ProjectDocument,
        forward: bool,
    ) -> Result<(), AppError> {
        let mut metadata = self
            .changes
            .iter()
            .filter(|change| change.entity.kind == EntityKind::Document);
        if let Some(change) = metadata.next() {
            if metadata.next().is_some() {
                return Err(invalid(
                    "A transaction can contain only one document metadata delta",
                ));
            }
            let expected = if forward {
                change.before.as_ref()
            } else {
                change.after.as_ref()
            };
            let target = if forward {
                change.after.as_ref()
            } else {
                change.before.as_ref()
            };
            let current = EntityState::Document {
                name: document.name.clone(),
                profile: document.profile.clone(),
            };
            if Some(&current) != expected {
                return Err(AppError::new(
                    crate::error::ErrorCode::RevisionConflict,
                    "The document metadata no longer matches the history entry",
                ));
            }
            let Some(EntityState::Document { name, profile }) = target else {
                return Err(invalid("Document metadata cannot be deleted"));
            };
            document.name = name.clone();
            document.profile = profile.clone();
        }

        let assets: Vec<_> = self
            .changes
            .iter()
            .filter(|change| change.entity.kind == EntityKind::Asset)
            .collect();
        apply_collection(
            &mut document.assets,
            &assets,
            forward,
            |value| &value.id,
            EntityState::Asset,
        )?;
        let tracks: Vec<_> = self
            .changes
            .iter()
            .filter(|change| change.entity.kind == EntityKind::Track)
            .collect();
        apply_collection(
            &mut document.tracks,
            &tracks,
            forward,
            |value| &value.id,
            EntityState::Track,
        )?;
        let clips: Vec<_> = self
            .changes
            .iter()
            .filter(|change| change.entity.kind == EntityKind::Clip)
            .collect();
        apply_collection(
            &mut document.clips,
            &clips,
            forward,
            |value| &value.id,
            EntityState::Clip,
        )?;
        let text_items: Vec<_> = self
            .changes
            .iter()
            .filter(|change| change.entity.kind == EntityKind::TextItem)
            .collect();
        apply_collection(
            &mut document.text_items,
            &text_items,
            forward,
            |value| &value.id,
            EntityState::TextItem,
        )?;
        let transitions: Vec<_> = self
            .changes
            .iter()
            .filter(|change| change.entity.kind == EntityKind::Transition)
            .collect();
        apply_collection(
            &mut document.transitions,
            &transitions,
            forward,
            |value| &value.id,
            EntityState::Transition,
        )?;

        document.validate()?;
        Ok(())
    }
}

fn apply_collection<T, Id, ToState>(
    values: &mut Vec<T>,
    changes: &[&EntityDelta],
    forward: bool,
    id: Id,
    to_state: ToState,
) -> Result<(), AppError>
where
    T: Clone + PartialEq + FromEntityState,
    Id: Fn(&T) -> &str + Copy,
    ToState: Fn(T) -> EntityState + Copy,
{
    if changes.is_empty() {
        return Ok(());
    }

    let mut by_id: HashMap<&str, (usize, &T)> = HashMap::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        if by_id.insert(id(value), (index, value)).is_some() {
            return Err(invalid("The document contains duplicate entity IDs"));
        }
    }
    for change in changes {
        let expected = if forward {
            change.before.as_ref()
        } else {
            change.after.as_ref()
        };
        match (by_id.get(change.entity.id.as_str()), expected) {
            (Some((_, current)), Some(expected)) => {
                if to_state((*current).clone()) != *expected {
                    return Err(AppError::new(
                        crate::error::ErrorCode::RevisionConflict,
                        "The history entry does not match the current entity",
                    ));
                }
            }
            (None, None) => {}
            (None, Some(_)) => {
                return Err(AppError::new(
                    crate::error::ErrorCode::RevisionConflict,
                    "The history entity is missing",
                ));
            }
            (Some(_), None) => {
                return Err(AppError::new(
                    crate::error::ErrorCode::RevisionConflict,
                    "An unexpected history entity is present",
                ));
            }
        }
    }

    let ids: HashSet<&str> = changes
        .iter()
        .map(|change| change.entity.id.as_str())
        .collect();
    let current_values = std::mem::take(values);
    let mut unchanged = Vec::with_capacity(current_values.len());
    for value in current_values {
        if !ids.contains(id(&value)) {
            unchanged.push(value);
        }
    }

    let mut inserts: Vec<(&EntityDelta, &EntityState, u32)> = changes
        .iter()
        .filter_map(|change| {
            let target = if forward {
                change.after.as_ref()
            } else {
                change.before.as_ref()
            }?;
            let index = if forward {
                change.after_index?
            } else {
                change.before_index?
            };
            Some((*change, target, index))
        })
        .collect();
    inserts.sort_by_key(|(_, _, index)| *index);
    let mut previous_index = None;
    for (_, _, index) in &inserts {
        validate_safe_integer(*index as u64, "history.entity.index")?;
        if previous_index == Some(*index) {
            return Err(invalid("History deltas target the same collection index"));
        }
        previous_index = Some(*index);
    }

    let mut output = Vec::with_capacity(unchanged.len() + inserts.len());
    let mut unchanged = unchanged.into_iter();
    for (change, target, index) in inserts {
        let index = index as usize;
        while output.len() < index {
            let Some(value) = unchanged.next() else {
                return Err(invalid("History delta collection index is out of bounds"));
            };
            output.push(value);
        }
        if output.len() != index {
            return Err(invalid("History delta collection index is out of bounds"));
        }
        output.push(state_to_value(target.clone(), &change.entity.kind)?);
    }
    output.extend(unchanged);
    *values = output;
    Ok(())
}

fn state_to_value<T>(state: EntityState, kind: &EntityKind) -> Result<T, AppError>
where
    T: FromEntityState,
{
    T::from_entity_state(state, *kind)
}

trait FromEntityState: Sized {
    fn from_entity_state(state: EntityState, kind: EntityKind) -> Result<Self, AppError>;
}

macro_rules! entity_state_conversion {
    ($ty:ty, $variant:ident, $kind:expr) => {
        impl FromEntityState for $ty {
            fn from_entity_state(state: EntityState, kind: EntityKind) -> Result<Self, AppError> {
                if kind != $kind {
                    return Err(invalid("History entity kind is inconsistent"));
                }
                match state {
                    EntityState::$variant(value) => Ok(value),
                    _ => Err(invalid("History entity payload has the wrong type")),
                }
            }
        }
    };
}

entity_state_conversion!(AssetManifest, Asset, EntityKind::Asset);
entity_state_conversion!(Track, Track, EntityKind::Track);
entity_state_conversion!(MediaClip, Clip, EntityKind::Clip);
entity_state_conversion!(TextItem, TextItem, EntityKind::TextItem);
entity_state_conversion!(Transition, Transition, EntityKind::Transition);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct EditResult {
    pub transaction_id: String,
    pub label: String,
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    pub changed: bool,
    pub affected_entities: Vec<EntityRef>,
}

impl EditResult {
    pub fn validate(&self) -> Result<(), AppError> {
        validate_token(&self.transaction_id, "result.transactionId", 256)?;
        validate_token(&self.label, "result.label", 1024)?;
        validate_safe_integer(self.revision, "result.revision")?;
        let mut seen = HashSet::with_capacity(self.affected_entities.len());
        for entity in &self.affected_entities {
            entity.validate()?;
            if !seen.insert((entity.kind, entity.id.as_str())) {
                return Err(invalid("Result affected entities must be unique"));
            }
        }
        if !self.changed && !self.affected_entities.is_empty() {
            return Err(invalid(
                "A no-change result cannot report affected entities",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub transaction_id: String,
    pub label: String,
    #[ts(type = "SafeInteger")]
    pub expected_revision: u64,
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    pub payload_hash: String,
    pub delta: TransactionDelta,
    pub result: EditResult,
}

impl HistoryEntry {
    pub fn validate(&self) -> Result<(), AppError> {
        validate_token(&self.transaction_id, "history.transactionId", 256)?;
        validate_token(&self.label, "history.label", 1024)?;
        validate_safe_integer(self.expected_revision, "history.expectedRevision")?;
        validate_safe_integer(self.revision, "history.revision")?;
        if self.revision <= self.expected_revision {
            return Err(invalid("A history entry must advance the revision"));
        }
        validate_token(&self.payload_hash, "history.payloadHash", 256)?;
        self.delta.validate()?;
        self.result.validate()?;
        if self.result.transaction_id != self.transaction_id
            || self.result.label != self.label
            || self.result.revision != self.revision
            || self.result.affected_entities != self.delta.affected_entities()
        {
            return Err(invalid("History entry result does not match its delta"));
        }
        if self.delta.is_empty() || !self.result.changed {
            return Err(invalid("No-change edits must not be stored in history"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct Receipt {
    pub transaction_id: String,
    pub payload_hash: String,
    pub result: EditResult,
}

pub type TransactionReceipt = Receipt;

impl Receipt {
    pub fn validate(&self) -> Result<(), AppError> {
        validate_token(&self.transaction_id, "receipt.transactionId", 256)?;
        validate_token(&self.payload_hash, "receipt.payloadHash", 256)?;
        self.result.validate()?;
        if self.result.transaction_id != self.transaction_id {
            return Err(invalid("Receipt result transaction ID does not match"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct HistoryState {
    pub undo: Vec<HistoryEntry>,
    pub redo: Vec<HistoryEntry>,
}

impl HistoryState {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.undo.len() > MAX_HISTORY_ENTRIES || self.redo.len() > MAX_HISTORY_ENTRIES {
            return Err(invalid("History exceeds its capacity"));
        }
        let mut ids = HashSet::with_capacity(self.undo.len() + self.redo.len());
        for entry in self.undo.iter().chain(self.redo.iter()) {
            entry.validate()?;
            if !ids.insert(entry.transaction_id.as_str()) {
                return Err(invalid(
                    "A transaction cannot appear in both undo and redo history",
                ));
            }
        }
        Ok(())
    }

    pub fn push_undo(&mut self, entry: HistoryEntry) -> Result<(), AppError> {
        entry.validate()?;
        self.undo.push(entry);
        if self.undo.len() > MAX_HISTORY_ENTRIES {
            let remove = self.undo.len() - MAX_HISTORY_ENTRIES;
            self.undo.drain(0..remove);
        }
        Ok(())
    }

    pub fn push_redo(&mut self, entry: HistoryEntry) -> Result<(), AppError> {
        entry.validate()?;
        self.redo.push(entry);
        if self.redo.len() > MAX_HISTORY_ENTRIES {
            let remove = self.redo.len() - MAX_HISTORY_ENTRIES;
            self.redo.drain(0..remove);
        }
        Ok(())
    }

    pub fn clear_redo(&mut self) {
        self.redo.clear();
    }

    pub fn pop_undo(&mut self) -> Option<HistoryEntry> {
        self.undo.pop()
    }

    pub fn pop_redo(&mut self) -> Option<HistoryEntry> {
        self.redo.pop()
    }

    pub fn peek_undo(&self) -> Option<&HistoryEntry> {
        self.undo.last()
    }

    pub fn peek_redo(&self) -> Option<&HistoryEntry> {
        self.redo.last()
    }
}

impl ProjectDocument {
    /// Produce a deterministic affected-entity delta. Revision is deliberately
    /// excluded: the store owns monotonic revision allocation and history
    /// replay advances it independently of content changes.
    pub fn diff(&self, other: &Self) -> Result<TransactionDelta, AppError> {
        self.validate()?;
        other.validate()?;
        self.diff_validated(other)
    }

    /// Diff documents whose graph and entity invariants have already been
    /// checked by the caller. This private store/editor boundary avoids
    /// validating both sides again after a staged candidate was validated.
    pub(crate) fn diff_validated(&self, other: &Self) -> Result<TransactionDelta, AppError> {
        if self.project_id != other.project_id {
            return Err(invalid("Cannot diff documents from different projects"));
        }
        let mut changes = Vec::new();
        if self.name != other.name || self.profile != other.profile {
            changes.push(EntityDelta {
                entity: EntityRef::new(EntityKind::Document, "document"),
                before: Some(EntityState::Document {
                    name: self.name.clone(),
                    profile: self.profile.clone(),
                }),
                after: Some(EntityState::Document {
                    name: other.name.clone(),
                    profile: other.profile.clone(),
                }),
                before_index: None,
                after_index: None,
            });
        }
        diff_collection(
            &self.assets,
            &other.assets,
            EntityKind::Asset,
            |value| &value.id,
            EntityState::Asset,
            &mut changes,
        );
        diff_collection(
            &self.tracks,
            &other.tracks,
            EntityKind::Track,
            |value| &value.id,
            EntityState::Track,
            &mut changes,
        );
        diff_collection(
            &self.clips,
            &other.clips,
            EntityKind::Clip,
            |value| &value.id,
            EntityState::Clip,
            &mut changes,
        );
        diff_collection(
            &self.text_items,
            &other.text_items,
            EntityKind::TextItem,
            |value| &value.id,
            EntityState::TextItem,
            &mut changes,
        );
        diff_collection(
            &self.transitions,
            &other.transitions,
            EntityKind::Transition,
            |value| &value.id,
            EntityState::Transition,
            &mut changes,
        );
        changes.sort_by(|left, right| {
            left.entity
                .kind
                .cmp(&right.entity.kind)
                .then_with(|| left.entity.id.cmp(&right.entity.id))
        });
        Ok(TransactionDelta { changes })
    }
}

fn diff_collection<T, Id, ToState>(
    before: &[T],
    after: &[T],
    kind: EntityKind,
    id: Id,
    to_state: ToState,
    changes: &mut Vec<EntityDelta>,
) where
    T: Clone + PartialEq,
    Id: Fn(&T) -> &str,
    ToState: Fn(T) -> EntityState + Copy,
{
    let before_by_id: HashMap<&str, (usize, &T)> = before
        .iter()
        .enumerate()
        .map(|(index, value)| (id(value), (index, value)))
        .collect();
    let after_by_id: HashMap<&str, (usize, &T)> = after
        .iter()
        .enumerate()
        .map(|(index, value)| (id(value), (index, value)))
        .collect();
    let mut ids = Vec::with_capacity(before_by_id.len() + after_by_id.len());
    ids.extend(before_by_id.keys().copied());
    ids.extend(after_by_id.keys().copied());
    ids.sort_unstable();
    ids.dedup();

    for entity_id in ids {
        let before_value = before_by_id.get(entity_id);
        let after_value = after_by_id.get(entity_id);
        let changed = match (before_value, after_value) {
            (Some((before_index, before)), Some((after_index, after))) => {
                before != after || before_index != after_index
            }
            _ => true,
        };
        if !changed {
            continue;
        }
        let before_state = before_value.map(|(_, value)| to_state((*value).clone()));
        let after_state = after_value.map(|(_, value)| to_state((*value).clone()));
        let before_index = before_value.map(|(index, _)| *index as u32);
        let after_index = after_value.map(|(index, _)| *index as u32);
        changes.push(EntityDelta {
            entity: EntityRef::new(kind, entity_id),
            before: before_state,
            after: after_state,
            before_index,
            after_index,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::model::{AspectRatio, FrameRate, Track, TrackKind};

    fn empty_document() -> ProjectDocument {
        ProjectDocument::new("history fixture", AspectRatio::Landscape, FrameRate::FPS_30)
            .expect("document")
    }

    fn assert_round_trip(before: &ProjectDocument, after: &ProjectDocument) {
        let delta = before.diff(after).expect("diff");
        let mut forward = before.clone();
        delta.apply_forward(&mut forward).expect("forward replay");
        assert_eq!(forward, *after);

        let mut backward = after.clone();
        delta
            .apply_backward(&mut backward)
            .expect("backward replay");
        assert_eq!(backward, *before);
    }

    #[test]
    fn diff_preserves_reorder_insert_delete_and_skips_unchanged_payloads() {
        let before = empty_document();
        assert!(before.diff(&before).expect("unchanged diff").is_empty());

        let mut reordered = before.clone();
        reordered.tracks.swap(0, 1);
        assert_round_trip(&before, &reordered);

        let mut inserted = before.clone();
        inserted.tracks.insert(
            1,
            Track::new(
                "90000000-0000-4000-8000-000000000001".to_owned(),
                TrackKind::Audio,
                "Inserted".to_owned(),
            )
            .expect("track"),
        );
        assert_round_trip(&before, &inserted);

        let mut deleted = before.clone();
        deleted.tracks.remove(1);
        assert_round_trip(&before, &deleted);
    }
}

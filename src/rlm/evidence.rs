use std::collections::HashMap;

use crate::domain::StoredObservation;
use crate::rlm::validate::HandleKind;

/// A unique investigation identifier.
pub type InvestigationId = u64;

/// A unique handle identifier within an investigation.
pub type HandleId = String;

/// A unique evidence identifier (opaque, keyed).
pub type EvidenceId = String;

/// Identifies one sanitized observation at one immutable source revision.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceHandleV1 {
    pub id: EvidenceId,
    pub observation_id: i64,
    pub revision_id: i64,
    pub ordinal: i64,
    pub sanitized_content_hash: Vec<u8>,
    pub investigation_id: InvestigationId,
}

/// Integrity error when verifying an evidence handle.
#[derive(Debug, PartialEq)]
pub enum IntegrityError {
    ContentHashMismatch,
    RevisionMismatch,
    OrdinalMismatch,
    WrongInvestigation,
    ObservationNotFound,
}

impl std::fmt::Display for IntegrityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for IntegrityError {}

/// Create an evidence handle from a stored observation.
pub fn create_evidence_handle(
    obs: &StoredObservation,
    investigation_id: InvestigationId,
    installation_key: &[u8],
) -> EvidenceHandleV1 {
    let content_hash = content_hash(obs.safe_text.as_bytes());
    let id = keyed_evidence_id(obs.id, investigation_id, installation_key);

    EvidenceHandleV1 {
        id,
        observation_id: obs.id,
        revision_id: obs.revision_id,
        ordinal: obs.ordinal,
        sanitized_content_hash: content_hash,
        investigation_id,
    }
}

/// Verify that a handle still matches its source observation.
pub fn verify_handle(
    handle: &EvidenceHandleV1,
    obs: &StoredObservation,
    investigation_id: InvestigationId,
) -> Result<(), IntegrityError> {
    if handle.investigation_id != investigation_id {
        return Err(IntegrityError::WrongInvestigation);
    }
    if handle.revision_id != obs.revision_id {
        return Err(IntegrityError::RevisionMismatch);
    }
    if handle.ordinal != obs.ordinal {
        return Err(IntegrityError::OrdinalMismatch);
    }
    let expected_hash = content_hash(obs.safe_text.as_bytes());
    if handle.sanitized_content_hash != expected_hash {
        return Err(IntegrityError::ContentHashMismatch);
    }
    Ok(())
}

/// FNV-1a hash (placeholder for BLAKE3).
fn content_hash(data: &[u8]) -> Vec<u8> {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash.to_le_bytes().to_vec()
}

/// Generate a keyed evidence ID that doesn't expose internal identifiers.
fn keyed_evidence_id(obs_id: i64, investigation_id: InvestigationId, key: &[u8]) -> EvidenceId {
    let mut input = Vec::new();
    input.extend_from_slice(&obs_id.to_le_bytes());
    input.extend_from_slice(&investigation_id.to_le_bytes());
    input.extend_from_slice(key);
    let hash = content_hash(&input);
    format!("ev1_{}", hex_encode(&hash))
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

// --- Handle registry ---

/// An immutable result set held outside model context.
#[derive(Debug, Clone)]
pub struct ResultHandleV1 {
    pub id: HandleId,
    pub investigation_id: InvestigationId,
    pub kind: HandleKind,
    pub count: u32,
}

/// Entry stored in the registry.
struct RegistryEntry {
    handle: ResultHandleV1,
    observations: Vec<StoredObservation>,
    evidence_handles: Vec<EvidenceHandleV1>,
}

/// Manages result handles for one investigation.
pub struct HandleRegistry {
    investigation_id: InvestigationId,
    installation_key: Vec<u8>,
    entries: HashMap<HandleId, RegistryEntry>,
    next_handle: u64,
    max_handles: usize,
}

impl HandleRegistry {
    pub fn new(investigation_id: InvestigationId, installation_key: Vec<u8>, max_handles: usize) -> Self {
        Self {
            investigation_id,
            installation_key,
            entries: HashMap::new(),
            next_handle: 0,
            max_handles,
        }
    }

    pub fn investigation_id(&self) -> InvestigationId {
        self.investigation_id
    }

    /// Insert an evidence set. Returns the handle ID.
    pub fn insert_evidence_set(&mut self, observations: Vec<StoredObservation>) -> HandleId {
        self.insert(HandleKind::EvidenceSet, observations)
    }

    /// Insert an assessment set. Returns the handle ID.
    pub fn insert_assessment_set(&mut self, observations: Vec<StoredObservation>) -> HandleId {
        self.insert(HandleKind::AssessmentSet, observations)
    }

    fn insert(&mut self, kind: HandleKind, observations: Vec<StoredObservation>) -> HandleId {
        let id = format!("h_{}", self.next_handle);
        self.next_handle += 1;

        let evidence_handles: Vec<EvidenceHandleV1> = observations
            .iter()
            .map(|obs| create_evidence_handle(obs, self.investigation_id, &self.installation_key))
            .collect();

        let count = observations.len() as u32;
        let handle = ResultHandleV1 {
            id: id.clone(),
            investigation_id: self.investigation_id,
            kind,
            count,
        };

        self.entries.insert(id.clone(), RegistryEntry {
            handle,
            observations,
            evidence_handles,
        });

        id
    }

    /// Look up a handle by ID. Returns None if not found or wrong investigation.
    pub fn get(&self, handle_id: &str) -> Option<&ResultHandleV1> {
        self.entries.get(handle_id).map(|e| &e.handle)
    }

    /// Get the observations for a handle.
    pub fn get_observations(&self, handle_id: &str) -> Option<&[StoredObservation]> {
        self.entries.get(handle_id).map(|e| e.observations.as_slice())
    }

    /// Get the evidence handles for a result handle.
    pub fn get_evidence_handles(&self, handle_id: &str) -> Option<&[EvidenceHandleV1]> {
        self.entries.get(handle_id).map(|e| e.evidence_handles.as_slice())
    }

    /// Check if a specific evidence ID exists in any handle of this investigation.
    pub fn resolve_evidence_id(&self, evidence_id: &str) -> Option<(&EvidenceHandleV1, &StoredObservation)> {
        for entry in self.entries.values() {
            for (i, eh) in entry.evidence_handles.iter().enumerate() {
                if eh.id == evidence_id {
                    return Some((eh, &entry.observations[i]));
                }
            }
        }
        None
    }

    /// Whether the registry has no handles.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of handles in the registry.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry is at capacity.
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.max_handles
    }

    /// Summary metadata for root state construction.
    pub fn summaries(&self) -> Vec<HandleSummary> {
        self.entries
            .values()
            .map(|e| HandleSummary {
                id: e.handle.id.clone(),
                kind: e.handle.kind,
                count: e.handle.count,
            })
            .collect()
    }
}

/// Minimal summary shown to the root model.
#[derive(Debug, Clone)]
pub struct HandleSummary {
    pub id: HandleId,
    pub kind: HandleKind,
    pub count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ObservationKind, StoredObservation};

    fn make_stored(id: i64, revision_id: i64, ordinal: i64, text: &str) -> StoredObservation {
        StoredObservation {
            id,
            revision_id,
            ordinal,
            kind: ObservationKind::ZshCommand,
            observed_at_ms: Some(1_700_000_000_000),
            safe_text: text.to_string(),
            flags: 0,
            locator: vec![],
        }
    }

    const KEY: &[u8] = b"test-installation-key";

    #[test]
    fn create_and_verify_handle() {
        let obs = make_stored(1, 10, 0, "cargo build --release");
        let handle = create_evidence_handle(&obs, 100, KEY);

        assert!(handle.id.starts_with("ev1_"));
        assert_eq!(handle.observation_id, 1);
        assert_eq!(handle.revision_id, 10);
        assert_eq!(handle.investigation_id, 100);

        assert!(verify_handle(&handle, &obs, 100).is_ok());
    }

    #[test]
    fn forged_content_hash_detected() {
        let obs = make_stored(1, 10, 0, "cargo build --release");
        let mut handle = create_evidence_handle(&obs, 100, KEY);
        handle.sanitized_content_hash = vec![0xFF; 8]; // forge

        assert_eq!(
            verify_handle(&handle, &obs, 100).unwrap_err(),
            IntegrityError::ContentHashMismatch
        );
    }

    #[test]
    fn forged_revision_detected() {
        let obs = make_stored(1, 10, 0, "cargo build --release");
        let mut handle = create_evidence_handle(&obs, 100, KEY);
        handle.revision_id = 999; // forge

        assert_eq!(
            verify_handle(&handle, &obs, 100).unwrap_err(),
            IntegrityError::RevisionMismatch
        );
    }

    #[test]
    fn forged_ordinal_detected() {
        let obs = make_stored(1, 10, 0, "cargo build --release");
        let mut handle = create_evidence_handle(&obs, 100, KEY);
        handle.ordinal = 42; // forge

        assert_eq!(
            verify_handle(&handle, &obs, 100).unwrap_err(),
            IntegrityError::OrdinalMismatch
        );
    }

    #[test]
    fn cross_investigation_reuse_detected() {
        let obs = make_stored(1, 10, 0, "cargo build --release");
        let handle = create_evidence_handle(&obs, 100, KEY);

        // Try to use in investigation 200
        assert_eq!(
            verify_handle(&handle, &obs, 200).unwrap_err(),
            IntegrityError::WrongInvestigation
        );
    }

    #[test]
    fn altered_observation_text_detected() {
        let obs = make_stored(1, 10, 0, "cargo build --release");
        let handle = create_evidence_handle(&obs, 100, KEY);

        // Observation text changed after handle was created
        let altered = make_stored(1, 10, 0, "rm -rf /");
        assert_eq!(
            verify_handle(&handle, &altered, 100).unwrap_err(),
            IntegrityError::ContentHashMismatch
        );
    }

    // --- Registry tests ---

    #[test]
    fn registry_insert_and_lookup() {
        let mut reg = HandleRegistry::new(100, KEY.to_vec(), 64);
        let obs = vec![
            make_stored(1, 10, 0, "cargo build"),
            make_stored(2, 10, 1, "cargo test"),
        ];

        let hid = reg.insert_evidence_set(obs);
        let handle = reg.get(&hid).unwrap();
        assert_eq!(handle.kind, HandleKind::EvidenceSet);
        assert_eq!(handle.count, 2);
        assert_eq!(reg.get_observations(&hid).unwrap().len(), 2);
    }

    #[test]
    fn registry_nonexistent_handle_returns_none() {
        let reg = HandleRegistry::new(100, KEY.to_vec(), 64);
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn registry_evidence_id_resolution() {
        let mut reg = HandleRegistry::new(100, KEY.to_vec(), 64);
        let obs = vec![make_stored(1, 10, 0, "cargo build")];
        let hid = reg.insert_evidence_set(obs);

        let ev_handles = reg.get_evidence_handles(&hid).unwrap();
        let ev_id = &ev_handles[0].id;

        let (resolved_handle, resolved_obs) = reg.resolve_evidence_id(ev_id).unwrap();
        assert_eq!(resolved_handle.observation_id, 1);
        assert_eq!(resolved_obs.safe_text, "cargo build");
    }

    #[test]
    fn registry_unknown_evidence_id_returns_none() {
        let reg = HandleRegistry::new(100, KEY.to_vec(), 64);
        assert!(reg.resolve_evidence_id("ev1_fake").is_none());
    }

    #[test]
    fn registry_summaries() {
        let mut reg = HandleRegistry::new(100, KEY.to_vec(), 64);
        reg.insert_evidence_set(vec![make_stored(1, 10, 0, "cmd1")]);
        reg.insert_evidence_set(vec![make_stored(2, 10, 1, "cmd2"), make_stored(3, 10, 2, "cmd3")]);

        let summaries = reg.summaries();
        assert_eq!(summaries.len(), 2);
        let counts: Vec<u32> = summaries.iter().map(|s| s.count).collect();
        assert!(counts.contains(&1));
        assert!(counts.contains(&2));
    }
}

use crate::rlm::evidence::HandleRegistry;
use crate::rlm::protocol::*;

#[derive(Debug, PartialEq)]
pub enum FinishValidationError {
    UnresolvableCitation(String),
    UncitedClaim { claim_text: String },
    UncitedProcedureStep { step_text: String },
    AnsweredWithoutClaims,
}

impl std::fmt::Display for FinishValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnresolvableCitation(id) => {
                write!(f, "it cites {id}, which this investigation never showed it")
            }
            Self::UncitedClaim { claim_text } => {
                write!(f, "the claim \"{claim_text}\" cites no record")
            }
            Self::UncitedProcedureStep { step_text } => {
                write!(f, "the step \"{step_text}\" cites no record")
            }
            Self::AnsweredWithoutClaims => f.write_str("it answered without any claim"),
        }
    }
}

impl std::error::Error for FinishValidationError {}

/// Result of finish validation — may downgrade status.
#[derive(Debug)]
pub struct ValidatedFinish {
    pub status: AnswerStatus,
    pub finish: FinishV1,
}

/// Validate a FinishV1 against the handle registry.
/// - Every citation must resolve to an evidence handle in the registry.
/// - Every claim must have at least one supporting citation.
/// - Every procedure step must have at least one supporting citation.
/// - If validation would fail, downgrade status rather than reject outright.
pub fn validate_finish(
    finish: &FinishV1,
    registry: &HandleRegistry,
) -> Result<ValidatedFinish, FinishValidationError> {
    // Check all claims have supports and all citations resolve.
    for claim in &finish.claims {
        if claim.supports.is_empty() {
            return Err(FinishValidationError::UncitedClaim {
                claim_text: claim.text.clone(),
            });
        }
        for ev_id in claim.supports.iter().chain(claim.contradicts.iter()) {
            if registry.resolve_evidence_id(ev_id).is_none() {
                return Err(FinishValidationError::UnresolvableCitation(ev_id.clone()));
            }
        }
    }

    // Check all procedure steps have supports and citations resolve.
    for step in &finish.procedure {
        if step.supports.is_empty() {
            return Err(FinishValidationError::UncitedProcedureStep {
                step_text: step.text.clone(),
            });
        }
        for ev_id in &step.supports {
            if registry.resolve_evidence_id(ev_id).is_none() {
                return Err(FinishValidationError::UnresolvableCitation(ev_id.clone()));
            }
        }
    }

    // Enforce sufficiency: answered requires at least one claim.
    let status = if finish.status == AnswerStatus::Answered && finish.claims.is_empty() {
        return Err(FinishValidationError::AnsweredWithoutClaims);
    } else {
        finish.status.clone()
    };

    Ok(ValidatedFinish {
        status,
        finish: finish.clone(),
    })
}

/// Force abstention: downgrade status when evidence is insufficient.
/// Returns a modified finish with status set to abstained and missing list populated.
pub fn force_abstention(finish: &FinishV1, reasons: Vec<String>) -> FinishV1 {
    FinishV1 {
        status: AnswerStatus::Abstained,
        answer: String::new(),
        claims: vec![],
        procedure: vec![],
        contradictions: finish.contradictions.clone(),
        missing: reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ObservationKind, StoredObservation};
    use crate::rlm::evidence::HandleRegistry;

    fn make_obs(id: i64, text: &str) -> StoredObservation {
        StoredObservation {
            id,
            revision_id: 1,
            ordinal: id,
            kind: ObservationKind::ZshCommand,
            observed_at_ms: Some(1_700_000_000_000),
            safe_text: text.to_string(),
            flags: 0,
            locator: vec![],
        }
    }

    fn registry_with_evidence() -> (HandleRegistry, Vec<String>) {
        let mut reg = HandleRegistry::new(64);
        let obs = vec![
            make_obs(1, "certbot renew --nginx"),
            make_obs(2, "certbot status"),
        ];
        let hid = reg.insert_evidence_set(obs);
        let ev_ids: Vec<String> = reg
            .get_evidence_handles(&hid)
            .unwrap()
            .iter()
            .map(|eh| eh.id.clone())
            .collect();
        (reg, ev_ids)
    }

    #[test]
    fn valid_finish_passes() {
        let (reg, ev_ids) = registry_with_evidence();
        let finish = FinishV1 {
            status: AnswerStatus::Answered,
            answer: "Certificate renewed.".to_string(),
            claims: vec![FinalClaimV1 {
                text: "certbot renew was run".to_string(),
                supports: vec![ev_ids[0].clone()],
                contradicts: vec![],
            }],
            procedure: vec![ProcedureStepV1 {
                text: "Run certbot renew".to_string(),
                supports: vec![ev_ids[0].clone()],
            }],
            contradictions: vec![],
            missing: vec![],
        };

        let result = validate_finish(&finish, &reg).unwrap();
        assert_eq!(result.status, AnswerStatus::Answered);
    }

    #[test]
    fn forged_citation_rejected() {
        let (reg, _) = registry_with_evidence();
        let finish = FinishV1 {
            status: AnswerStatus::Answered,
            answer: "Forged.".to_string(),
            claims: vec![FinalClaimV1 {
                text: "claim".to_string(),
                supports: vec!["ev1_FORGED_ID".to_string()],
                contradicts: vec![],
            }],
            procedure: vec![],
            contradictions: vec![],
            missing: vec![],
        };

        assert_eq!(
            validate_finish(&finish, &reg).unwrap_err(),
            FinishValidationError::UnresolvableCitation("ev1_FORGED_ID".to_string())
        );
    }

    #[test]
    fn uncited_claim_rejected() {
        let (reg, _) = registry_with_evidence();
        let finish = FinishV1 {
            status: AnswerStatus::Answered,
            answer: "No citation.".to_string(),
            claims: vec![FinalClaimV1 {
                text: "unsupported claim".to_string(),
                supports: vec![],
                contradicts: vec![],
            }],
            procedure: vec![],
            contradictions: vec![],
            missing: vec![],
        };

        assert!(matches!(
            validate_finish(&finish, &reg).unwrap_err(),
            FinishValidationError::UncitedClaim { .. }
        ));
    }

    #[test]
    fn uncited_procedure_step_rejected() {
        let (reg, ev_ids) = registry_with_evidence();
        let finish = FinishV1 {
            status: AnswerStatus::Answered,
            answer: "Has claim, no procedure citation.".to_string(),
            claims: vec![FinalClaimV1 {
                text: "valid claim".to_string(),
                supports: vec![ev_ids[0].clone()],
                contradicts: vec![],
            }],
            procedure: vec![ProcedureStepV1 {
                text: "Run something".to_string(),
                supports: vec![], // no citation!
            }],
            contradictions: vec![],
            missing: vec![],
        };

        assert!(matches!(
            validate_finish(&finish, &reg).unwrap_err(),
            FinishValidationError::UncitedProcedureStep { .. }
        ));
    }

    #[test]
    fn answered_without_claims_rejected() {
        let (reg, _) = registry_with_evidence();
        let finish = FinishV1 {
            status: AnswerStatus::Answered,
            answer: "I answered with no claims.".to_string(),
            claims: vec![],
            procedure: vec![],
            contradictions: vec![],
            missing: vec![],
        };

        assert_eq!(
            validate_finish(&finish, &reg).unwrap_err(),
            FinishValidationError::AnsweredWithoutClaims
        );
    }

    #[test]
    fn cross_investigation_citation_rejected() {
        // Registry from investigation 1.
        let (_, ev_ids) = registry_with_evidence();

        // New registry for investigation 2 — different evidence IDs.
        let reg2 = HandleRegistry::new(64);

        let finish = FinishV1 {
            status: AnswerStatus::Answered,
            answer: "Cross-investigation.".to_string(),
            claims: vec![FinalClaimV1 {
                text: "claim".to_string(),
                supports: vec![ev_ids[0].clone()], // from investigation 1
                contradicts: vec![],
            }],
            procedure: vec![],
            contradictions: vec![],
            missing: vec![],
        };

        // Citation from investigation 1 doesn't resolve in investigation 2's registry.
        assert!(matches!(
            validate_finish(&finish, &reg2).unwrap_err(),
            FinishValidationError::UnresolvableCitation(_)
        ));
    }

    #[test]
    fn force_abstention_clears_claims_and_procedure() {
        let finish = FinishV1 {
            status: AnswerStatus::Answered,
            answer: "Some answer.".to_string(),
            claims: vec![FinalClaimV1 {
                text: "claim".to_string(),
                supports: vec!["ev1_x".to_string()],
                contradicts: vec![],
            }],
            procedure: vec![ProcedureStepV1 {
                text: "do something".to_string(),
                supports: vec!["ev1_x".to_string()],
            }],
            contradictions: vec![],
            missing: vec![],
        };

        let abstained = force_abstention(&finish, vec!["no decisive evidence".to_string()]);
        assert_eq!(abstained.status, AnswerStatus::Abstained);
        assert!(abstained.claims.is_empty());
        assert!(abstained.procedure.is_empty());
        assert_eq!(abstained.missing, vec!["no decisive evidence"]);
    }

    #[test]
    fn abstained_finish_with_missing_passes_validation() {
        let reg = HandleRegistry::new(64);
        let finish = FinishV1 {
            status: AnswerStatus::Abstained,
            answer: String::new(),
            claims: vec![],
            procedure: vec![],
            contradictions: vec![],
            missing: vec!["no certificate commands found".to_string()],
        };

        let result = validate_finish(&finish, &reg).unwrap();
        assert_eq!(result.status, AnswerStatus::Abstained);
    }
}

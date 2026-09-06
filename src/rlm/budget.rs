use crate::rlm::protocol::BudgetV1;

/// Tracks consumed and reserved budget across all fence dimensions.
pub struct BudgetLedger {
    limits: BudgetV1,
    consumed: Counters,
}

#[derive(Debug, Clone, Default)]
struct Counters {
    root_calls: u16,
    plan_ops: u16,
    child_calls: u16,
    rows_returned: u64,
    evidence_handles: u32,
    local_materialized_bytes: u64,
    model_egress_bytes: u64,
}

/// Which budget dimension was exhausted.
#[derive(Debug, PartialEq)]
pub enum BudgetExhausted {
    RootCalls,
    PlanOps,
    ChildCalls,
    RowsReturned,
    EvidenceHandles,
    LocalMaterializedBytes,
    ModelEgressBytes,
}

impl std::fmt::Display for BudgetExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for BudgetExhausted {}

/// A reservation request for a specific operation.
#[derive(Debug, Clone, Default)]
pub struct Reservation {
    pub root_calls: u16,
    pub plan_ops: u16,
    pub child_calls: u16,
    pub rows_returned: u64,
    pub evidence_handles: u32,
    pub local_materialized_bytes: u64,
    pub model_egress_bytes: u64,
}

impl BudgetLedger {
    pub fn new(limits: BudgetV1) -> Self {
        Self {
            limits,
            consumed: Counters::default(),
        }
    }

    /// Check if a reservation can be satisfied. If yes, consume it atomically.
    pub fn reserve(&mut self, res: &Reservation) -> Result<(), BudgetExhausted> {
        // Check each dimension before consuming any.
        if self.consumed.root_calls + res.root_calls > self.limits.root_calls {
            return Err(BudgetExhausted::RootCalls);
        }
        if self.consumed.plan_ops + res.plan_ops > self.limits.plan_ops {
            return Err(BudgetExhausted::PlanOps);
        }
        if self.consumed.child_calls + res.child_calls > self.limits.child_calls {
            return Err(BudgetExhausted::ChildCalls);
        }
        if self.consumed.rows_returned + res.rows_returned > self.limits.rows_returned {
            return Err(BudgetExhausted::RowsReturned);
        }
        if self.consumed.evidence_handles + res.evidence_handles > self.limits.evidence_handles {
            return Err(BudgetExhausted::EvidenceHandles);
        }
        if self.consumed.local_materialized_bytes + res.local_materialized_bytes
            > self.limits.local_materialized_bytes
        {
            return Err(BudgetExhausted::LocalMaterializedBytes);
        }
        if self.consumed.model_egress_bytes + res.model_egress_bytes
            > self.limits.model_egress_bytes
        {
            return Err(BudgetExhausted::ModelEgressBytes);
        }

        // All checks passed — consume atomically.
        self.consumed.root_calls += res.root_calls;
        self.consumed.plan_ops += res.plan_ops;
        self.consumed.child_calls += res.child_calls;
        self.consumed.rows_returned += res.rows_returned;
        self.consumed.evidence_handles += res.evidence_handles;
        self.consumed.local_materialized_bytes += res.local_materialized_bytes;
        self.consumed.model_egress_bytes += res.model_egress_bytes;
        Ok(())
    }

    /// Release unused portion of a previous reservation.
    pub fn release_rows(&mut self, released: u64) {
        self.consumed.rows_returned = self.consumed.rows_returned.saturating_sub(released);
    }

    /// Remaining budget for display/root state.
    pub fn remaining(&self) -> BudgetV1 {
        BudgetV1 {
            wall_ms: self.limits.wall_ms, // wall clock tracked separately
            root_calls: self.limits.root_calls - self.consumed.root_calls,
            plan_ops: self.limits.plan_ops - self.consumed.plan_ops,
            child_calls: self.limits.child_calls - self.consumed.child_calls,
            provider_attempts: self.limits.provider_attempts,
            max_concurrency: self.limits.max_concurrency,
            max_depth: self.limits.max_depth,
            rows_returned: self.limits.rows_returned - self.consumed.rows_returned,
            evidence_handles: self.limits.evidence_handles - self.consumed.evidence_handles,
            local_materialized_bytes: self.limits.local_materialized_bytes
                - self.consumed.local_materialized_bytes,
            model_egress_bytes: self.limits.model_egress_bytes
                - self.consumed.model_egress_bytes,
            estimated_input_tokens: self.limits.estimated_input_tokens,
            reserved_output_tokens: self.limits.reserved_output_tokens,
            max_cost_microusd: self.limits.max_cost_microusd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_budget() -> BudgetV1 {
        BudgetV1 {
            wall_ms: 10_000,
            root_calls: 4,
            plan_ops: 8,
            child_calls: 8,
            provider_attempts: 10,
            max_concurrency: 2,
            max_depth: 1,
            rows_returned: 100,
            evidence_handles: 10,
            local_materialized_bytes: 4096,
            model_egress_bytes: 2048,
            estimated_input_tokens: 10_000,
            reserved_output_tokens: 2_000,
            max_cost_microusd: None,
        }
    }

    #[test]
    fn reserve_within_limits_succeeds() {
        let mut ledger = BudgetLedger::new(small_budget());
        let res = Reservation {
            root_calls: 1,
            child_calls: 2,
            rows_returned: 50,
            ..Default::default()
        };
        assert!(ledger.reserve(&res).is_ok());
        assert_eq!(ledger.remaining().root_calls, 3);
        assert_eq!(ledger.remaining().child_calls, 6);
        assert_eq!(ledger.remaining().rows_returned, 50);
    }

    #[test]
    fn reserve_at_exact_limit_succeeds() {
        let mut ledger = BudgetLedger::new(small_budget());
        let res = Reservation {
            root_calls: 4,
            ..Default::default()
        };
        assert!(ledger.reserve(&res).is_ok());
        assert_eq!(ledger.remaining().root_calls, 0);
    }

    #[test]
    fn root_calls_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        // Consume 3
        ledger.reserve(&Reservation { root_calls: 3, ..Default::default() }).unwrap();
        // Try to consume 2 more (only 1 remaining)
        let err = ledger.reserve(&Reservation { root_calls: 2, ..Default::default() }).unwrap_err();
        assert_eq!(err, BudgetExhausted::RootCalls);
    }

    #[test]
    fn child_calls_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger.reserve(&Reservation { child_calls: 7, ..Default::default() }).unwrap();
        let err = ledger.reserve(&Reservation { child_calls: 2, ..Default::default() }).unwrap_err();
        assert_eq!(err, BudgetExhausted::ChildCalls);
    }

    #[test]
    fn plan_ops_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger.reserve(&Reservation { plan_ops: 8, ..Default::default() }).unwrap();
        let err = ledger.reserve(&Reservation { plan_ops: 1, ..Default::default() }).unwrap_err();
        assert_eq!(err, BudgetExhausted::PlanOps);
    }

    #[test]
    fn rows_returned_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger.reserve(&Reservation { rows_returned: 90, ..Default::default() }).unwrap();
        let err = ledger.reserve(&Reservation { rows_returned: 20, ..Default::default() }).unwrap_err();
        assert_eq!(err, BudgetExhausted::RowsReturned);
    }

    #[test]
    fn evidence_handles_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger.reserve(&Reservation { evidence_handles: 10, ..Default::default() }).unwrap();
        let err = ledger.reserve(&Reservation { evidence_handles: 1, ..Default::default() }).unwrap_err();
        assert_eq!(err, BudgetExhausted::EvidenceHandles);
    }

    #[test]
    fn materialized_bytes_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger.reserve(&Reservation { local_materialized_bytes: 4000, ..Default::default() }).unwrap();
        let err = ledger.reserve(&Reservation { local_materialized_bytes: 200, ..Default::default() }).unwrap_err();
        assert_eq!(err, BudgetExhausted::LocalMaterializedBytes);
    }

    #[test]
    fn model_egress_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger.reserve(&Reservation { model_egress_bytes: 2000, ..Default::default() }).unwrap();
        let err = ledger.reserve(&Reservation { model_egress_bytes: 100, ..Default::default() }).unwrap_err();
        assert_eq!(err, BudgetExhausted::ModelEgressBytes);
    }

    #[test]
    fn release_rows_restores_budget() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger.reserve(&Reservation { rows_returned: 80, ..Default::default() }).unwrap();
        assert_eq!(ledger.remaining().rows_returned, 20);

        // Operator only used 30, release 50
        ledger.release_rows(50);
        assert_eq!(ledger.remaining().rows_returned, 70);
    }

    #[test]
    fn failed_reserve_does_not_consume() {
        let mut ledger = BudgetLedger::new(small_budget());
        // This will fail because root_calls limit is 4
        let res = Reservation {
            root_calls: 5,
            child_calls: 1,
            rows_returned: 10,
            ..Default::default()
        };
        assert!(ledger.reserve(&res).is_err());
        // Nothing should have been consumed
        assert_eq!(ledger.remaining().root_calls, 4);
        assert_eq!(ledger.remaining().child_calls, 8);
        assert_eq!(ledger.remaining().rows_returned, 100);
    }
}

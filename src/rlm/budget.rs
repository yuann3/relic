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
    provider_attempts: u16,
    output_tokens: u64,
    cost_microusd: u64,
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
    ProviderAttempts,
    OutputTokens,
    Cost,
    WallClock,
}

impl std::fmt::Display for BudgetExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RootCalls => "out of root calls",
            Self::PlanOps => "out of plan operations",
            Self::ChildCalls => "out of child calls",
            Self::RowsReturned => "out of rows to return",
            Self::EvidenceHandles => "out of evidence handles",
            Self::LocalMaterializedBytes => "out of bytes to read from the store",
            Self::ModelEgressBytes => "out of bytes to send to the model",
            Self::ProviderAttempts => "out of provider attempts",
            Self::OutputTokens => "out of output tokens",
            Self::Cost => "out of money: the cost cap is spent",
            Self::WallClock => "out of time",
        })
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
    pub provider_attempts: u16,
    pub output_tokens: u64,
    /// Zero when the profile has no prices: the cost is then unknown, not
    /// fenced (research, budget and cost lease).
    pub cost_microusd: u64,
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
        if self.consumed.provider_attempts + res.provider_attempts > self.limits.provider_attempts {
            return Err(BudgetExhausted::ProviderAttempts);
        }
        if self.consumed.output_tokens + res.output_tokens > self.limits.reserved_output_tokens {
            return Err(BudgetExhausted::OutputTokens);
        }
        if self
            .limits
            .max_cost_microusd
            .is_some_and(|max| self.consumed.cost_microusd + res.cost_microusd > max)
        {
            return Err(BudgetExhausted::Cost);
        }

        // All checks passed — consume atomically.
        self.consumed.root_calls += res.root_calls;
        self.consumed.plan_ops += res.plan_ops;
        self.consumed.child_calls += res.child_calls;
        self.consumed.rows_returned += res.rows_returned;
        self.consumed.evidence_handles += res.evidence_handles;
        self.consumed.local_materialized_bytes += res.local_materialized_bytes;
        self.consumed.model_egress_bytes += res.model_egress_bytes;
        self.consumed.provider_attempts += res.provider_attempts;
        self.consumed.output_tokens += res.output_tokens;
        self.consumed.cost_microusd += res.cost_microusd;
        Ok(())
    }

    /// Give back what a call reserved but its provider-reported usage shows
    /// it did not spend. Only authoritative usage may release a reservation.
    pub fn release_output(&mut self, tokens: u64, cost_microusd: u64) {
        self.consumed.output_tokens = self.consumed.output_tokens.saturating_sub(tokens);
        self.consumed.cost_microusd = self.consumed.cost_microusd.saturating_sub(cost_microusd);
    }

    /// Release unused portion of a previous reservation.
    pub fn release_rows(&mut self, released: u64) {
        self.consumed.rows_returned = self.consumed.rows_returned.saturating_sub(released);
    }

    /// Micro-US-dollars spent so far, at most: a call whose usage went
    /// unreported keeps its whole reservation.
    pub fn cost_spent(&self) -> u64 {
        self.consumed.cost_microusd
    }

    /// Remaining budget for display/root state.
    pub fn remaining(&self) -> BudgetV1 {
        BudgetV1 {
            wall_ms: self.limits.wall_ms, // wall clock tracked separately
            root_calls: self.limits.root_calls - self.consumed.root_calls,
            plan_ops: self.limits.plan_ops - self.consumed.plan_ops,
            child_calls: self.limits.child_calls - self.consumed.child_calls,
            provider_attempts: self.limits.provider_attempts - self.consumed.provider_attempts,
            max_concurrency: self.limits.max_concurrency,
            max_depth: self.limits.max_depth,
            rows_returned: self.limits.rows_returned - self.consumed.rows_returned,
            evidence_handles: self.limits.evidence_handles - self.consumed.evidence_handles,
            local_materialized_bytes: self.limits.local_materialized_bytes
                - self.consumed.local_materialized_bytes,
            model_egress_bytes: self.limits.model_egress_bytes - self.consumed.model_egress_bytes,
            estimated_input_tokens: self.limits.estimated_input_tokens,
            reserved_output_tokens: self.limits.reserved_output_tokens
                - self.consumed.output_tokens,
            max_cost_microusd: self
                .limits
                .max_cost_microusd
                .map(|max| max - self.consumed.cost_microusd),
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
        ledger
            .reserve(&Reservation {
                root_calls: 3,
                ..Default::default()
            })
            .unwrap();
        // Try to consume 2 more (only 1 remaining)
        let err = ledger
            .reserve(&Reservation {
                root_calls: 2,
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(err, BudgetExhausted::RootCalls);
    }

    #[test]
    fn child_calls_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger
            .reserve(&Reservation {
                child_calls: 7,
                ..Default::default()
            })
            .unwrap();
        let err = ledger
            .reserve(&Reservation {
                child_calls: 2,
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(err, BudgetExhausted::ChildCalls);
    }

    #[test]
    fn plan_ops_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger
            .reserve(&Reservation {
                plan_ops: 8,
                ..Default::default()
            })
            .unwrap();
        let err = ledger
            .reserve(&Reservation {
                plan_ops: 1,
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(err, BudgetExhausted::PlanOps);
    }

    #[test]
    fn rows_returned_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger
            .reserve(&Reservation {
                rows_returned: 90,
                ..Default::default()
            })
            .unwrap();
        let err = ledger
            .reserve(&Reservation {
                rows_returned: 20,
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(err, BudgetExhausted::RowsReturned);
    }

    #[test]
    fn evidence_handles_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger
            .reserve(&Reservation {
                evidence_handles: 10,
                ..Default::default()
            })
            .unwrap();
        let err = ledger
            .reserve(&Reservation {
                evidence_handles: 1,
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(err, BudgetExhausted::EvidenceHandles);
    }

    #[test]
    fn materialized_bytes_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger
            .reserve(&Reservation {
                local_materialized_bytes: 4000,
                ..Default::default()
            })
            .unwrap();
        let err = ledger
            .reserve(&Reservation {
                local_materialized_bytes: 200,
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(err, BudgetExhausted::LocalMaterializedBytes);
    }

    #[test]
    fn model_egress_fence_blocks() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger
            .reserve(&Reservation {
                model_egress_bytes: 2000,
                ..Default::default()
            })
            .unwrap();
        let err = ledger
            .reserve(&Reservation {
                model_egress_bytes: 100,
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(err, BudgetExhausted::ModelEgressBytes);
    }

    #[test]
    fn release_rows_restores_budget() {
        let mut ledger = BudgetLedger::new(small_budget());
        ledger
            .reserve(&Reservation {
                rows_returned: 80,
                ..Default::default()
            })
            .unwrap();
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

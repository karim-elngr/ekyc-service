//! Pinned synthetic address contracts. Percentages are integers over buckets 0..99.
use super::{BoundEvidence, Rule};
use crate::valid_reference;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementMode {
    Weighted,
    Priority,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddressRouting {
    pub mode: PlacementMode,
    pub salt: String,
    /// Canonical vendor order for weighted placement; priority order for ordered placement.
    pub roster: Vec<ApprovedRoute>,
    #[serde(default)]
    pub replace_ambiguous: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedRoute {
    pub provider: String,
    pub method: String,
    pub weight: u8,
    pub min_confidence: f64,
    pub max_proof_age_days: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteAvailability {
    pub blocked: bool,
    pub probe: bool,
    pub probe_generation: Option<i64>,
    pub share: Option<u8>,
}

pub struct RouteChoice<'a> {
    pub provider: &'a str,
    pub reason: &'static str,
}

impl AddressRouting {
    pub(super) fn validate(&self, rule: &Rule) -> bool {
        rule.kind == crate::CheckKind::Address
            && valid_reference(&self.salt)
            && self.roster.len() == 2
            && self.roster[0].provider != self.roster[1].provider
            && (self.mode == PlacementMode::Priority
                || self.roster[0].provider < self.roster[1].provider)
            && self.roster.iter().map(|r| u32::from(r.weight)).sum::<u32>() == 100
            && self.roster.iter().all(|r| {
                matches!(r.provider.as_str(), "address-a" | "address-b")
                    && r.weight > 0
                    && rule.methods.contains(&r.method)
                    && (0.0..=100.0).contains(&r.min_confidence)
                    && r.max_proof_age_days > 0
            })
    }

    pub fn approves(&self, evidence: &BoundEvidence) -> bool {
        self.roster
            .iter()
            .any(|r| r.provider == evidence.provider && r.method == evidence.method)
    }

    /// FNV-1a 64 over UTF-8 salt, NUL separator and requirement identity.
    pub fn bucket(&self, requirement: &str) -> u8 {
        let hash = self
            .salt
            .bytes()
            .chain([0])
            .chain(requirement.bytes())
            .fold(0xcbf29ce484222325u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
            });
        (hash % 100) as u8
    }

    pub fn select<'a>(
        &'a self,
        eligible: &[String],
        limits: &BTreeMap<String, RouteAvailability>,
        bucket: u8,
    ) -> Option<RouteChoice<'a>> {
        let eligible_route = |i: usize| eligible.contains(&self.roster[i].provider);
        let nominal =
            usize::from(self.mode == PlacementMode::Weighted && bucket >= self.roster[0].weight);
        // A half-open probe is independent of the ordinary recovery share. SQL leases one token.
        for (i, route) in self.roster.iter().enumerate() {
            if eligible_route(i)
                && limits
                    .get(&route.provider)
                    .is_some_and(|l| l.probe && !l.blocked)
            {
                return Some(RouteChoice {
                    provider: &route.provider,
                    reason: "recovery_probe",
                });
            }
        }
        for i in [nominal, 1 - nominal] {
            let route = &self.roster[i];
            let position = if route.provider == "address-a" {
                bucket
            } else {
                99 - bucket
            };
            if !eligible_route(i)
                || limits
                    .get(&route.provider)
                    .is_some_and(|l| l.blocked || l.share.is_some_and(|cap| position >= cap))
            {
                continue;
            }
            let reason = if eligible.len() == 1 {
                "single_eligible"
            } else if i != nominal {
                "outage_diversion"
            } else if self.mode == PlacementMode::Priority {
                "priority"
            } else {
                "weighted"
            };
            return Some(RouteChoice {
                provider: &route.provider,
                reason,
            });
        }
        None
    }
}

use super::{BindingSnapshot, CheckStatus, ProductCase};
use crate::{CheckKind, Observation};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutedAnswer {
    pub operation_id: String,
    pub status: String,
    pub binding: BoundEvidence,
    pub observation: Option<Observation>,
    pub observed_at: Option<DateTime<Utc>>,
    pub received_at: Option<DateTime<Utc>>,
}

impl BindingSnapshot {
    /// Keep issuer alternatives only for checks whose pinned policy uses them.
    pub fn for_policy(&mut self, policy: &super::Policy) {
        self.provider_candidates.retain(|kind, _| {
            policy.checks.iter().any(|r| {
                r.kind == *kind
                    && (r.routing.is_some()
                        || (*kind == CheckKind::Sanctions && policy.strict_screen.is_some()))
            })
        });
    }
}

impl ProductCase {
    pub fn routed_address(&self) -> bool {
        self.policy
            .checks
            .iter()
            .any(|r| r.kind == CheckKind::Address && r.routing.is_some())
    }

    pub fn block_route(&mut self, kind: CheckKind, reason: &str, now: DateTime<Utc>) {
        let check = self.checks.get_mut(&kind).expect("pinned check");
        if check.status == CheckStatus::Blocked && check.reasons == [reason] {
            return;
        }
        check.status = CheckStatus::Blocked;
        check.reasons = vec![reason.to_owned()];
        self.advance(now);
        self.revision += 1;
    }

    pub fn replace_address(&mut self, op: &str, now: DateTime<Utc>) -> bool {
        let Some(check) = self.checks.get_mut(&CheckKind::Address) else {
            return false;
        };
        if check.status != CheckStatus::Pending
            || check.operation_id.as_deref() != Some(op)
            || self.decided_at.is_some()
        {
            return false;
        }
        let Some(asset) = check.evidence.clone() else {
            return false;
        };
        check.start(&self.case_id, asset);
        self.advance(now);
        self.revision += 1;
        true
    }

    pub fn settle_address(
        &mut self,
        answers: &[RoutedAnswer],
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        if self.is_terminal() || !self.routed_address() {
            return Ok(());
        }
        let check = &self.checks[&CheckKind::Address];
        let Some(last) = answers.last() else {
            return Ok(());
        };
        if !matches!(check.status, CheckStatus::Blocked | CheckStatus::Pending)
            || check.operation_id.as_ref() != Some(&last.operation_id)
        {
            return Ok(());
        }
        if answers
            .iter()
            .any(|a| matches!(a.status.as_str(), "outstanding" | "assigned"))
        {
            if matches!(
                last.status.as_str(),
                "complete" | "not_accepted" | "inconclusive"
            ) {
                self.block_route(CheckKind::Address, "ADDRESS_ORIGINAL_OUTSTANDING", now);
            }
            return Ok(());
        }
        let rule = self
            .policy
            .checks
            .iter()
            .find(|r| r.kind == CheckKind::Address)
            .expect("routed rule");
        let complete: Vec<_> = answers.iter().filter(|a| a.status == "complete").collect();
        let Some(chosen) = complete.last() else {
            self.block_route(CheckKind::Address, "ADDRESS_RESULT_UNAVAILABLE", now);
            return Ok(());
        };
        let assessment = |a: &RoutedAnswer| {
            if a.observed_at > a.received_at {
                Some(CheckStatus::Blocked)
            } else {
                a.observation
                    .as_ref()
                    .map(|v| rule.assess(&a.binding, v, now).status)
            }
        };
        if answers.iter().any(|a| a.status == "inconclusive")
            || complete.iter().any(|a| assessment(a) != assessment(chosen))
        {
            let check = self
                .checks
                .get_mut(&CheckKind::Address)
                .expect("routed address");
            check.status = CheckStatus::Review;
            check.reasons = vec!["CONFLICTING_ADDRESS_FINDINGS".into()];
            self.advance(now);
            self.revision += 1;
            return Ok(());
        }
        let (Some(observation), Some(observed), Some(received)) =
            (&chosen.observation, chosen.observed_at, chosen.received_at)
        else {
            return Err("authoritative address result lacks observation or timestamp".into());
        };
        let check = self
            .checks
            .get_mut(&CheckKind::Address)
            .expect("routed address");
        check.evidence = Some(chosen.binding.clone());
        check.operation_id = Some(chosen.operation_id.clone());
        check.status = CheckStatus::Pending;
        self.record_received(
            CheckKind::Address,
            &chosen.operation_id,
            observation.clone(),
            observed,
            Some(received),
            now,
        )
    }
}

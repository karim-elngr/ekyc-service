use super::Rule;
use crate::{valid_reference, CheckKind, Observation};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundEvidence {
    pub tenant_id: String,
    pub subject_ref: String,
    pub kind: CheckKind,
    pub evidence_ref: String,
    pub revision: String,
    pub provider: String,
    pub provider_ref: String,
    /// Adapter-selected delivery mode, pinned when the evidence is resolved.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub callback: bool,
    pub method: String,
    pub document_revision: Option<String>,
    pub portrait_revision: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingSnapshot {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub provider_candidates: std::collections::BTreeMap<CheckKind, Vec<BoundEvidence>>,
    pub current_document_revision: Option<String>,
    pub current_portrait_revision: Option<String>,
    pub screening: Option<BoundEvidence>,
}

impl BoundEvidence {
    /// Each immutable capture gets a distinct requirement, including issuers which restart
    /// revision numbers per asset. Escaping keeps opaque issuer delimiters unambiguous.
    pub fn requirement_id(&self, case: &str) -> String {
        let escape = |value: &str| {
            value
                .replace('%', "%25")
                .replace('/', "%2F")
                .replace('#', "%23")
        };
        let mut id = format!("{}/{}/{}", case, self.kind.as_str(), escape(&self.revision));
        if self.evidence_ref != self.revision {
            id.push('#');
            id.push_str(&escape(&self.evidence_ref));
        }
        id
    }

    pub fn same_capture(&self, other: &Self) -> bool {
        self.tenant_id == other.tenant_id
            && self.subject_ref == other.subject_ref
            && self.kind == other.kind
            && self.evidence_ref == other.evidence_ref
            && self.revision == other.revision
            && self.document_revision == other.document_revision
            && self.portrait_revision == other.portrait_revision
    }

    /// An issuer may rotate the vendor locator for an unchanged immutable asset.
    pub(super) fn same_screening_identity(&self, current: &Self) -> bool {
        self.evidence_ref == current.evidence_ref
            && self.revision == current.revision
            && self.document_revision == current.document_revision
            && self.provider == current.provider
            && self.method == current.method
    }

    pub(super) fn matches(
        &self,
        tenant: &str,
        subject: &str,
        rule: &Rule,
        bindings: &BindingSnapshot,
    ) -> bool {
        if self.tenant_id != tenant
            || self.subject_ref != subject
            || self.kind != rule.kind
            || !rule.methods.contains(&self.method)
            || rule.routing.as_ref().is_some_and(|r| !r.approves(self))
            || ![
                &self.evidence_ref,
                &self.revision,
                &self.provider,
                &self.provider_ref,
            ]
            .iter()
            .all(|s| valid_reference(s))
            || ![&self.document_revision, &self.portrait_revision]
                .into_iter()
                .flatten()
                .all(|s| valid_reference(s))
        {
            return false;
        }
        let matches_doc = self.document_revision.is_some()
            && self.document_revision == bindings.current_document_revision;
        let matches_portrait = self.portrait_revision.is_some()
            && self.portrait_revision == bindings.current_portrait_revision;
        match self.kind {
            CheckKind::Document => {
                matches_doc && (self.portrait_revision.is_none() || matches_portrait)
            }
            CheckKind::Face => matches_doc && matches_portrait,
            CheckKind::Address => true,
            CheckKind::Sanctions => matches_doc,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    /// Original provider evaluation under the source product policy.
    #[serde(default)]
    pub source_passed: bool,
    pub source_product: String,
    pub origin_case_id: String,
    pub operation_id: String,
    pub provider: String,
    pub observed_at: DateTime<Utc>,
    /// Absent on historical facts whose only clock was local HTTP receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_at: Option<DateTime<Utc>>,
    pub evidence: BoundEvidence,
    pub observation: Observation,
}

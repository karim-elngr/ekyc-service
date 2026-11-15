//! Product-scoped verification decisions. Reuse always refers to an original provider fact.
mod case;
mod check;
mod commands;
mod evidence;
mod policy;
mod view;
pub mod wire;

pub use case::ProductCase;
pub use check::{CaseState, CheckStatus, Origin, ProductCheck, ReviewAction, ReviewRecord};
pub use evidence::{BindingSnapshot, BoundEvidence, Fact};
pub use policy::{valid_slug, Policy, Reuse, Rule};
pub use view::{PublicCase, PublicCheck, PublicReason};
struct Assessment {
    status: CheckStatus,
    reasons: Vec<String>,
}

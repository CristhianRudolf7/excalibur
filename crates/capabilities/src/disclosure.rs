//! Outbound disclosure filter for agent-to-agent messages (§14.11).
//!
//! The policy per contact and purpose lists the categories that may be
//! shared. Default is deny: anything not authorized is withheld and requires
//! the user's permission. Credentials are never shared, whatever the policy
//! says. Commitments (payments, contracts, appointments) require explicit
//! authorization. Content is categorized by the caller (host or a classifier
//! outside this crate); this module only enforces the policy.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    General,
    Availability,
    Preferences,
    ContactDetails,
    Location,
    /// Private personal data.
    PrivateData,
    Memory,
    SensitiveDocument,
    Credential,
}
impl Category {
    /// Removed unless the policy names them explicitly (§14.11).
    pub fn is_protected(self) -> bool {
        matches!(
            self,
            Self::PrivateData | Self::Memory | Self::SensitiveDocument | Self::Credential
        )
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Commitment {
    Payment,
    Contract,
    Appointment,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisclosurePolicy {
    pub contact: String,
    pub purpose: String,
    pub allowed: BTreeSet<Category>,
    pub authorized_commitments: BTreeSet<Commitment>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Segment {
    pub category: Category,
    pub text: String,
    pub commitment: Option<Commitment>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WithheldReason {
    /// Credentials never leave through this channel.
    NeverShared,
    /// Not authorized by the policy for this contact and purpose.
    NeedsPermission,
    /// Would commit the user without explicit authorization.
    CommitmentNeedsAuthorization,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Withheld {
    pub index: usize,
    pub category: Category,
    pub reason: WithheldReason,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutboundDecision {
    /// Segments authorized to send, in their original order.
    pub send: Vec<Segment>,
    /// Withheld segments by index; their text is never echoed here.
    pub withheld: Vec<Withheld>,
    /// The host must ask the user before sending anything withheld for
    /// `NeedsPermission` or `CommitmentNeedsAuthorization`.
    pub requires_permission: bool,
}

/// Applies the policy matching `contact` and `purpose` exactly, if any.
pub fn filter_outbound(
    policies: &[DisclosurePolicy],
    contact: &str,
    purpose: &str,
    segments: Vec<Segment>,
) -> OutboundDecision {
    let policy = policies
        .iter()
        .find(|p| p.contact == contact && p.purpose == purpose);
    let mut decision = OutboundDecision {
        send: Vec::new(),
        withheld: Vec::new(),
        requires_permission: false,
    };
    for (index, segment) in segments.into_iter().enumerate() {
        let reason = if segment.category == Category::Credential {
            Some(WithheldReason::NeverShared)
        } else if !policy.is_some_and(|p| p.allowed.contains(&segment.category)) {
            Some(WithheldReason::NeedsPermission)
        } else if segment
            .commitment
            .is_some_and(|c| !policy.is_some_and(|p| p.authorized_commitments.contains(&c)))
        {
            Some(WithheldReason::CommitmentNeedsAuthorization)
        } else {
            None
        };
        match reason {
            None => decision.send.push(segment),
            Some(reason) => {
                decision.requires_permission |= reason != WithheldReason::NeverShared;
                decision.withheld.push(Withheld {
                    index,
                    category: segment.category,
                    reason,
                });
            }
        }
    }
    decision
}

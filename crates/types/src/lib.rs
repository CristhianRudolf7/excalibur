//! Portable identifiers shared by the core and its adapters.
pub mod runtime;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};
use thiserror::Error;
use ulid::Ulid;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid identifier")]
pub struct InvalidId;

macro_rules! identifier {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Ulid);
        impl $name {
            pub const PREFIX: &'static str = $prefix;
            pub fn new() -> Self {
                Self(Ulid::new())
            }
            /// Construct from an existing ULID, including one supplied by an adapter.
            pub fn from_ulid(id: Ulid) -> Self {
                Self(id)
            }
            pub fn ulid(self) -> Ulid {
                self.0
            }
            pub fn timestamp_ms(self) -> u64 {
                self.0.timestamp_ms()
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $prefix, self.0)
            }
        }
        impl FromStr for $name {
            type Err = InvalidId;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let tail = value.strip_prefix($prefix).ok_or(InvalidId)?;
                if tail.len() != 26 {
                    return Err(InvalidId);
                }
                let id = Ulid::from_string(tail).map_err(|_| InvalidId)?;
                // Canonical uppercase avoids aliases, ambiguous alphabet and overflow spellings.
                if id.to_string() != tail {
                    return Err(InvalidId);
                }
                Ok(Self(id))
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.to_string())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}
// Principal identifiers extend the listed entity prefixes for physical data isolation.
identifier!(PrincipalId, "usr_");
identifier!(ProjectId, "prj_");
identifier!(ConversationId, "cnv_");
identifier!(MessageId, "msg_");
identifier!(TaskId, "tsk_");
identifier!(RunId, "run_");
identifier!(AgentId, "agt_");
identifier!(RequirementId, "req_");
identifier!(CriterionId, "crt_");
identifier!(CheckId, "chk_");
identifier!(EvidenceId, "evd_");
identifier!(MemoryId, "mem_");
identifier!(SkillId, "skl_");
identifier!(JobId, "job_");

/// Project-scoped stable codes. Allocation belongs to a storage transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShortId {
    Task(u64),
    Requirement(u64),
    Criterion { requirement: u64, ordinal: u64 },
    Milestone(u64),
}
impl ShortId {
    pub fn validate(self) -> Result<Self, InvalidId> {
        match self {
            Self::Task(0)
            | Self::Requirement(0)
            | Self::Milestone(0)
            | Self::Criterion { requirement: 0, .. }
            | Self::Criterion { ordinal: 0, .. } => Err(InvalidId),
            _ => Ok(self),
        }
    }
}
impl fmt::Display for ShortId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Task(n) => write!(f, "T{n}"),
            Self::Requirement(n) => write!(f, "R{n}"),
            Self::Milestone(n) => write!(f, "M{n}"),
            Self::Criterion {
                requirement,
                ordinal,
            } => write!(f, "C{requirement}.{ordinal}"),
        }
    }
}
impl FromStr for ShortId {
    type Err = InvalidId;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        fn number(s: &str) -> Result<u64, InvalidId> {
            if s.starts_with('0') || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(InvalidId);
            }
            s.parse().map_err(|_| InvalidId)
        }
        let (kind, rest) = value.split_at_checked(1).ok_or(InvalidId)?;
        let id = match kind {
            "T" => Self::Task(number(rest)?),
            "R" => Self::Requirement(number(rest)?),
            "M" => Self::Milestone(number(rest)?),
            "C" => {
                let (requirement, ordinal) = rest.split_once('.').ok_or(InvalidId)?;
                Self::Criterion {
                    requirement: number(requirement)?,
                    ordinal: number(ordinal)?,
                }
            }
            _ => return Err(InvalidId),
        };
        id.validate()
    }
}
impl Serialize for ShortId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for ShortId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_are_typed_canonical_and_sortable_by_time() {
        let old = ProjectId::from_ulid(Ulid::from_parts(100, 1));
        let new = ProjectId::from_ulid(Ulid::from_parts(101, 0));
        assert!(old < new);
        assert!(old.to_string() < new.to_string());
        assert_eq!(old.timestamp_ms(), 100);
        assert_eq!(old.to_string().parse::<ProjectId>().unwrap(), old);
        assert!(old.to_string().parse::<TaskId>().is_err());
        assert!(
            "prj_0000000000000000000000000i"
                .parse::<ProjectId>()
                .is_err()
        );
        assert!(
            "prj_ZZZZZZZZZZZZZZZZZZZZZZZZZZ"
                .parse::<ProjectId>()
                .is_err()
        );
        let json = serde_json::to_string(&old).unwrap();
        assert_eq!(serde_json::from_str::<ProjectId>(&json).unwrap(), old);
        assert!(serde_json::from_str::<TaskId>(&json).is_err());
    }
    #[test]
    fn every_required_prefix_is_available() {
        let ids = [
            PrincipalId::new().to_string(),
            ProjectId::new().to_string(),
            ConversationId::new().to_string(),
            MessageId::new().to_string(),
            TaskId::new().to_string(),
            RunId::new().to_string(),
            AgentId::new().to_string(),
            RequirementId::new().to_string(),
            CriterionId::new().to_string(),
            CheckId::new().to_string(),
            EvidenceId::new().to_string(),
            MemoryId::new().to_string(),
            SkillId::new().to_string(),
            JobId::new().to_string(),
        ];
        let prefixes = [
            "usr_", "prj_", "cnv_", "msg_", "tsk_", "run_", "agt_", "req_", "crt_", "chk_", "evd_",
            "mem_", "skl_", "job_",
        ];
        for (id, prefix) in ids.iter().zip(prefixes) {
            assert!(id.starts_with(prefix));
        }
    }
    #[test]
    fn short_codes_reject_unstable_or_ambiguous_forms() {
        for text in ["T45", "R12", "C12.3", "M2"] {
            assert_eq!(text.parse::<ShortId>().unwrap().to_string(), text);
        }
        for text in [
            "T0",
            "T01",
            "C0.1",
            "C1.0",
            "C1.2.3",
            "T-2",
            "r2",
            "T18446744073709551616",
            "",
        ] {
            assert!(text.parse::<ShortId>().is_err(), "{text}");
        }
    }
}

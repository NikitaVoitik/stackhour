//! Newtype identifiers for the control-plane domain.
//!
//! `TaskId`, `RunId`, `EventId`, `CommandId`, and `ApprovalId` wrap a
//! [`uuid::Uuid`]; `NodeId` wraps a caller-chosen stable `String` (a laptop or
//! remote-development box picks its own durable name). Every id serializes as
//! its plain string form — no wrapper object — so the same spelling appears in
//! JSON payloads, DB `TEXT` columns, and log lines.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use stackhour_core::{Error, Result};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Declare a UUID-backed newtype id with the shared derives, a `new()`
/// random-v4 constructor, `Display`, `FromStr`, and transparent string serde.
macro_rules! uuid_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Mint a fresh random (v4) id.
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                $name(Uuid::new_v4())
            }

            /// The wrapped UUID.
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self> {
                Uuid::parse_str(s)
                    .map($name)
                    .map_err(|e| Error::msg(e.to_string()))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.collect_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                Uuid::parse_str(&raw).map($name).map_err(serde::de::Error::custom)
            }
        }
    };
}

uuid_id!(
    /// Durable identity of a user-visible unit of work. Survives engine
    /// restarts, node sleep, and client switching.
    TaskId
);
uuid_id!(
    /// One execution attempt of a task on one node/engine/access policy.
    RunId
);
uuid_id!(
    /// Stable identity of a durable event-log row. Node-originated events carry
    /// their own `EventId` so the hub can de-duplicate a retried delivery
    /// before it assigns a sequence.
    EventId
);
uuid_id!(
    /// Idempotency key a client attaches to every mutation. Replaying the same
    /// `CommandId` returns the original receipt instead of a second effect.
    CommandId
);
uuid_id!(
    /// Durable identity of an approval request.
    ApprovalId
);

/// Stable identity of an execution node. Unlike the UUID ids this is a
/// caller-chosen name (e.g. `"laptop"`, `"dev-box"`) that must stay constant
/// across reconnects; it serializes transparently as that string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    /// Borrow the underlying name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        NodeId(s.to_string())
    }
}

impl From<String> for NodeId {
    fn from(s: String) -> Self {
        NodeId(s)
    }
}

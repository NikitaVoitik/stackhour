//! stackhour-e2e — cross-crate end-to-end tests for the control-plane transport.
//!
//! This crate deliberately ships **no runtime code**. It exists only to host the
//! integration tests under `tests/` that drive a real [`stackhour_hub`] server
//! and a real [`stackhour_node`] client together over a loopback WebSocket, and
//! assert the Phase-1 transport acceptance criteria from
//! `docs/architecture/remote-agent-control-plane.md`.
//!
//! Those tests need to link *both* the hub and the node at once. Putting them in
//! either crate's own `tests/` directory would force that crate to dev-depend on
//! the other, and a hub `dev-dependency` on the node plus a node `dev-dependency`
//! on the hub is a dependency cycle Cargo rejects. A separate, dependency-free
//! test crate breaks the cycle: it dev-depends on both, and neither production
//! crate depends on it.
//!
//! There is nothing to call here — see `tests/transport.rs`.

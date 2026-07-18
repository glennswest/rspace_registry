//! `rspace-registry` library surface. Most of the registry is implemented
//! as a library so it can be exercised from integration tests without
//! standing up a real TCP listener.

pub mod auth;
pub mod error;
pub mod handlers;
pub mod k8s;
pub mod router;

pub use router::{build_router, AppState, Auth};

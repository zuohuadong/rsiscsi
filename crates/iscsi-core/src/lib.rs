//! Async, single-connection iSCSI initiator (RFC 7143, ERL 0).
//!
//! A socket-owning task pipelines commands up to the negotiated command window.
//! [`Session::read_pooled`] transfers a reusable owned buffer to that task; the
//! borrowed [`Session::read_into`] convenience API performs one final copy.
//! No block devices are created. Writes modify the remote LUN.

mod bench;
mod config;
mod error;
mod login;
mod pool;
mod scsi;
mod session;
mod stats;
pub mod wire;

pub use bench::*;
pub use config::*;
pub use error::Error;
pub use pool::{BufferPool, PooledBuffer};
pub use scsi::{Capacity, InquiryData};
pub use session::{Session, discover, discover_with_chap, login};
pub use stats::{SessionStats, StatsSnapshot};

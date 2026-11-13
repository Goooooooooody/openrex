//! Client for the Rex server API (control and data connections).
//!
//! The wire protocol is the one the server documents about itself at
//! `GET /llms.txt`: JSON text messages on `/api/control` (`rex.control.v1`)
//! and binary streams on `/api/data` (`rex.data.terminal.v1`).
//!
//! The crate is runtime agnostic. Sockets use `async-io`, so the futures can
//! be driven by any executor.

pub mod control;
pub mod data;
pub mod endpoint;
pub mod snapshot;
pub mod stream;
pub mod types;

pub use control::{Control, ControlEvent, Hello, RpcError, Welcome};
pub use data::{DataConn, DataError};
pub use endpoint::Endpoint;
pub use snapshot::{Framed, SnapshotFramer};
pub use stream::{RexMessage, StreamItem, StreamSplitter};

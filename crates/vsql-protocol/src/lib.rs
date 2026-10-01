//! PostgreSQL Wire Protocol v3 for VelocitySQL.
//!
//! The crate is split so that the protocol logic is testable without a socket:
//!
//! * [`messages`] — pure encoder/decoder for frontend and backend messages.
//! * [`types`] — value encoding (text format) and `pg_type.typlen` widths.
//! * [`server`] — the async accept loop and per-connection state machine.
//!
//! # Compatibility
//!
//! `psql` works out of the box:
//!
//! ```text
//! $ cargo run -p vsql-server -- --port 5210
//! $ psql -h 127.0.0.1 -p 5210 -U velocitysql
//! ```
//!
//! TLS is refused with the protocol's `'N'` answer (legal, and what makes
//! `psql` continue in plaintext); `pg_catalog` is still a milestone M6 item, so
//! `\d` and friends will report that the catalog relations are missing.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod messages;
pub mod server;
pub mod types;

pub use messages::{
    authentication_ok, backend_key_data, bind_complete, close_complete, command_complete, data_row,
    empty_query_response, error_response, execute_result_messages, no_data, parameter_description,
    parameter_status, parse_complete, parse_frontend, parse_startup, protocol_error_response,
    query_result_messages, ready_for_query, row_description, transaction_status,
    unsupported_response, Buffer, DescribeTarget, Frontend, ProtocolError, Startup,
    CANCEL_REQUEST_CODE, GSSENC_REQUEST_CODE, PROTOCOL_V3, SSL_REQUEST_CODE,
};
pub use server::{bind, serve, serve_connection, AuthMethod, ServerConfig};

//! VelocitySQL's planner and executor.
//!
//! # Pipeline
//!
//! ```text
//! SQL text ──► vsql-parser ──► Planner ──► Plan ──► volcano operators ──► rows
//!                                 │
//!                                 └── catalog (names, types) + storage (rows, indexes)
//! ```
//!
//! * [`planner`] resolves names, types literals, extracts aggregates and builds a
//!   [`plan::Plan`].
//! * [`exec`] turns a plan into a tree of pull-based operators.
//! * [`session::Session`] owns statement dispatch: DDL and DML are executed
//!   directly against the catalog and the storage engine, queries go through the
//!   planner.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use vsql_executor::{Engine, QueryResult, Session};
//!
//! let engine = Arc::new(Engine::new());
//! let mut session = Session::new(engine);
//!
//! session
//!     .execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL)")
//!     .unwrap();
//! session
//!     .execute("INSERT INTO users VALUES (1, 'ada'), (2, 'grace')")
//!     .unwrap();
//!
//! let results = session
//!     .execute("SELECT name FROM users WHERE id = 2")
//!     .unwrap();
//! let QueryResult::Rows { rows, .. } = &results[0] else {
//!     panic!("expected rows");
//! };
//! assert_eq!(rows.len(), 1);
//! ```
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod catalog_scan;
pub mod database;
pub mod error;
pub mod exec;
pub mod explain;
pub mod expr;
pub mod field;
pub mod functions;
pub mod persist;
pub mod plan;
pub mod planner;
pub mod render;
pub mod session;

pub use database::{
    Database, Engine, PersistHook, COMPAT_DATABASE, DEFAULT_DATABASE, DEFAULT_OWNER,
    DEFAULT_PASSWORD, DEFAULT_SUPERUSER,
};
pub use error::ExecError;
pub use expr::ScalarExpr;
pub use field::{Field, Schema};
pub use plan::{Plan, ScanPlan};
pub use planner::Planner;
pub use session::{QueryResult, Session};

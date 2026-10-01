//! PostgreSQL Wire Protocol v3 message codec.
//!
//! # Framing
//!
//! The protocol has two framing shapes, and the only thing that tells them
//! apart is *when* a message arrives:
//!
//! * **Startup phase** — a bare `Int32` length followed by the payload, with no
//!   type byte. The very first message on a connection is always this shape.
//! * **Regular phase** — `Byte1` type tag, `Int32` length, payload. The length
//!   includes itself but excludes the tag.
//!
//! Because the shapes are ambiguous on their own, the server tracks the phase
//! ([`crate::server`]) and calls [`parse_startup`] or [`parse_frontend`]
//! accordingly. Keeping the decision in the server — rather than guessing from
//! the bytes — is what makes the framing correct rather than heuristic.
//!
//! Everything here is a pure function over byte slices so the codec can be unit
//! tested without a socket.

use vsql_executor::{ExecError, Field, QueryResult};
use vsql_types::{DataType, Value};

use crate::types;

/// Protocol version 3.0, the only one VelocitySQL speaks.
pub const PROTOCOL_V3: i32 = 196_608;
/// Magic code of an `SSLRequest`.
pub const SSL_REQUEST_CODE: i32 = 80_877_103;
/// Magic code of a `GSSENCRequest`.
pub const GSSENC_REQUEST_CODE: i32 = 80_877_104;
/// Magic code of a `CancelRequest`.
pub const CANCEL_REQUEST_CODE: i32 = 80_877_102;

/// The first message a client sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Startup {
    /// A normal connection request with the client's parameters.
    Connect {
        /// Protocol version the client asked for.
        version: i32,
        /// Startup parameters (`user`, `database`, `application_name`, ...).
        parameters: Vec<(String, String)>,
    },
    /// The client asks to negotiate TLS before sending a startup message.
    SslRequest,
    /// The client asks to negotiate GSSAPI encryption.
    GssEncRequest,
    /// The client asks to cancel a running query.
    CancelRequest {
        /// Backend process id from `BackendKeyData`.
        pid: i32,
        /// Secret key from `BackendKeyData`.
        secret: i32,
    },
}

/// A message sent by a connected client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frontend {
    /// `Query`: run one or more SQL statements (simple query protocol).
    Query(String),
    /// `Parse`: prepare a statement.
    Parse {
        /// Statement name; empty means the unnamed statement.
        name: String,
        /// SQL text.
        query: String,
        /// Parameter type OIDs, when the client supplied them.
        param_types: Vec<u32>,
    },
    /// `Bind`: create a portal from a prepared statement.
    Bind {
        /// Portal name; empty means the unnamed portal.
        portal: String,
        /// Source statement name.
        statement: String,
        /// Format of each parameter (0 text, 1 binary); empty means all text,
        /// and a single entry applies to every parameter.
        parameter_formats: Vec<i16>,
        /// Supplied parameter values.
        parameters: Vec<Option<Vec<u8>>>,
        /// Requested result formats.
        result_formats: Vec<i16>,
    },
    /// `Describe`: ask about a statement or a portal.
    Describe {
        /// Whether a statement or a portal is being described.
        target: DescribeTarget,
        /// Name of the target.
        name: String,
    },
    /// `Execute`: run a portal.
    Execute {
        /// Portal name.
        portal: String,
        /// Maximum rows to return; `0` means "all".
        max_rows: i32,
    },
    /// `Close`: drop a statement or a portal.
    Close {
        /// Whether a statement or a portal is being closed.
        target: DescribeTarget,
        /// Name of the target.
        name: String,
    },
    /// `Sync`: end an extended-protocol exchange.
    Sync,
    /// `Flush`: push pending output to the client.
    Flush,
    /// `Terminate`: the client is done.
    Terminate,
    /// A `p` message: a `PasswordMessage`, a `SASLInitialResponse` or a
    /// `SASLResponse`.
    ///
    /// The three share a type byte and are told apart by the authentication
    /// state, so the raw bytes are handed to the caller and interpreted there.
    PasswordMessage(Vec<u8>),
    /// A type tag this version does not implement.
    Unknown(u8),
}

/// Whether a `Describe`/`Close` message targets a statement or a portal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescribeTarget {
    /// A prepared statement (`'S'`).
    Statement,
    /// A portal (`'P'`).
    Portal,
}

/// A decoder failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// The message ended before the declared fields were read.
    #[error("truncated {message} message: expected {expected} bytes")]
    Truncated {
        /// Message name, for the log.
        message: &'static str,
        /// How many bytes were expected.
        expected: usize,
    },
    /// A length field was negative or absurd.
    #[error("invalid {message} message length {length}")]
    InvalidLength {
        /// Message name.
        message: &'static str,
        /// The offending length.
        length: usize,
    },
    /// A text field was not valid UTF-8.
    #[error("invalid UTF-8 in {message} message")]
    InvalidUtf8 {
        /// Message name.
        message: &'static str,
    },
}

/// A cursor over a message payload.
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
    message: &'static str,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], message: &'static str) -> Self {
        Reader {
            bytes,
            position: 0,
            message,
        }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ProtocolError> {
        if self.remaining() < count {
            return Err(ProtocolError::Truncated {
                message: self.message,
                expected: count,
            });
        }
        let slice = &self.bytes[self.position..self.position + count];
        self.position += count;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn i16(&mut self) -> Result<i16, ProtocolError> {
        let bytes = self.take(2)?;
        Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn i32(&mut self) -> Result<i32, ProtocolError> {
        let bytes = self.take(4)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads the remainder of the payload.
    fn tail(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.position..];
        self.position = self.bytes.len();
        rest
    }

    /// Reads a NUL-terminated string.
    fn cstr(&mut self) -> Result<String, ProtocolError> {
        let rest = &self.bytes[self.position..];
        let Some(end) = rest.iter().position(|byte| *byte == 0) else {
            return Err(ProtocolError::Truncated {
                message: self.message,
                expected: 1,
            });
        };
        let text = std::str::from_utf8(&rest[..end]).map_err(|_| ProtocolError::InvalidUtf8 {
            message: self.message,
        })?;
        self.position += end + 1;
        Ok(text.to_string())
    }
}

/// Parses the payload of a startup-phase message (everything after the length).
pub fn parse_startup(payload: &[u8]) -> Result<Startup, ProtocolError> {
    let mut reader = Reader::new(payload, "startup");
    let code = reader.i32()?;
    match code {
        SSL_REQUEST_CODE => Ok(Startup::SslRequest),
        GSSENC_REQUEST_CODE => Ok(Startup::GssEncRequest),
        CANCEL_REQUEST_CODE => Ok(Startup::CancelRequest {
            pid: reader.i32()?,
            secret: reader.i32()?,
        }),
        version => {
            let mut parameters = Vec::new();
            while reader.remaining() > 1 {
                let key = reader.cstr()?;
                if key.is_empty() {
                    break;
                }
                let value = reader.cstr()?;
                parameters.push((key, value));
            }
            Ok(Startup::Connect {
                version,
                parameters,
            })
        }
    }
}

/// Parses a `SASLInitialResponse` payload into its mechanism and data.
pub fn parse_sasl_initial(payload: &[u8]) -> Result<(String, Vec<u8>), ProtocolError> {
    let mut reader = Reader::new(payload, "SASLInitialResponse");
    let mechanism = reader.cstr()?;
    let length = reader.i32()?;
    let data = if length < 0 {
        Vec::new()
    } else {
        reader.take(length as usize)?.to_vec()
    };
    Ok((mechanism, data))
}

/// Parses a regular frontend message.
pub fn parse_frontend(tag: u8, payload: &[u8]) -> Result<Frontend, ProtocolError> {
    let mut reader = Reader::new(payload, "frontend");
    let message = match tag {
        b'Q' => Frontend::Query(reader.cstr()?),
        b'P' => {
            let name = reader.cstr()?;
            let query = reader.cstr()?;
            let count = reader.i16()?;
            let mut param_types = Vec::with_capacity(count.max(0) as usize);
            for _ in 0..count {
                param_types.push(reader.i32()? as u32);
            }
            Frontend::Parse {
                name,
                query,
                param_types,
            }
        }
        b'B' => {
            let portal = reader.cstr()?;
            let statement = reader.cstr()?;
            let format_count = reader.i16()?;
            let mut parameter_formats = Vec::with_capacity(format_count.max(0) as usize);
            for _ in 0..format_count {
                parameter_formats.push(reader.i16()?);
            }
            let parameter_count = reader.i16()?;
            let mut parameters = Vec::with_capacity(parameter_count.max(0) as usize);
            for _ in 0..parameter_count {
                let length = reader.i32()?;
                parameters.push(if length < 0 {
                    None
                } else {
                    Some(reader.take(length as usize)?.to_vec())
                });
            }
            let result_count = reader.i16()?;
            let mut result_formats = Vec::with_capacity(result_count.max(0) as usize);
            for _ in 0..result_count {
                result_formats.push(reader.i16()?);
            }
            Frontend::Bind {
                portal,
                statement,
                parameter_formats,
                parameters,
                result_formats,
            }
        }
        b'D' => Frontend::Describe {
            target: describe_target(reader.u8()?)?,
            name: reader.cstr()?,
        },
        b'E' => Frontend::Execute {
            portal: reader.cstr()?,
            max_rows: reader.i32()?,
        },
        b'C' => Frontend::Close {
            target: describe_target(reader.u8()?)?,
            name: reader.cstr()?,
        },
        b'S' => Frontend::Sync,
        b'H' => Frontend::Flush,
        b'X' => Frontend::Terminate,
        b'p' => Frontend::PasswordMessage(reader.tail().to_vec()),
        other => Frontend::Unknown(other),
    };
    Ok(message)
}

fn describe_target(tag: u8) -> Result<DescribeTarget, ProtocolError> {
    match tag {
        b'S' => Ok(DescribeTarget::Statement),
        b'P' => Ok(DescribeTarget::Portal),
        _ => Err(ProtocolError::InvalidLength {
            message: "describe target",
            length: tag as usize,
        }),
    }
}

// ------------------------------------------------------------------ encoding

/// A big-endian byte sink.
#[derive(Debug, Default, Clone)]
pub struct Buffer {
    bytes: Vec<u8>,
}

impl Buffer {
    /// Creates an empty buffer.
    pub fn new() -> Self {
        Buffer::default()
    }

    /// Creates an empty buffer that can take `capacity` bytes without growing.
    pub fn with_capacity(capacity: usize) -> Self {
        Buffer {
            bytes: Vec::with_capacity(capacity),
        }
    }

    /// Appends a byte.
    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.bytes.push(value);
        self
    }

    /// Appends a big-endian `i16`.
    pub fn i16(&mut self, value: i16) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Appends a big-endian `i32`.
    pub fn i32(&mut self, value: i32) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Appends raw bytes.
    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.bytes.extend_from_slice(value);
        self
    }

    /// Appends a NUL-terminated string.
    pub fn cstr(&mut self, value: &str) -> &mut Self {
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
        self
    }

    /// Number of bytes written so far.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// `true` when nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Consumes the buffer.
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

/// Wraps a payload in `Byte1` tag + `Int32` length framing.
fn frame(tag: u8, body: &Buffer) -> Vec<u8> {
    let mut out = Buffer::with_capacity(body.len() + 5);
    out.u8(tag);
    out.i32(body.len() as i32 + 4);
    out.bytes(&body.bytes);
    out.into_vec()
}

/// `AuthenticationOk`.
pub fn authentication_ok() -> Vec<u8> {
    let mut body = Buffer::new();
    body.i32(0);
    frame(b'R', &body)
}

/// `ParameterStatus`.
pub fn parameter_status(key: &str, value: &str) -> Vec<u8> {
    let mut body = Buffer::new();
    body.cstr(key).cstr(value);
    frame(b'S', &body)
}

/// `AuthenticationSASL`: the list of mechanisms the server accepts.
pub fn authentication_sasl(mechanisms: &[&str]) -> Vec<u8> {
    let mut body = Buffer::new();
    body.i32(10);
    for mechanism in mechanisms {
        body.cstr(mechanism);
    }
    // The list is terminated by an extra zero byte.
    body.u8(0);
    frame(b'R', &body)
}

/// `AuthenticationSASLContinue`, carrying the server-first message.
pub fn authentication_sasl_continue(data: &str) -> Vec<u8> {
    let mut body = Buffer::new();
    body.i32(11);
    body.bytes(data.as_bytes());
    frame(b'R', &body)
}

/// `AuthenticationSASLFinal`, carrying the server signature.
pub fn authentication_sasl_final(data: &str) -> Vec<u8> {
    let mut body = Buffer::new();
    body.i32(12);
    body.bytes(data.as_bytes());
    frame(b'R', &body)
}

/// `BackendKeyData`, used by clients to send a later `CancelRequest`.
pub fn backend_key_data(pid: i32, secret: i32) -> Vec<u8> {
    let mut body = Buffer::new();
    body.i32(pid).i32(secret);
    frame(b'K', &body)
}

/// `ReadyForQuery` with `'I'`, `'T'` or `'E'`.
pub fn ready_for_query(status: u8) -> Vec<u8> {
    let mut body = Buffer::new();
    body.u8(status);
    frame(b'Z', &body)
}

/// `RowDescription` for a result schema.
///
/// Table OID and attribute number are reported as `0`: VelocitySQL's result
/// columns come from expressions, not from a single relation, and drivers treat
/// `0` as "not a simple table column".
pub fn row_description(schema: &[Field]) -> Vec<u8> {
    let mut body = Buffer::new();
    body.i16(schema.len() as i16);
    for field in schema {
        body.cstr(&field.name);
        body.i32(0);
        body.i16(0);
        body.i32(field.data_type.oid() as i32);
        body.i16(types::type_len(&field.data_type));
        body.i32(-1);
        body.i16(0);
    }
    frame(b'T', &body)
}

/// `DataRow`, encoding each column in the format the portal asked for.
///
/// `formats` is what `Bind` requested: empty means text for every column, a
/// single entry applies to all of them, and one entry per column is applied in
/// order. A type with no binary representation is reported rather than sent in
/// bytes the client would misread.
pub fn data_row(row: &[Value], schema: &[Field], formats: &[i16]) -> Result<Vec<u8>, ExecError> {
    let mut body = Buffer::new();
    body.i16(row.len() as i16);
    for (index, value) in row.iter().enumerate() {
        let binary = match formats.len() {
            0 => false,
            1 => formats[0] == 1,
            _ => formats.get(index).copied().unwrap_or(0) == 1,
        };
        let encoded = if binary {
            let data_type = schema
                .get(index)
                .map(|field| field.data_type.clone())
                .unwrap_or_else(|| value.type_of());
            types::encode_binary(value, &data_type).map_err(|data_type| {
                ExecError::not_supported(format!(
                    "binary result format for type {}",
                    data_type.format_type()
                ))
            })?
        } else {
            types::encode_text(value)
        };
        // A `NULL` is transmitted as a length of -1, an empty string as 0.
        match encoded {
            None => {
                body.i32(-1);
            }
            Some(encoded) => {
                body.i32(encoded.len() as i32);
                body.bytes(&encoded);
            }
        }
    }
    Ok(frame(b'D', &body))
}

/// `CommandComplete`.
pub fn command_complete(tag: &str) -> Vec<u8> {
    let mut body = Buffer::new();
    body.cstr(tag);
    frame(b'C', &body)
}

/// `EmptyQueryResponse`.
pub fn empty_query_response() -> Vec<u8> {
    frame(b'I', &Buffer::new())
}

/// `ParseComplete`.
pub fn parse_complete() -> Vec<u8> {
    frame(b'1', &Buffer::new())
}

/// `BindComplete`.
pub fn bind_complete() -> Vec<u8> {
    frame(b'2', &Buffer::new())
}

/// `CloseComplete`.
pub fn close_complete() -> Vec<u8> {
    frame(b'3', &Buffer::new())
}

/// `NoData`, the response to describing a statement that returns no rows.
pub fn no_data() -> Vec<u8> {
    frame(b'n', &Buffer::new())
}

/// `ParameterDescription`.
pub fn parameter_description(oids: &[u32]) -> Vec<u8> {
    let mut body = Buffer::new();
    body.i16(oids.len() as i16);
    for oid in oids {
        body.i32(*oid as i32);
    }
    frame(b't', &body)
}

/// `ErrorResponse`, built from an execution error.
///
/// The fields are what `psql` and every driver display: severity, SQLSTATE
/// (which drivers branch on) and the message. `POSITION` is forwarded when the
/// parser reported one, which is what makes syntax errors point at the offending
/// token.
pub fn error_response(error: &ExecError) -> Vec<u8> {
    let mut body = Buffer::new();
    body.u8(b'S').cstr("ERROR");
    body.u8(b'V').cstr("ERROR");
    body.u8(b'C').cstr(error.sqlstate());
    body.u8(b'M').cstr(&error.to_string());
    if let Some(position) = error.position() {
        body.u8(b'P').cstr(&position.to_string());
    }
    body.u8(0);
    frame(b'E', &body)
}

/// `ErrorResponse` for a protocol-level problem (not an SQL error).
pub fn protocol_error_response(message: &str) -> Vec<u8> {
    let mut body = Buffer::new();
    body.u8(b'S').cstr("ERROR");
    body.u8(b'V').cstr("ERROR");
    // `08P01` is `protocol_violation`; `0A000` is `feature_not_supported`.
    body.u8(b'C').cstr("08P01");
    body.u8(b'M').cstr(message);
    body.u8(0);
    frame(b'E', &body)
}

/// `FATAL` `ErrorResponse`, used when the connection cannot be established.
///
/// PostgreSQL reports an unusable database this way and then closes the socket,
/// *before* any `ParameterStatus` is sent, so clients can tell "wrong database"
/// apart from "server is up but the statement failed".
pub fn fatal_error_response(sqlstate: &str, message: &str) -> Vec<u8> {
    let mut body = Buffer::new();
    body.u8(b'S').cstr("FATAL");
    body.u8(b'V').cstr("FATAL");
    body.u8(b'C').cstr(sqlstate);
    body.u8(b'M').cstr(message);
    body.u8(0);
    frame(b'E', &body)
}

/// `ErrorResponse` for a recognised but unimplemented feature.
pub fn unsupported_response(message: &str) -> Vec<u8> {
    let mut body = Buffer::new();
    body.u8(b'S').cstr("ERROR");
    body.u8(b'V').cstr("ERROR");
    body.u8(b'C').cstr("0A000");
    body.u8(b'M').cstr(message);
    body.u8(0);
    frame(b'E', &body)
}

/// The `ReadyForQuery` status byte implied by a session's transaction state.
pub fn transaction_status(in_transaction: bool, failed: bool) -> u8 {
    if failed {
        b'E'
    } else if in_transaction {
        b'T'
    } else {
        b'I'
    }
}

/// Encodes the rows of a query result, i.e. `RowDescription` + `DataRow`*.
///
/// This is the shape the **simple** query protocol answers with, and the one a
/// `Describe` answers a statement with. An `Execute` must *not* use it: see
/// [`execute_result_messages`].
pub fn query_result_messages(
    result: &QueryResult,
    formats: &[i16],
) -> Result<Vec<Vec<u8>>, ExecError> {
    let mut messages = Vec::new();
    match result {
        QueryResult::Rows { schema, rows } => {
            check_result_formats(schema, formats)?;
            messages.push(row_description(schema));
            for row in rows {
                messages.push(data_row(row, schema, formats)?);
            }
        }
        QueryResult::Command { .. } => {}
    }
    Ok(messages)
}

/// Encodes the rows an `Execute` answers with: `DataRow`* only.
///
/// PostgreSQL never sends a `RowDescription` in reply to `Execute` — the header
/// belongs to `Describe`, which the client sent earlier (or which it must send).
/// Repeating it here is not harmless: `lib/pq` reads exactly `DataRow`,
/// `CommandComplete` or an error after `Execute` and aborts the statement with
/// `unexpected message during extended query execution: "(T) RowDescription"`
/// on anything else.
pub fn execute_result_messages(
    result: &QueryResult,
    formats: &[i16],
) -> Result<Vec<Vec<u8>>, ExecError> {
    let mut messages = Vec::new();
    if let QueryResult::Rows { schema, rows } = result {
        check_result_formats(schema, formats)?;
        for row in rows {
            messages.push(data_row(row, schema, formats)?);
        }
    }
    Ok(messages)
}

/// Rejects a `Bind` whose result formats do not fit the result's width.
///
/// The protocol allows three shapes: none (all text), one (applies to every
/// column), or one per column. Anything else means the client and the server
/// disagree about how wide the result is.
fn check_result_formats(schema: &[Field], formats: &[i16]) -> Result<(), ExecError> {
    if formats.len() > 1 && formats.len() != schema.len() {
        return Err(ExecError::other(format!(
            "Bind sent {} result formats for {} columns",
            formats.len(),
            schema.len()
        )));
    }
    Ok(())
}

/// The fixed-width `pg_type.typlen` of a type, or `-1` when it is variable.
pub fn type_len(data_type: &DataType) -> i16 {
    types::type_len(data_type)
}

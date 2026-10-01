//! The PostgreSQL Wire Protocol v3 server.
//!
//! # Connection lifecycle
//!
//! ```text
//! [StartupMessage | SSLRequest → 'N' → StartupMessage] → AuthenticationOk
//!   → ParameterStatus* → BackendKeyData → ReadyForQuery
//!   → {simple query | extended query}* → Terminate
//! ```
//!
//! Both query protocols are implemented:
//!
//! * **Simple** (`Query`) — used by `psql` when you type a statement. Responses
//!   stream result sets one `DataRow` at a time, then a `CommandComplete`.
//! * **Extended** (`Parse`/`Bind`/`Describe`/`Execute`/`Sync`) — used by
//!   `tokio-postgres`, `sqlx` and JDBC. Statements are prepared once and
//!   re-planned on each `Execute` so that a changed schema is picked up; the
//!   plan itself is never stale.
//!
//! # Error handling
//!
//! After a failed message in the extended protocol the server must ignore
//! everything until `Sync` (that is what the `sync_required` flag encodes).
//! Ignoring this rule is how servers end up desynchronised by a byte or two and
//! then "randomly" fail — the flag makes the rule explicit.
//!
//! # Bind parameters
//!
//! `Parse` reports how many parameters the statement uses through
//! `ParameterDescription`, and `Bind` supplies their values; `$1` is the first.
//! The values arrive in *text* format, which is what `lib/pq` sends, and are
//! read as the type each one is compared against (see `Planner::coerce_against`)
//! — `int_col = $1` with the text `1` compares integers, and an index probe
//! built from it carries the column's own type. A `Bind` whose parameter count
//! disagrees with the statement is refused, in PostgreSQL's words.
//!
//! # Result formats
//!
//! `Bind` chooses the format of each result column, and both are honoured:
//! `lib/pq` asks for **binary** on the types it can decode natively, so a
//! server that answered in text would hand it bytes it reads as garbage. The
//! encoders live in [`crate::types`]; a type without one (an array, so far) is
//! reported as unsupported instead.
//!
//! # Statement logging
//!
//! Every statement is logged at `info`, which the server's default filter shows:
//! a restart should not be needed to find out what a client is sending. A
//! *failure* is logged at `warn` with the SQL and the bound parameters — a
//! message like `column "x" does not exist` cannot be acted on when the log does
//! not say which statement produced it. The extended protocol used to log
//! nothing at all, which left a failing client's statements invisible here.
//!
//! Both carry `elapsed`, measured around the work the statement did: the log
//! answers *how long* as well as *what*. That is also why the line is written
//! once the statement has finished rather than as it starts — the duration is
//! only known then, and it is the durations that find the slow query.
//!
//! `RUST_LOG` still decides: `RUST_LOG=vsql_protocol=warn` keeps the warnings
//! and drops the statements, `RUST_LOG=debug` adds the connection-level detail.
//!
//! A role's `PASSWORD` is masked before it is written: the wire carries role
//! passwords in clear text (SCRAM protects the *login*, not the DDL), so a
//! verbatim statement log would leak them.
//!
//! # Deliberate limits (milestone M3)
//!
//! * No TLS: an `SSLRequest` is answered with `'N'`, which is a legal refusal
//!   that makes `psql` fall back to plaintext.
//! * No binary *parameter* format: reading one as text would store garbage, so
//!   it is refused with `feature not supported` instead.
//! * `Execute` with a row limit is refused: portals hold complete result sets,
//!   so a partial fetch cannot be honoured faithfully.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use vsql_catalog::{scram, CatalogError, ScramVerifier};
use vsql_executor::{Engine, ExecError, QueryResult, Schema, Session};
use vsql_types::Value;

use crate::messages::{self, DescribeTarget, Frontend, Startup, PROTOCOL_V3};

/// Largest startup payload accepted, to bound memory on a hostile connection.
const MAX_STARTUP_BYTES: usize = 10_000;
/// Largest regular message accepted.
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// How a connecting client must prove who it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthMethod {
    /// Accept any role that exists and may log in.
    ///
    /// Only safe when the socket is already protected (a loopback-only test, a
    /// unix socket behind another layer). It is never the default.
    Trust,
    /// SCRAM-SHA-256, PostgreSQL's default since v10.
    #[default]
    ScramSha256,
}

impl AuthMethod {
    /// Parses the `--auth` command line value.
    pub fn parse(value: &str) -> Option<AuthMethod> {
        match value.trim().to_ascii_lowercase().as_str() {
            "trust" => Some(AuthMethod::Trust),
            "scram-sha-256" | "scram" => Some(AuthMethod::ScramSha256),
            _ => None,
        }
    }

    /// The value as it appears in `--help`.
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthMethod::Trust => "trust",
            AuthMethod::ScramSha256 => "scram-sha-256",
        }
    }
}

/// Server settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Address to bind.
    pub host: String,
    /// Port to bind.
    pub port: u16,
    /// How clients authenticate.
    pub auth: AuthMethod,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 5210,
            // Password authentication by default: a server that accepts anyone
            // is a surprise nobody should get from a default.
            auth: AuthMethod::ScramSha256,
        }
    }
}

/// Binds the listener described by `config`.
pub async fn bind(config: &ServerConfig) -> std::io::Result<TcpListener> {
    TcpListener::bind((config.host.as_str(), config.port)).await
}

/// Serves connections until the listener fails.
///
/// Each connection is an independent task sharing the engine; `Session` state is
/// per connection, so nothing needs a lock except the catalog and the storage
/// engine, which are already internally synchronised.
pub async fn serve(
    engine: Arc<Engine>,
    listener: TcpListener,
    auth: AuthMethod,
) -> std::io::Result<()> {
    loop {
        let (socket, peer) = listener.accept().await?;
        let engine = engine.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_connection(engine, socket, auth).await {
                // A client that hangs up mid-message is normal, not an incident.
                debug!(%peer, %error, "connection closed with an error");
            }
        });
    }
}

/// Handles one client connection.
pub async fn serve_connection(
    engine: Arc<Engine>,
    socket: TcpStream,
    auth: AuthMethod,
) -> std::io::Result<()> {
    socket.set_nodelay(true)?;
    let (mut reader, mut writer) = socket.into_split();

    // ---------------------------------------------------------- startup phase
    let mut startup = read_startup(&mut reader).await?;
    if matches!(startup, Startup::SslRequest) {
        // Refusing TLS is signalled with a single 'N', after which the client
        // sends its real startup message over the plaintext connection.
        writer.write_all(b"N").await?;
        writer.flush().await?;
        startup = read_startup(&mut reader).await?;
    }
    let parameters = match startup {
        Startup::Connect {
            version,
            parameters,
        } => {
            if version != PROTOCOL_V3 {
                // An unsupported major version is reported *before* any
                // ParameterStatus, exactly like PostgreSQL does.
                let mut payload = messages::Buffer::new();
                payload.i32(PROTOCOL_V3);
                let mut error = messages::Buffer::new();
                error.u8(b'S').cstr("FATAL");
                error.u8(b'V').cstr("FATAL");
                error.u8(b'C').cstr("08P01");
                error.u8(b'M').cstr(&format!(
                    "unsupported frontend protocol {version}.{}.0: server supports 3.0",
                    version & 0xFFFF
                ));
                error.u8(0);
                writer.write_all(&error.into_vec()).await?;
                writer.flush().await?;
                return Ok(());
            }
            parameters
        }
        Startup::CancelRequest { pid, secret } => {
            // Query cancellation is not implemented yet; acknowledging silently
            // would be a lie, so it is logged instead.
            info!(
                pid,
                secret, "cancel request ignored (query cancellation is planned)"
            );
            return Ok(());
        }
        Startup::GssEncRequest => {
            writer.write_all(b"N").await?;
            writer.flush().await?;
            return Ok(());
        }
        Startup::SslRequest => unreachable!("handled above"),
    };

    let user = parameters
        .iter()
        .find(|(key, _)| key == "user")
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| "velocitysql".to_string());
    let application = parameters
        .iter()
        .find(|(key, _)| key == "application_name")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();

    // The startup packet names the database to attach to; when absent,
    // PostgreSQL falls back to the user name.
    let database = parameters
        .iter()
        .find(|(key, _)| key == "database")
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| user.clone());

    // Authentication comes first: only an authenticated client gets to hear
    // whether its database exists.
    let session =
        match authenticate(&mut reader, &mut writer, &engine, &user, &database, auth).await {
            Ok(session) => session,
            Err(AuthError::Denied(error)) => {
                let message = messages::fatal_error_response(error.sqlstate(), &error.to_string());
                writer.write_all(&message).await?;
                writer.flush().await?;
                return Ok(());
            }
            // A dead socket is a transport problem, not a rejected credential.
            Err(AuthError::Transport(error)) => return Err(error),
        };

    // `application_name` is the one parameter PostgreSQL accepts in the startup
    // packet; recording it here means `SHOW application_name` and the
    // `ParameterStatus` stream both report what the client announced.
    let mut session = session;
    session.set_parameter("application_name", application.clone());
    let mut connection = Connection::new(session);
    connection.send_startup_reply(&mut writer).await?;
    connection
        .run(&mut reader, &mut writer)
        .await
        .map_err(|error| {
            debug!(%error, "session ended");
            error
        })
}

/// What went wrong during authentication.
enum AuthError {
    /// The credential was rejected: answer with `FATAL` and close.
    Denied(ExecError),
    /// The socket failed; the connection is unusable either way.
    Transport(std::io::Error),
}

impl From<std::io::Error> for AuthError {
    fn from(error: std::io::Error) -> Self {
        AuthError::Transport(error)
    }
}

impl From<ExecError> for AuthError {
    fn from(error: ExecError) -> Self {
        AuthError::Denied(error)
    }
}

/// Authenticates the client and opens its session.
///
/// The database is validated here too, so the caller only has to turn a failure
/// into a `FATAL` error and close the socket.
async fn authenticate<R, W>(
    reader: &mut R,
    writer: &mut W,
    engine: &Arc<Engine>,
    user: &str,
    database: &str,
    auth: AuthMethod,
) -> Result<Session, AuthError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    match auth {
        AuthMethod::Trust => Ok(Session::connect(engine.clone(), database, user)?),
        AuthMethod::ScramSha256 => scram_exchange(reader, writer, engine, user, database).await,
    }
}

/// SCRAM-SHA-256 (RFC 5802, RFC 7677) as PostgreSQL speaks it.
///
/// # Why a decoy verifier
///
/// An unknown role, or one without a password, is answered with a challenge built
/// from a throwaway verifier instead of an error. The exchange is then
/// byte-for-byte the same shape as a genuine one and fails at the proof step with
/// the same message a wrong password produces, so the handshake cannot be used to
/// discover which accounts exist.
async fn scram_exchange<R, W>(
    reader: &mut R,
    writer: &mut W,
    engine: &Arc<Engine>,
    user: &str,
    database: &str,
) -> Result<Session, AuthError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    /// The one error a client may see from this exchange.
    fn denied(user: &str) -> AuthError {
        AuthError::Denied(ExecError::Catalog(
            CatalogError::PasswordAuthenticationFailed(user.to_string()),
        ))
    }

    let known = engine.role(user);
    let verifier = known
        .as_ref()
        .and_then(|role| role.verifier())
        .unwrap_or_else(|| ScramVerifier::new("decoy"));

    // 1. Offer the mechanism.
    writer
        .write_all(&messages::authentication_sasl(&[scram::MECHANISM]))
        .await?;
    writer.flush().await?;

    // 2. Read the client-first message.
    let initial = read_password_message(reader).await?;
    let (mechanism, data) = messages::parse_sasl_initial(&initial)
        .map_err(|error| AuthError::Denied(ExecError::other(error.to_string())))?;
    if mechanism != scram::MECHANISM {
        return Err(AuthError::Denied(ExecError::other(format!(
            "unsupported SASL mechanism `{mechanism}`"
        ))));
    }
    let client_first = String::from_utf8_lossy(&data).to_string();
    // The GS2 header ("n,," for no channel binding) is repeated in the client's
    // channel binding attribute, so it is kept for that check.
    // The GS2 header *includes* its trailing `,,`: that whole prefix is what the
    // client repeats, base64-encoded, in its `c=` channel binding attribute.
    let (gs2_header, client_first_bare) = client_first
        .split_once(",,")
        .ok_or_else(|| AuthError::Denied(ExecError::other("malformed SASL initial response")))?;
    let gs2_header = format!("{gs2_header},,");
    let client_nonce = scram::attribute(client_first_bare, "r")
        .ok_or_else(|| AuthError::Denied(ExecError::other("missing client nonce")))?
        .to_string();

    // 3. Send the server-first message: nonce, salt, iteration count.
    let combined_nonce = format!("{client_nonce}{}", scram::random_nonce());
    let server_first = format!(
        "r={combined_nonce},s={},i={}",
        scram::base64_encode(&verifier.salt),
        verifier.iterations
    );
    writer
        .write_all(&messages::authentication_sasl_continue(&server_first))
        .await?;
    writer.flush().await?;

    // 4. Read the client-final message and check the proof.
    let response = read_password_message(reader).await?;
    let client_final = String::from_utf8_lossy(&response).to_string();
    let (without_proof, proof) = client_final
        .split_once(",p=")
        .ok_or_else(|| AuthError::Denied(ExecError::other("missing client proof")))?;
    let proof = scram::base64_decode(proof)
        .ok_or_else(|| AuthError::Denied(ExecError::other("malformed client proof")))?;

    // The nonce must be echoed...
    if scram::attribute(without_proof, "r") != Some(combined_nonce.as_str()) {
        return Err(denied(user));
    }
    // ... and the channel binding must be the GS2 header we were given.
    let binding = scram::attribute(without_proof, "c").and_then(scram::base64_decode);
    if binding.as_deref() != Some(gs2_header.as_bytes()) {
        return Err(denied(user));
    }

    let auth_message = format!("{client_first_bare},{server_first},{without_proof}");

    let Some(signature) = verifier.verify_proof(&auth_message, &proof) else {
        return Err(denied(user));
    };
    // Reaching here means the proof matched; for the decoy path that is
    // impossible, and the role is `None` exactly then.
    let Some(role) = known else {
        return Err(denied(user));
    };

    // 5. Prove back that the server holds the verifier.
    //
    // `AuthenticationOk` is deliberately *not* sent here: `send_startup_reply`
    // owns it. Sending it twice is a protocol violation that libpq rejects with
    // `unexpected response from server; first received character was "R"`, because
    // it stops treating `R` as an authentication request once it has seen
    // `AuthenticationOk`.
    writer
        .write_all(&messages::authentication_sasl_final(&format!(
            "v={}",
            scram::base64_encode(&signature)
        )))
        .await?;
    writer.flush().await?;

    Ok(Session::with_role(engine.clone(), database, role)?)
}

/// Reads a `p` message, returning its raw payload.
async fn read_password_message<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<Vec<u8>, AuthError> {
    let denied = |message: &str| AuthError::Denied(ExecError::other(message.to_string()));
    let Some((tag, payload)) = read_message(reader).await? else {
        return Err(denied("client disconnected during authentication"));
    };
    match messages::parse_frontend(tag, &payload) {
        Ok(messages::Frontend::PasswordMessage(bytes)) => Ok(bytes),
        Ok(_) => Err(denied("expected an authentication message")),
        Err(error) => Err(denied(&error.to_string())),
    }
}
/// Per-connection state.
struct Connection {
    session: Session,
    /// Prepared statements, keyed by the name the client supplied.
    statements: HashMap<String, CachedStatement>,
    /// Portals, keyed by the name the client supplied.
    portals: HashMap<String, String>,
    /// Bind parameters captured at `Bind` time, keyed by portal name.
    portal_params: HashMap<String, Vec<Value>>,
    /// Result formats requested at `Bind` time, keyed by portal name.
    ///
    /// `lib/pq` asks for binary columns for the types it can decode natively,
    /// so the format list has to travel with the portal to `Execute`.
    portal_formats: HashMap<String, Vec<i16>>,
    /// Set after an extended-protocol error: skip messages until `Sync`.
    sync_required: bool,
    /// Set when a failure inside a transaction aborted it (status `'E'`).
    aborted: bool,
    /// The backend key data reported to the client.
    pid: i32,
    secret: i32,
}

struct CachedStatement {
    /// Result schema, `None` when the statement returns no rows.
    schema: Option<Schema>,
    /// The original SQL text, replayed with bind parameters by `execute`.
    sql: String,
    /// How many bind parameters the statement uses, reported by
    /// `ParameterDescription` so drivers can check the arguments they were
    /// handed. `lib/pq` refuses to run a statement whose count differs from the
    /// argument list (`got 3 parameters but the statement requires 0`).
    parameters: usize,
}

/// Renders bound parameters the way PostgreSQL numbers them: `$1=1, $2='x'`.
///
/// Each value is clipped: a log line is there to be read, not to archive a
/// document. `NULL` is spelled out because `Value`'s `Display` renders it as the
/// empty string, which reads like a missing log field.
fn render_parameters(params: &[Value]) -> String {
    /// Beyond this, a rendered parameter is cut short.
    const LIMIT: usize = 120;
    params
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let mut rendered = if value.is_null() {
                "NULL".to_string()
            } else {
                value.to_string()
            };
            if rendered.chars().count() > LIMIT {
                rendered = rendered.chars().take(LIMIT).collect::<String>() + "...";
            }
            format!("${}={rendered}", index + 1)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Masks the value of every `PASSWORD` clause in a statement about to be logged.
///
/// A role's password reaches the server in clear text in `CREATE`/`ALTER ROLE`
/// — only the *login* is protected by SCRAM — so logging the statement verbatim
/// would put every role's password in the log. Borrows the input when there is
/// nothing to mask, which is the common case.
fn redact_password(sql: &str) -> Cow<'_, str> {
    // Only role DDL can carry a password, so everything else is left exactly as
    // written: masking on a guess would rewrite `SELECT password FROM secrets`
    // into a statement that never ran.
    if !is_role_ddl(sql) {
        return Cow::Borrowed(sql);
    }
    const KEYWORD: &str = "password";
    let lower = sql.to_ascii_lowercase();
    let mut masked = String::new();
    let mut copied = 0usize;
    let mut search = 0usize;
    let mut found = false;
    while let Some(offset) = lower[search..].find(KEYWORD) {
        let start = search + offset;
        let end = start + KEYWORD.len();
        search = end;
        // Only a whole word: `my_password` is an identifier, not the clause.
        let before = sql[..start].chars().next_back();
        let after = sql[end..].chars().next();
        let boundary = |c: Option<char>| c.map_or(true, |c| !c.is_alphanumeric() && c != '_');
        if !(boundary(before) && boundary(after)) {
            continue;
        }
        let Some(value) = password_value(sql, end) else {
            continue;
        };
        masked.push_str(&sql[copied..value.start]);
        masked.push_str("'***'");
        copied = value.end;
        search = value.end;
        found = true;
    }
    if !found {
        return Cow::Borrowed(sql);
    }
    masked.push_str(&sql[copied..]);
    Cow::Owned(masked)
}

/// `true` for the statements that can carry a password: `CREATE`/`ALTER` of a
/// role or a user. Everything else is logged as written.
fn is_role_ddl(sql: &str) -> bool {
    let mut words = sql
        .split(|c: char| c.is_whitespace() || c == ';')
        .filter(|word| !word.is_empty());
    let verb = words.next().unwrap_or_default();
    let object = words.next().unwrap_or_default();
    (verb.eq_ignore_ascii_case("create") || verb.eq_ignore_ascii_case("alter"))
        && (object.eq_ignore_ascii_case("role") || object.eq_ignore_ascii_case("user"))
}

/// The span of the value following a `PASSWORD` keyword, or `None` when there is
/// no secret to mask (`PASSWORD NULL`, or nothing at all).
fn password_value(sql: &str, from: usize) -> Option<std::ops::Range<usize>> {
    let rest = &sql[from..];
    let start = from + (rest.len() - rest.trim_start().len());
    let tail = &sql[start..];
    if tail.len() >= 4 && tail[..4].eq_ignore_ascii_case("null") {
        return None;
    }
    if let Some(quoted) = tail.strip_prefix('\'') {
        // A single-quoted literal, whose `''` escapes must not end it early.
        let bytes = quoted.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() {
            if bytes[index] == b'\'' {
                if bytes.get(index + 1) == Some(&b'\'') {
                    index += 2;
                    continue;
                }
                return Some(start..start + index + 2);
            }
            index += 1;
        }
        // Unterminated: masking the rest of the statement is better than
        // printing a password that the parser will reject anyway.
        return Some(start..sql.len());
    }
    let end = tail
        .find(|c: char| c.is_whitespace() || c == ';' || c == ')')
        .unwrap_or(tail.len());
    Some(start..start + end)
}

/// Decodes an extended-protocol bind parameter; every one arrives as text.
///
/// The text alone does not say what it is: `{"a":1}` is a JSON object bound for
/// a `jsonb` column while `{1,2}` is an array literal bound for an `int[]` one,
/// and a driver is free to send either. Deciding here — as reading anything
/// brace-shaped as an array once did — reads the JSON as `text[]` and fails the
/// insert, so the value is kept as text and the *target type* decides what it
/// means (`Value::cast_to`, and the `= ANY($1)` evaluation path, which parses an
/// array literal only because its left operand is an array element).
fn decode_parameter(bytes: &[u8]) -> Value {
    Value::Text(String::from_utf8_lossy(bytes).into_owned())
}

/// Backend process ids handed out to clients, starting at a plausible value.
static NEXT_PID: AtomicI32 = AtomicI32::new(10_000);

impl Connection {
    /// Attaches a session to `database` as `user`.
    ///
    /// `Err` carries `3D000` when the database does not exist; the caller turns
    /// that into a `FATAL` error and closes the connection.
    fn new(session: Session) -> Self {
        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
        Connection {
            session,
            statements: HashMap::new(),
            portals: HashMap::new(),
            portal_params: HashMap::new(),
            portal_formats: HashMap::new(),
            sync_required: false,
            aborted: false,
            pid,
            // The secret is only meaningful for cancellation, which is not
            // implemented yet; a stable value keeps clients from crashing.
            secret: 0x5EED_1234,
        }
    }

    async fn send_startup_reply<W: AsyncWriteExt + Unpin>(
        &self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        let mut out = messages::authentication_ok();
        // The exact set of parameters PostgreSQL reports, so libpq-style clients
        // see a familiar startup. The values come from the session, so a role's
        // superuser status or a changed `TimeZone` is reflected honestly.
        for (name, value) in self.session.reported_parameters() {
            out.extend_from_slice(&messages::parameter_status(name, &value));
        }
        out.extend_from_slice(&messages::backend_key_data(self.pid, self.secret));
        out.extend_from_slice(&messages::ready_for_query(b'I'));
        writer.write_all(&out).await?;
        writer.flush().await
    }

    async fn run<R, W>(&mut self, reader: &mut R, writer: &mut W) -> std::io::Result<()>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        loop {
            let Some((tag, payload)) = read_message(reader).await? else {
                return Ok(()); // clean EOF
            };
            let message = match messages::parse_frontend(tag, &payload) {
                Ok(message) => message,
                Err(error) => {
                    let mut out = messages::protocol_error_response(&error.to_string());
                    out.extend_from_slice(&messages::ready_for_query(b'I'));
                    writer.write_all(&out).await?;
                    writer.flush().await?;
                    continue;
                }
            };

            // Once an extended-protocol exchange has failed, everything is
            // discarded until `Sync` resynchronises the stream.
            if self.sync_required {
                match message {
                    Frontend::Sync => {
                        self.sync_required = false;
                        self.send(writer, messages::ready_for_query(self.status()))
                            .await?;
                    }
                    Frontend::Terminate => return Ok(()),
                    _ => {}
                }
                continue;
            }

            let mut out: Vec<u8> = Vec::new();
            match message {
                Frontend::Query(sql) => {
                    if let Err(error) = self.simple_query(&sql, &mut out) {
                        out.clear();
                        out.extend_from_slice(&messages::error_response(&error));
                        self.note_failure();
                    }
                    out.extend_from_slice(&messages::ready_for_query(self.status()));
                }
                Frontend::Parse {
                    name,
                    query,
                    param_types: _,
                } => {
                    match self.prepare(&name, &query) {
                        Ok(()) => out.extend_from_slice(&messages::parse_complete()),
                        Err(error) => {
                            // Planning happens here in the extended protocol, so
                            // this is where a client's broken SQL shows up;
                            // `prepare` already logged it with SQL and duration.
                            out.extend_from_slice(&messages::error_response(&error));
                            self.sync_required = true;
                        }
                    }
                }
                Frontend::Bind {
                    portal,
                    statement,
                    parameter_formats,
                    parameters,
                    result_formats,
                } => {
                    if parameter_formats.iter().any(|format| *format != 0) {
                        // Reading a binary parameter as text would store garbage
                        // without ever failing, so it is refused instead.
                        out.extend_from_slice(&messages::unsupported_response(
                            "feature not supported: binary parameter format",
                        ));
                        self.sync_required = true;
                    } else if result_formats
                        .iter()
                        .any(|format| *format != 0 && *format != 1)
                    {
                        out.extend_from_slice(&messages::protocol_error_response(
                            "result format must be 0 (text) or 1 (binary)",
                        ));
                        self.sync_required = true;
                    } else if !self.statements.contains_key(&statement) {
                        out.extend_from_slice(&messages::protocol_error_response(&format!(
                            "prepared statement \"{statement}\" does not exist"
                        )));
                        self.sync_required = true;
                    } else {
                        // The statement exists, so its parameter count is known.
                        let required = self
                            .statements
                            .get(&statement)
                            .map(|cached| cached.parameters)
                            .unwrap_or_default();
                        if parameters.len() != required {
                            // PostgreSQL refuses a mismatch here rather than
                            // letting the statement fail later on an unbound
                            // `$n`, and this is its wording.
                            out.extend_from_slice(&messages::protocol_error_response(&format!(
                                "bind message supplies {} parameters, but prepared statement \"{statement}\" requires {required}",
                                parameters.len()
                            )));
                            self.sync_required = true;
                        } else {
                            let params: Vec<Value> = parameters
                                .iter()
                                .map(|p| match p {
                                    Some(bytes) => decode_parameter(bytes),
                                    None => Value::Null,
                                })
                                .collect();
                            self.portal_params.insert(portal.clone(), params);
                            self.portal_formats.insert(portal.clone(), result_formats);
                            self.portals.insert(portal, statement);
                            out.extend_from_slice(&messages::bind_complete());
                        }
                    }
                }
                Frontend::Describe { target, name } => match self.describe(target, &name) {
                    Ok(mut described) => out.append(&mut described),
                    Err(error) => {
                        out.extend_from_slice(&messages::error_response(&error));
                        self.sync_required = true;
                    }
                },
                Frontend::Execute { portal, max_rows } => {
                    if max_rows != 0 {
                        // Portals only ever hold complete result sets here, so a
                        // row limit cannot be honoured faithfully.
                        out.extend_from_slice(&messages::unsupported_response(
                            "feature not supported: partial portal execution",
                        ));
                        self.sync_required = true;
                    } else {
                        match self.execute(&portal) {
                            Ok(mut messages) => out.append(&mut messages),
                            Err(error) => {
                                out.extend_from_slice(&messages::error_response(&error));
                                self.sync_required = true;
                            }
                        }
                    }
                }
                Frontend::Close { target, name } => {
                    self.close(target, &name);
                    out.extend_from_slice(&messages::close_complete());
                }
                Frontend::Sync => {
                    out.extend_from_slice(&messages::ready_for_query(self.status()));
                }
                Frontend::Flush => {}
                Frontend::PasswordMessage(_) => {
                    // Only meaningful during startup, which has already finished.
                    out.extend_from_slice(&messages::protocol_error_response(
                        "unexpected password message",
                    ));
                    self.sync_required = true;
                }
                Frontend::Terminate => return Ok(()),
                Frontend::Unknown(tag) => {
                    out.extend_from_slice(&messages::protocol_error_response(&format!(
                        "unsupported frontend message '{}'",
                        char::from(tag)
                    )));
                    self.sync_required = true;
                }
            }

            if !out.is_empty() {
                writer.write_all(&out).await?;
                writer.flush().await?;
            }
        }
    }

    async fn send<W: AsyncWriteExt + Unpin>(
        &self,
        writer: &mut W,
        message: Vec<u8>,
    ) -> std::io::Result<()> {
        writer.write_all(&message).await?;
        writer.flush().await
    }

    /// The `ReadyForQuery` status byte for the current session state.
    fn status(&self) -> u8 {
        messages::transaction_status(self.session.in_transaction(), self.aborted)
    }

    /// Runs one or more statements from the simple query protocol.
    ///
    /// Answers in text format: a simple query has no `Bind` to request
    /// otherwise.
    fn simple_query(&mut self, sql: &str, out: &mut Vec<u8>) -> Result<(), ExecError> {
        if sql.trim().is_empty() || sql.trim() == ";" {
            out.extend_from_slice(&messages::empty_query_response());
            return Ok(());
        }
        let started = Instant::now();
        match self.session.execute(sql) {
            Ok(results) => {
                info!(
                    sql = %redact_password(sql),
                    elapsed = ?started.elapsed(),
                    "simple query"
                );
                if results.is_empty() {
                    out.extend_from_slice(&messages::empty_query_response());
                } else {
                    for result in &results {
                        for message in messages::query_result_messages(result, &[])? {
                            out.extend_from_slice(&message);
                        }
                        out.extend_from_slice(&messages::command_complete(&result.tag()));
                        self.note_transaction_state(result);
                    }
                }
            }
            Err(error) => {
                warn!(
                    %error,
                    sql = %redact_password(sql),
                    elapsed = ?started.elapsed(),
                    "statement failed"
                );
                out.extend_from_slice(&messages::error_response(&error));
                self.note_failure();
                // The error is reported here; the caller only adds
                // `ReadyForQuery`, exactly as it does for a successful query.
            }
        }
        // A `SET` may have changed a parameter the client caches.
        self.flush_parameter_status(out);
        Ok(())
    }

    /// Appends `ParameterStatus` for the parameters the last statement changed.
    ///
    /// PostgreSQL reports a parameter whenever it changes; libpq-based clients
    /// cache the values they saw at startup, so without this a `SET timezone`
    /// would silently disagree with what the driver believes.
    fn flush_parameter_status(&mut self, out: &mut Vec<u8>) {
        for (name, value) in self.session.take_parameter_updates() {
            out.extend_from_slice(&messages::parameter_status(&name, &value));
        }
    }

    /// Parses and plans `query`, caching it under `name`.
    ///
    /// `elapsed` covers both: in the extended protocol this call *is* the work
    /// `Parse` does, which is why a failure is logged here — the caller only
    /// reports it to the client.
    fn prepare(&mut self, name: &str, query: &str) -> Result<(), ExecError> {
        let started = Instant::now();
        // The parser's error is converted at the boundary so both steps share a
        // type, which is what lets one `match` log and report either failure.
        let planned = vsql_parser::parse_statement(query)
            .map_err(ExecError::from)
            .and_then(|statement| {
                let schema = self.session.describe(&statement)?;
                Ok((statement, schema))
            });
        match planned {
            Ok((statement, schema)) => {
                self.statements.insert(
                    name.to_string(),
                    CachedStatement {
                        schema,
                        sql: query.to_string(),
                        parameters: statement.parameter_count(),
                    },
                );
                info!(
                    statement = %name,
                    sql = %redact_password(query),
                    elapsed = ?started.elapsed(),
                    "parse"
                );
                Ok(())
            }
            Err(error) => {
                warn!(
                    %error,
                    statement = %name,
                    sql = %redact_password(query),
                    elapsed = ?started.elapsed(),
                    "statement failed"
                );
                Err(error)
            }
        }
    }

    fn describe(&mut self, target: DescribeTarget, name: &str) -> Result<Vec<u8>, ExecError> {
        let statement_name = match target {
            DescribeTarget::Statement => name.to_string(),
            DescribeTarget::Portal => match self.portals.get(name) {
                Some(statement) => statement.clone(),
                None => {
                    return Err(ExecError::other(format!(
                        "portal \"{name}\" does not exist"
                    )))
                }
            },
        };
        let Some(cached) = self.statements.get(&statement_name) else {
            return Err(ExecError::other(format!(
                "prepared statement \"{statement_name}\" does not exist"
            )));
        };
        let mut out = Vec::new();
        if target == DescribeTarget::Statement {
            // A statement description starts with ParameterDescription; a portal
            // description does not, because the parameters were already bound.
            //
            // The OIDs are left unspecified (`0`), which is what PostgreSQL
            // reports for a parameter whose type it cannot pin down at parse
            // time; clients then send the values in text format, which is what
            // `decode_parameter` expects. The *count* is what drivers validate,
            // so it has to be exact.
            let oids = vec![0u32; cached.parameters];
            out.extend_from_slice(&messages::parameter_description(&oids));
        }
        match &cached.schema {
            Some(schema) => out.extend_from_slice(&messages::row_description(schema)),
            None => out.extend_from_slice(&messages::no_data()),
        }
        Ok(out)
    }

    fn execute(&mut self, portal: &str) -> Result<Vec<u8>, ExecError> {
        let Some(statement_name) = self.portals.get(portal).cloned() else {
            return Err(ExecError::other(format!(
                "portal \"{portal}\" does not exist"
            )));
        };
        let Some(cached) = self.statements.get(&statement_name) else {
            return Err(ExecError::other(format!(
                "prepared statement \"{statement_name}\" does not exist"
            )));
        };
        // Clone out of the cache so the session can run while `self` is borrowed.
        let sql = cached.sql.clone();
        let params = self.portal_params.get(portal).cloned().unwrap_or_default();
        let formats = self.portal_formats.get(portal).cloned().unwrap_or_default();
        let started = Instant::now();
        match self.session.execute_with_params(&sql, &params) {
            Ok(results) => {
                info!(
                    sql = %redact_password(&sql),
                    params = %render_parameters(&params),
                    elapsed = ?started.elapsed(),
                    "execute"
                );
                let mut out = Vec::new();
                let mut last = None;
                for result in &results {
                    // `Execute` answers with rows only: the header belongs to
                    // `Describe`. Encoding can fail on a binary format the
                    // engine cannot produce; nothing is sent in that case, only
                    // the error.
                    let encoded = match messages::execute_result_messages(result, &formats) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            self.note_failure();
                            return Err(error);
                        }
                    };
                    for message in encoded {
                        out.extend_from_slice(&message);
                    }
                    out.extend_from_slice(&messages::command_complete(&result.tag()));
                    last = Some(result);
                }
                if let Some(result) = last {
                    self.note_transaction_state(result);
                }
                self.flush_parameter_status(&mut out);
                Ok(out)
            }
            Err(error) => {
                warn!(
                    %error,
                    sql = %redact_password(&sql),
                    params = %render_parameters(&params),
                    elapsed = ?started.elapsed(),
                    "statement failed"
                );
                self.note_failure();
                Err(error)
            }
        }
    }

    fn close(&mut self, target: DescribeTarget, name: &str) {
        match target {
            DescribeTarget::Statement => {
                self.statements.remove(name);
                self.portals.retain(|_, statement| statement != name);
            }
            DescribeTarget::Portal => {
                self.portals.remove(name);
                self.portal_params.remove(name);
                self.portal_formats.remove(name);
            }
        }
    }

    /// Clears the aborted flag when a transaction ends successfully.
    fn note_transaction_state(&mut self, result: &QueryResult) {
        if let QueryResult::Command { tag } = result {
            if tag == "COMMIT" || tag == "ROLLBACK" {
                self.aborted = false;
            }
        }
    }

    /// Marks the transaction aborted when a failure happens inside one.
    fn note_failure(&mut self) {
        if self.session.in_transaction() {
            self.aborted = true;
        }
    }
}

/// Reads a startup-phase message, returning its payload after the length.
async fn read_startup<R: AsyncReadExt + Unpin>(reader: &mut R) -> std::io::Result<Startup> {
    let length = reader.read_i32().await?;
    if !(8..=MAX_STARTUP_BYTES as i32).contains(&length) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid startup message length {length}"),
        ));
    }
    let mut payload = vec![0u8; length as usize - 4];
    reader.read_exact(&mut payload).await?;
    messages::parse_startup(&payload)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))
}

/// Reads a regular frontend message. Returns `None` on a clean EOF.
async fn read_message<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut tag = [0u8; 1];
    match reader.read_exact(&mut tag).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = reader.read_i32().await?;
    if !(4..=MAX_MESSAGE_BYTES as i32).contains(&length) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid message length {length} for tag {}",
                char::from(tag[0])
            ),
        ));
    }
    let mut payload = vec![0u8; length as usize - 4];
    reader.read_exact(&mut payload).await?;
    Ok(Some((tag[0], payload)))
}

/// Convenience for binaries: the parameter list a healthy connection reports.
pub fn standard_parameters(application: &str) -> Vec<(String, String)> {
    vec![
        (
            "server_version".to_string(),
            "15.0 (VelocitySQL)".to_string(),
        ),
        ("client_encoding".to_string(), "UTF8".to_string()),
        ("application_name".to_string(), application.to_string()),
        ("DateStyle".to_string(), "ISO, MDY".to_string()),
        ("TimeZone".to_string(), "UTC".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_password_never_reaches_the_log() {
        assert_eq!(
            redact_password("CREATE ROLE app LOGIN PASSWORD 's3cret'"),
            "CREATE ROLE app LOGIN PASSWORD '***'"
        );
        assert_eq!(
            redact_password("ALTER ROLE app PASSWORD 'it''s'"),
            "ALTER ROLE app PASSWORD '***'"
        );
        // Case is not a way around it, and `PASSWORD NULL` has nothing to hide.
        assert_eq!(
            redact_password("create user bob password 'x'"),
            "create user bob password '***'"
        );
        assert_eq!(
            redact_password("ALTER USER bob PASSWORD NULL"),
            "ALTER USER bob PASSWORD NULL"
        );
        // Only the clause is masked: an identifier or an unrelated statement is
        // passed through untouched — and borrowed, not copied.
        assert_eq!(
            redact_password("SELECT password FROM secrets"),
            "SELECT password FROM secrets"
        );
        assert_eq!(
            redact_password("SELECT my_password, 'x' FROM t"),
            "SELECT my_password, 'x' FROM t"
        );
    }

    #[test]
    fn parameters_render_the_way_postgres_numbers_them() {
        assert_eq!(render_parameters(&[]), "");
        assert_eq!(
            render_parameters(&[
                Value::Int4(1),
                Value::Text("allowRegister".into()),
                Value::Null
            ]),
            "$1=1, $2=allowRegister, $3=NULL"
        );
        // A long value is clipped rather than dumped whole.
        let long = render_parameters(&[Value::Text("x".repeat(200))]);
        assert!(long.ends_with("..."), "{long}");
        assert!(long.len() < 200, "{long}");
    }
}

//! Wire-level integration tests.
//!
//! These speak the protocol over a real TCP socket with a hand-written client,
//! which is the only way to prove that a *driver* would be able to talk to the
//! server: framing, message order, type OIDs and error fields are all checked
//! from the outside, byte by byte.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use vsql_catalog::scram;
use vsql_executor::{Engine, DEFAULT_PASSWORD, DEFAULT_SUPERUSER};
use vsql_protocol::{messages, AuthMethod, ServerConfig};

/// A minimal protocol client.
struct Client {
    stream: TcpStream,
    /// Kept so a test can open a second connection to the same server, which is
    /// the only way to observe two roles on one engine.
    address: std::net::SocketAddr,
}

/// A decoded backend message.
#[derive(Debug, Clone, PartialEq)]
enum Message {
    AuthenticationOk,
    AuthenticationSasl(Vec<String>),
    AuthenticationSaslContinue(String),
    AuthenticationSaslFinal(String),
    ParameterStatus(String, String),
    BackendKeyData,
    ReadyForQuery(char),
    RowDescription(Vec<Field>),
    DataRow(Vec<Option<Vec<u8>>>),
    CommandComplete(String),
    EmptyQuery,
    ParseComplete,
    BindComplete,
    CloseComplete,
    ParameterDescription(Vec<i32>),
    NoData,
    Error(HashMap<String, String>),
    Other(char),
}

#[derive(Debug, Clone, PartialEq)]
struct Field {
    name: String,
    type_oid: i32,
    type_len: i16,
}

/// Collects what the server logs, so a test can assert on it.
///
/// A binary installs the subscriber; the test binary is the one place that can
/// point one at a buffer instead and read back what a client would have seen on
/// the operator's console.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    /// Everything logged so far.
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log buffer")).into_owned()
    }
}

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogs;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The first logged line containing `needle`.
fn line_containing<'a>(logs: &'a str, needle: &str) -> &'a str {
    logs.lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line containing `{needle}` in:\n{logs}"))
}

/// Asserts that a logged line carries the statement's duration.
///
/// Statement lines end in `elapsed=` followed by a number and a unit (`1.234ms`,
/// `503.1µs`). The check is on the *shape* of the value, so a change that logged
/// a `Duration` through a derived `Debug` (`Duration { secs: 0, .. }`) fails here
/// rather than quietly making the log unreadable.
fn assert_duration(line: &str) {
    let (_, duration) = line
        .split_once("elapsed=")
        .unwrap_or_else(|| panic!("no duration in: {line}"));
    assert!(
        duration.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "the duration is not a number: {line}"
    );
}

/// `true` for every message the backend sends as an authentication exchange.
///
/// libpq handles `R` only while it is *waiting* for authentication; once
/// `AuthenticationOk` has arrived, another one is a protocol violation it reports
/// as `unexpected response from server; first received character was "R"`.
fn is_authentication(message: &Message) -> bool {
    matches!(
        message,
        Message::AuthenticationOk
            | Message::AuthenticationSasl(_)
            | Message::AuthenticationSaslContinue(_)
            | Message::AuthenticationSaslFinal(_)
    )
}

impl Client {
    /// Starts the server on an ephemeral port and opens a socket.
    ///
    /// The startup exchange is left to the caller so that a test can name a
    /// database that does not exist.
    async fn open() -> Client {
        Client::open_with(AuthMethod::Trust).await
    }

    /// Starts the server with a chosen authentication method.
    async fn open_with(auth: AuthMethod) -> Client {
        let engine = Arc::new(Engine::new());
        let config = ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            auth,
        };
        let listener = vsql_protocol::bind(&config).await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let _ = vsql_protocol::serve(engine, listener, auth).await;
        });
        let stream = TcpStream::connect(address).await.expect("connect");
        // The server task owns the listener; detaching it here is safe because
        // the whole process ends with the test binary.
        drop(server);
        Client { stream, address }
    }

    /// Opens another connection to the same server.
    async fn reconnect(&self) -> Client {
        Client {
            stream: TcpStream::connect(self.address).await.expect("reconnect"),
            address: self.address,
        }
    }

    /// Refuses TLS, then sends the startup packet asking for `database`.
    async fn start(&mut self, user: &str, database: &str) {
        // Ask for TLS first: the server must refuse with a single 'N'.
        let mut request = Vec::new();
        request.extend_from_slice(&8i32.to_be_bytes());
        request.extend_from_slice(&messages::SSL_REQUEST_CODE.to_be_bytes());
        self.stream.write_all(&request).await.expect("ssl request");
        let mut answer = [0u8; 1];
        self.stream
            .read_exact(&mut answer)
            .await
            .expect("ssl answer");
        assert_eq!(&answer, b"N", "TLS must be refused with 'N'");

        let mut body = Vec::new();
        body.extend_from_slice(&messages::PROTOCOL_V3.to_be_bytes());
        for (key, value) in [("user", user), ("database", database)] {
            body.extend_from_slice(key.as_bytes());
            body.push(0);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut packet = Vec::new();
        packet.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        packet.extend_from_slice(&body);
        self.stream.write_all(&packet).await.expect("startup");
    }

    /// Reads the startup reply, stopping at `ReadyForQuery` or at an error.
    async fn read_startup(&mut self) -> Vec<Message> {
        let mut messages = Vec::new();
        loop {
            let message = self.read().await;
            let done = matches!(message, Message::ReadyForQuery(_) | Message::Error(_));
            messages.push(message);
            if done {
                return messages;
            }
        }
    }

    /// The happy path: connect to the default database, then check the handshake.
    async fn connect() -> Client {
        let mut client = Client::open().await;
        client.start("velocitysql", "velocitysql").await;
        let messages = client.read_startup().await;
        assert_eq!(messages[0], Message::AuthenticationOk);
        // `AuthenticationOk` must be the *only* authentication message: libpq
        // stops treating `R` as an authentication request once it has seen it,
        // and reports any later one as `unexpected response from server; first
        // received character was "R"`.
        assert!(!messages[1..].iter().any(is_authentication), "{messages:?}");
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, Message::BackendKeyData)),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::ParameterStatus(..)))
                .count()
                >= 8,
            "the startup reply must advertise the server's settings: {messages:?}"
        );
        assert_eq!(
            messages.last().unwrap(),
            &Message::ReadyForQuery('I'),
            "{messages:?}"
        );
        client
    }
    /// Sends a `Query` message.
    async fn query(&mut self, sql: &str) {
        let mut body = Vec::new();
        body.extend_from_slice(sql.as_bytes());
        body.push(0);
        self.send(b'Q', &body).await;
    }

    /// Performs the SCRAM-SHA-256 exchange and returns the startup messages.
    ///
    /// Written from the RFC rather than reusing the server's helpers, so the two
    /// implementations have to agree for the test to pass.
    async fn start_scram(&mut self, user: &str, database: &str, password: &str) -> Vec<Message> {
        self.start(user, database).await;

        let Message::AuthenticationSasl(mechanisms) = self.read().await else {
            panic!("expected an AuthenticationSASL request");
        };
        assert_eq!(mechanisms, vec![scram::MECHANISM.to_string()]);

        let client_nonce = "rOprNGfwEbeRWgbNEkqO";
        let client_first_bare = format!("n=,r={client_nonce}");
        let client_first = format!("n,,{client_first_bare}");

        // SASLInitialResponse: mechanism, length, payload.
        let mut body = Vec::new();
        body.extend_from_slice(scram::MECHANISM.as_bytes());
        body.push(0);
        body.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
        body.extend_from_slice(client_first.as_bytes());
        self.send(b'p', &body).await;

        let Message::AuthenticationSaslContinue(server_first) = self.read().await else {
            panic!("expected a server-first message");
        };
        let salt = scram::base64_decode(scram::attribute(&server_first, "s").expect("salt"))
            .expect("base64 salt");
        let iterations: u32 = scram::attribute(&server_first, "i")
            .expect("iterations")
            .parse()
            .expect("integer iterations");
        let combined_nonce = scram::attribute(&server_first, "r")
            .expect("nonce")
            .to_string();
        assert!(
            combined_nonce.starts_with(client_nonce),
            "the server nonce must extend the client's"
        );

        let without_proof = format!("c=biws,r={combined_nonce}");
        let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
        let salted_password = scram::pbkdf2_hmac_sha256(password.as_bytes(), &salt, iterations);
        let client_key = scram::hmac_sha256(&salted_password, b"Client Key");
        let stored_key = scram::sha256(&client_key);
        let client_signature = scram::hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(client_signature.iter())
            .map(|(key, signature)| key ^ signature)
            .collect();

        let client_final = format!("{without_proof},p={}", scram::base64_encode(&proof));
        self.send(b'p', client_final.as_bytes()).await;

        let Message::AuthenticationSaslFinal(server_final) = self.read().await else {
            panic!("expected a server-final message");
        };
        let server_key = scram::hmac_sha256(&salted_password, b"Server Key");
        assert_eq!(
            scram::attribute(&server_final, "v"),
            Some(
                scram::base64_encode(&scram::hmac_sha256(&server_key, auth_message.as_bytes()))
                    .as_str()
            ),
            "the server must prove it holds the verifier"
        );

        let mut messages = vec![self.read().await];
        messages.extend(self.read_startup().await);
        messages
    }

    /// Sends a typed frontend message.
    async fn send(&mut self, tag: u8, body: &[u8]) {
        let mut packet = Vec::new();
        packet.push(tag);
        packet.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        packet.extend_from_slice(body);
        self.stream.write_all(&packet).await.expect("write");
    }

    /// Runs a query and collects every message up to `ReadyForQuery`.
    async fn query_all(&mut self, sql: &str) -> Vec<Message> {
        self.query(sql).await;
        self.read_until_ready().await
    }

    async fn read_until_ready(&mut self) -> Vec<Message> {
        let mut messages = Vec::new();
        loop {
            let message = self.read().await;
            let done = matches!(message, Message::ReadyForQuery(_));
            messages.push(message);
            if done {
                return messages;
            }
        }
    }

    /// Reads and decodes one backend message.
    async fn read(&mut self) -> Message {
        let mut tag = [0u8; 1];
        self.stream.read_exact(&mut tag).await.expect("message tag");
        let length = self.stream.read_i32().await.expect("message length");
        let mut payload = vec![0u8; length as usize - 4];
        self.stream
            .read_exact(&mut payload)
            .await
            .expect("message payload");
        decode(tag[0], &payload)
    }
}

/// Decodes a backend message payload.
fn decode(tag: u8, payload: &[u8]) -> Message {
    let mut cursor = Cursor::new(payload);
    match tag {
        b'R' => match cursor.i32() {
            0 => Message::AuthenticationOk,
            10 => {
                let mut mechanisms = Vec::new();
                loop {
                    let mechanism = cursor.cstr();
                    if mechanism.is_empty() {
                        break;
                    }
                    mechanisms.push(mechanism);
                }
                Message::AuthenticationSasl(mechanisms)
            }
            11 => Message::AuthenticationSaslContinue(cursor.rest()),
            12 => Message::AuthenticationSaslFinal(cursor.rest()),
            other => panic!("unexpected authentication request {other}"),
        },
        b'S' => Message::ParameterStatus(cursor.cstr(), cursor.cstr()),
        b'K' => {
            cursor.i32();
            cursor.i32();
            Message::BackendKeyData
        }
        b'Z' => Message::ReadyForQuery(char::from(cursor.u8())),
        b'T' => {
            let count = cursor.i16();
            let mut fields = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let name = cursor.cstr();
                cursor.i32();
                cursor.i16();
                let type_oid = cursor.i32();
                let type_len = cursor.i16();
                cursor.i32();
                cursor.i16();
                fields.push(Field {
                    name,
                    type_oid,
                    type_len,
                });
            }
            Message::RowDescription(fields)
        }
        b'D' => {
            let count = cursor.i16();
            let mut values = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let length = cursor.i32();
                if length < 0 {
                    values.push(None);
                } else {
                    // Kept as raw bytes: a client that asked for the binary
                    // format receives bytes, not text.
                    values.push(Some(cursor.take(length as usize).to_vec()));
                }
            }
            Message::DataRow(values)
        }
        b'C' => Message::CommandComplete(cursor.cstr()),
        b'I' => Message::EmptyQuery,
        b'1' => Message::ParseComplete,
        b'2' => Message::BindComplete,
        b'3' => Message::CloseComplete,
        b't' => {
            let count = cursor.i16();
            let mut oids = Vec::with_capacity(count as usize);
            for _ in 0..count {
                oids.push(cursor.i32());
            }
            Message::ParameterDescription(oids)
        }
        b'n' => Message::NoData,
        b'E' => {
            let mut fields = HashMap::new();
            loop {
                let code = cursor.u8();
                if code == 0 {
                    break;
                }
                fields.insert(char::from(code).to_string(), cursor.cstr());
            }
            Message::Error(fields)
        }
        other => Message::Other(char::from(other)),
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Cursor { bytes, position: 0 }
    }

    fn take(&mut self, count: usize) -> &'a [u8] {
        let slice = &self.bytes[self.position..self.position + count];
        self.position += count;
        slice
    }

    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }

    fn i16(&mut self) -> i16 {
        let bytes = self.take(2);
        i16::from_be_bytes([bytes[0], bytes[1]])
    }

    fn i32(&mut self) -> i32 {
        let bytes = self.take(4);
        i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    fn rest(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.bytes[self.position..]).into_owned();
        self.position = self.bytes.len();
        text
    }

    fn cstr(&mut self) -> String {
        let rest = &self.bytes[self.position..];
        let end = rest.iter().position(|byte| *byte == 0).expect("terminator");
        let text = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.position += end + 1;
        text
    }
}

/// Builds a `Bind` payload with no parameters and no result formats.
fn bind_body(portal: &str, statement: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // parameter format codes
    body.extend_from_slice(&0i16.to_be_bytes()); // parameter values
    body.extend_from_slice(&0i16.to_be_bytes()); // result format codes
    body
}

/// Builds a `Bind` payload whose parameters are all sent in text format, the
/// way `lib/pq` sends them; `None` is SQL `NULL`.
fn bind_body_with(portal: &str, statement: &str, params: &[Option<&str>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // parameter format codes: text
    body.extend_from_slice(&(params.len() as i16).to_be_bytes());
    for param in params {
        match param {
            Some(text) => {
                body.extend_from_slice(&(text.len() as i32).to_be_bytes());
                body.extend_from_slice(text.as_bytes());
            }
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
    body.extend_from_slice(&0i16.to_be_bytes()); // result format codes
    body
}

/// Builds a `Bind` payload that requests the binary result format for every
/// column and binds no parameters.
fn binary_bind_body(statement: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"portal\0");
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // parameter format codes
    body.extend_from_slice(&0i16.to_be_bytes()); // no parameters
    body.extend_from_slice(&1i16.to_be_bytes()); // one result format ...
    body.extend_from_slice(&1i16.to_be_bytes()); // ... binary for all columns
    body
}

/// Builds an `Execute` payload with no row limit.
fn execute_body(portal: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i32.to_be_bytes());
    body
}

fn rows(messages: &[Message]) -> Vec<Vec<Option<String>>> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::DataRow(values) => Some(
                values
                    .iter()
                    .map(|value| {
                        value
                            .as_ref()
                            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                    })
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

/// The raw `DataRow` values, for a test that asked for the binary format.
fn binary_rows(messages: &[Message]) -> Vec<Vec<Option<Vec<u8>>>> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::DataRow(values) => Some(values.clone()),
            _ => None,
        })
        .collect()
}

fn error_fields(messages: &[Message]) -> HashMap<String, String> {
    messages
        .iter()
        .find_map(|message| match message {
            Message::Error(fields) => Some(fields.clone()),
            _ => None,
        })
        .expect("an ErrorResponse")
}

#[tokio::test]
async fn startup_reports_parameters_and_readiness() {
    // `connect` already asserts the handshake shape; this documents it.
    let mut client = Client::connect().await;
    client.query("SELECT 1").await;
    assert!(matches!(client.read().await, Message::RowDescription(_)));
}

#[tokio::test]
async fn simple_query_round_trip() {
    let mut client = Client::connect().await;
    client
        .query_all("CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL)")
        .await;
    client
        .query_all("INSERT INTO users VALUES (1, 'ada'), (2, 'grace')")
        .await;

    let messages = client
        .query_all("SELECT id, name FROM users ORDER BY id")
        .await;

    let Message::RowDescription(fields) = &messages[0] else {
        panic!("expected a RowDescription, got {:?}", messages[0]);
    };
    assert_eq!(fields[0].name, "id");
    assert_eq!(fields[0].type_oid, 23, "int4 oid");
    assert_eq!(fields[0].type_len, 4);
    assert_eq!(fields[1].name, "name");
    assert_eq!(fields[1].type_oid, 25, "text oid");
    assert_eq!(fields[1].type_len, -1, "text is variable width");

    assert_eq!(
        rows(&messages),
        vec![
            vec![Some("1".to_string()), Some("ada".to_string())],
            vec![Some("2".to_string()), Some("grace".to_string())],
        ]
    );
    assert_eq!(
        messages.last().unwrap(),
        &Message::ReadyForQuery('I'),
        "the stream is ready for the next statement"
    );
}

#[tokio::test]
async fn nulls_are_sent_as_minus_one_length() {
    let mut client = Client::connect().await;
    client.query_all("CREATE TABLE t (a INT, b TEXT)").await;
    client.query_all("INSERT INTO t VALUES (1, NULL)").await;
    let messages = client.query_all("SELECT a, b FROM t").await;
    assert_eq!(
        rows(&messages),
        vec![vec![Some("1".to_string()), None]],
        "NULL must not be confused with an empty string"
    );
    // And an empty string stays distinguishable from NULL.
    client.query_all("INSERT INTO t VALUES (2, '')").await;
    let messages = client.query_all("SELECT b FROM t WHERE a = 2").await;
    assert_eq!(rows(&messages), vec![vec![Some(String::new())]]);
}

#[tokio::test]
async fn command_tags_match_postgres() {
    let mut client = Client::connect().await;
    let messages = client.query_all("CREATE TABLE t (a INT)").await;
    assert!(messages.contains(&Message::CommandComplete("CREATE TABLE".to_string())));

    let messages = client.query_all("INSERT INTO t VALUES (1), (2)").await;
    assert!(
        messages.contains(&Message::CommandComplete("INSERT 0 2".to_string())),
        "INSERT reports `INSERT 0 <rows>`, not `INSERT <rows>`"
    );

    let messages = client.query_all("SELECT a FROM t").await;
    assert!(messages.contains(&Message::CommandComplete("SELECT 2".to_string())));

    let messages = client.query_all("UPDATE t SET a = a + 1").await;
    assert!(messages.contains(&Message::CommandComplete("UPDATE 2".to_string())));

    let messages = client.query_all("DELETE FROM t").await;
    assert!(messages.contains(&Message::CommandComplete("DELETE 2".to_string())));
}

#[tokio::test]
async fn multiple_statements_are_reported_one_tag_each() {
    let mut client = Client::connect().await;
    let messages = client
        .query_all("CREATE TABLE t (a INT); INSERT INTO t VALUES (1); SELECT a FROM t;")
        .await;
    let tags: Vec<&str> = messages
        .iter()
        .filter_map(|message| match message {
            Message::CommandComplete(tag) => Some(tag.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tags, vec!["CREATE TABLE", "INSERT 0 1", "SELECT 1"]);
    assert_eq!(messages.last().unwrap(), &Message::ReadyForQuery('I'));
}

#[tokio::test]
async fn empty_query_is_answered_with_empty_query_response() {
    let mut client = Client::connect().await;
    let messages = client.query_all(";").await;
    assert_eq!(
        messages,
        vec![Message::EmptyQuery, Message::ReadyForQuery('I')]
    );
}

#[tokio::test]
async fn errors_carry_a_sqlstate_and_close_the_exchange() {
    let mut client = Client::connect().await;
    let messages = client.query_all("SELECT * FROM missing").await;
    let fields = error_fields(&messages);
    assert_eq!(fields.get("C").map(String::as_str), Some("42P01"));
    assert_eq!(
        fields.get("M").map(String::as_str),
        Some("relation \"missing\" does not exist")
    );
    assert_eq!(fields.get("S").map(String::as_str), Some("ERROR"));
    assert_eq!(
        messages.last().unwrap(),
        &Message::ReadyForQuery('I'),
        "ReadyForQuery must follow an error so the client can continue"
    );

    // A syntax error points at the offending token.
    let mut client = Client::connect().await;
    let messages = client.query_all("SELECT FROM").await;
    let fields = error_fields(&messages);
    assert_eq!(fields.get("C").map(String::as_str), Some("42601"));
    assert!(fields
        .get("M")
        .expect("message")
        .starts_with("syntax error at or near"));
}

#[tokio::test]
async fn a_failed_statement_inside_a_transaction_aborts_it() {
    let mut client = Client::connect().await;
    client.query_all("BEGIN").await;
    let messages = client.query_all("SELECT * FROM missing").await;
    assert_eq!(
        messages.last().unwrap(),
        &Message::ReadyForQuery('E'),
        "an aborted transaction reports status 'E'"
    );
    let messages = client.query_all("COMMIT").await;
    assert_eq!(messages.last().unwrap(), &Message::ReadyForQuery('I'));
}

#[tokio::test]
async fn extended_protocol_round_trip() {
    let mut client = Client::connect().await;
    client.query_all("CREATE TABLE t (a INT, b TEXT)").await;
    client.query_all("INSERT INTO t VALUES (1, 'x')").await;

    // Parse
    let mut body = Vec::new();
    body.extend_from_slice(b"stmt\0");
    body.extend_from_slice(b"SELECT a, b FROM t\0");
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);

    // Describe(statement): parameters first, then the row description.
    client.send(b'D', b"Sstmt\0").await;
    assert_eq!(client.read().await, Message::ParameterDescription(vec![]));
    let Message::RowDescription(fields) = client.read().await else {
        panic!("expected RowDescription");
    };
    assert_eq!(fields.len(), 2);

    // Bind
    client.send(b'B', &bind_body("portal", "stmt")).await;
    assert_eq!(client.read().await, Message::BindComplete);

    // Execute: a DataRow per row, then the command tag — and *no*
    // RowDescription. The header belongs to `Describe`, and repeating it here
    // is what makes `lib/pq` abort with `unexpected message during extended
    // query execution: "(T) RowDescription"`.
    client.send(b'E', &execute_body("portal")).await;
    assert_eq!(
        rows(&[client.read().await]),
        vec![vec![Some("1".to_string()), Some("x".to_string())]]
    );
    assert_eq!(
        client.read().await,
        Message::CommandComplete("SELECT 1".to_string())
    );
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));
}

#[tokio::test]
async fn extended_protocol_errors_are_skipped_until_sync() {
    let mut client = Client::connect().await;
    // A statement that cannot be planned must be refused honestly.
    let mut body = Vec::new();
    body.extend_from_slice(b"stmt\0");
    body.extend_from_slice(b"SELECT * FROM does_not_exist\0");
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    let Message::Error(fields) = client.read().await else {
        panic!("expected an error for the undefined table");
    };
    assert_eq!(fields.get("C").map(String::as_str), Some("42P01"));

    // The server must discard messages until Sync, then resynchronise.
    client.send(b'B', &bind_body("portal", "stmt")).await;
    client.send(b'E', &execute_body("portal")).await;
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));

    // ... and the connection is still usable afterwards.
    let messages = client.query_all("SELECT 1 + 1").await;
    assert_eq!(rows(&messages), vec![vec![Some("2".to_string())]]);
}

#[tokio::test]
async fn extended_protocol_binds_parameters() {
    let mut client = Client::connect().await;
    let mut body = Vec::new();
    body.extend_from_slice(b"stmt\0");
    body.extend_from_slice(b"SELECT 1 + $1\0");
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);

    // A driver asks how many parameters the statement wants before it binds;
    // reporting zero here is what makes `lib/pq` refuse the query outright.
    client.send(b'D', b"Sstmt\0").await;
    assert_eq!(
        client.read().await,
        Message::ParameterDescription(vec![0]),
        "one `$1` must be reported as one parameter"
    );
    let Message::RowDescription(fields) = client.read().await else {
        panic!("expected a RowDescription");
    };
    assert_eq!(fields.len(), 1, "the header comes from `Describe`");

    // Bind `$1` to the text `2`; the engine reads it as the integer 2.
    client
        .send(b'B', &bind_body_with("portal", "stmt", &[Some("2")]))
        .await;
    assert_eq!(client.read().await, Message::BindComplete);

    client.send(b'E', &execute_body("portal")).await;
    assert_eq!(
        rows(&[client.read().await]),
        vec![vec![Some("3".to_string())]]
    );
    assert_eq!(
        client.read().await,
        Message::CommandComplete("SELECT 1".to_string())
    );
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));
}

#[tokio::test]
async fn a_drivers_three_parameter_query_round_trips() {
    // The shape `GORM` emits through `lib/pq`: double-quoted identifiers, a
    // parameter per predicate, and every value sent as text.
    let mut client = Client::connect().await;
    client
        .query_all(
            "CREATE TABLE carts (id INT, status INT, expires_at TIMESTAMPTZ, is_permanent BOOL)",
        )
        .await;
    client
        .query_all(
            "INSERT INTO carts VALUES \
             (1, 1, '2026-10-01 00:00:00+00', false), \
             (2, 2, '2026-10-01 00:00:00+00', false), \
             (3, 1, '2026-10-01 00:00:00+00', true)",
        )
        .await;

    let mut body = Vec::new();
    body.extend_from_slice(b"stmt\0");
    body.extend_from_slice(
        b"SELECT \"carts\".\"id\" FROM \"carts\" \
          WHERE (\"carts\".\"status\" = $1) \
            AND (\"carts\".\"expires_at\" < $2) \
            AND (\"carts\".\"is_permanent\" = $3)\0",
    );
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);

    client.send(b'D', b"Sstmt\0").await;
    assert_eq!(
        client.read().await,
        Message::ParameterDescription(vec![0, 0, 0]),
        "`lib/pq` reports `got 3 parameters but the statement requires 0` \
         without this"
    );
    let Message::RowDescription(fields) = client.read().await else {
        panic!("expected a RowDescription");
    };
    assert_eq!(fields.len(), 1, "the header comes from `Describe`");

    // The text values must be read as the columns' types: `int`, `timestamptz`
    // (with its offset) and `bool`.
    client
        .send(
            b'B',
            &bind_body_with(
                "portal",
                "stmt",
                &[Some("1"), Some("2026-10-01 23:00:00+08:00"), Some("false")],
            ),
        )
        .await;
    assert_eq!(client.read().await, Message::BindComplete);

    client.send(b'E', &execute_body("portal")).await;
    assert_eq!(
        rows(&[client.read().await]),
        vec![vec![Some("1".to_string())]],
        "only the non-permanent, unexpired cart with status 1 matches"
    );
    assert_eq!(
        client.read().await,
        Message::CommandComplete("SELECT 1".to_string())
    );
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));
}

#[tokio::test]
async fn binary_result_format_encodes_the_common_types() {
    let mut client = Client::connect().await;
    client
        .query_all(
            "CREATE TABLE carts (id INT, status INT, expires_at TIMESTAMPTZ, \
             is_permanent BOOL, exact NUMERIC, scaled NUMERIC(10,2))",
        )
        .await;
    client
        .query_all(
            "INSERT INTO carts VALUES \
             (7, 1, '2026-10-01 00:00:00+00', true, 12345.678, 12345.678)",
        )
        .await;

    let mut body = Vec::new();
    body.extend_from_slice(b"stmt\0");
    body.extend_from_slice(
        b"SELECT id, status, expires_at, is_permanent, exact, scaled FROM carts\0",
    );
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);

    // The header a client works from comes from `Describe`, which is why
    // `Execute` must not send it again.
    client.send(b'D', b"Sstmt\0").await;
    assert_eq!(client.read().await, Message::ParameterDescription(vec![]));
    let Message::RowDescription(fields) = client.read().await else {
        panic!("expected a RowDescription");
    };
    assert_eq!(fields.len(), 6);

    // A single format code applies to every column. Binary is what `lib/pq`
    // asks for on the types it decodes natively; sending text instead would be
    // read as garbage.
    let mut bind = Vec::new();
    bind.extend_from_slice(b"portal\0stmt\0");
    bind.extend_from_slice(&0i16.to_be_bytes()); // parameter format codes
    bind.extend_from_slice(&0i16.to_be_bytes()); // no parameters
    bind.extend_from_slice(&1i16.to_be_bytes()); // one result format ...
    bind.extend_from_slice(&1i16.to_be_bytes()); // ... binary for all of them
    client.send(b'B', &bind).await;
    assert_eq!(client.read().await, Message::BindComplete);

    client.send(b'E', &execute_body("portal")).await;
    let values = binary_rows(&[client.read().await]);
    assert_eq!(values.len(), 1, "one row");

    let value = |index: usize| values[0][index].clone().expect("not NULL");
    // Integers and `bool` are big-endian fixed-width fields.
    assert_eq!(i32::from_be_bytes(value(0).try_into().expect("int4")), 7);
    assert_eq!(i32::from_be_bytes(value(1).try_into().expect("int4")), 1);
    assert_eq!(value(3), vec![1u8], "`true` is one byte");
    // `2026-10-01 00:00:00+00` is 9770 days after PostgreSQL's 2000-01-01
    // epoch, in microseconds.
    assert_eq!(
        i64::from_be_bytes(value(2).try_into().expect("timestamptz")),
        9_770 * 86_400_000_000
    );
    // `12345.678` in PostgreSQL's numeric layout: 3 digits, weight 1, positive,
    // scale 3, then the base-10000 groups 1, 2345, 6780.
    assert_eq!(
        value(4),
        vec![0, 3, 0, 1, 0, 0, 0, 3, 0, 1, 0x09, 0x29, 0x1A, 0x7C]
    );
    // The same value in a `NUMERIC(10,2)` column is rounded on the way in, and
    // the declared display scale travels with it.
    assert_eq!(
        value(5),
        vec![0, 3, 0, 1, 0, 0, 0, 2, 0, 1, 0x09, 0x29, 0x1A, 0x90]
    );

    assert_eq!(
        client.read().await,
        Message::CommandComplete("SELECT 1".to_string())
    );
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));
}

#[tokio::test]
async fn binary_format_covers_jsonb_and_refuses_arrays() {
    let mut client = Client::connect().await;

    let mut body = Vec::new();
    body.extend_from_slice(b"doc\0");
    body.extend_from_slice(b"SELECT '{\"a\":1}'::jsonb\0");
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);
    client.send(b'B', &binary_bind_body("doc")).await;
    assert_eq!(client.read().await, Message::BindComplete);
    client.send(b'E', &execute_body("portal")).await;
    let values = binary_rows(&[client.read().await]);
    // `jsonb_send` prefixes the version byte, then the JSON text.
    assert_eq!(
        values[0][0].clone().expect("jsonb"),
        b"\x01{\"a\":1}".to_vec()
    );
    assert_eq!(
        client.read().await,
        Message::CommandComplete("SELECT 1".to_string())
    );
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));

    // An array has a binary form too, but the engine does not produce it yet:
    // the statement is refused rather than answered with bytes the client
    // would decode as three integers.
    let mut body = Vec::new();
    body.extend_from_slice(b"list\0");
    body.extend_from_slice(b"SELECT ARRAY[1,2,3]\0");
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);
    client.send(b'B', &binary_bind_body("list")).await;
    assert_eq!(client.read().await, Message::BindComplete);
    client.send(b'E', &execute_body("portal")).await;
    let Message::Error(fields) = client.read().await else {
        panic!("expected an error for the array");
    };
    let message = fields.get("M").expect("message");
    assert!(
        message.contains("binary result format"),
        "unexpected message: {message}"
    );
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));
}

#[tokio::test]
async fn array_and_json_parameters_are_told_apart() {
    let mut client = Client::connect().await;
    client
        .query_all("CREATE TABLE logs (id INT, payload JSONB, note TEXT)")
        .await;
    client
        .query_all("INSERT INTO logs (id, note) VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .await;

    // `{1,2}` is an array literal *because the left operand is one*; nothing at
    // `Bind` time has to guess.
    let mut body = Vec::new();
    body.extend_from_slice(b"any\0");
    body.extend_from_slice(b"SELECT id FROM logs WHERE id = ANY($1) ORDER BY id\0");
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);
    client
        .send(b'B', &bind_body_with("portal", "any", &[Some("{1,2}")]))
        .await;
    assert_eq!(client.read().await, Message::BindComplete);
    client.send(b'E', &execute_body("portal")).await;
    let mut messages = Vec::new();
    loop {
        let message = client.read().await;
        let done = matches!(message, Message::CommandComplete(_));
        messages.push(message);
        if done {
            break;
        }
    }
    assert_eq!(
        rows(&messages),
        vec![vec![Some("1".to_string())], vec![Some("2".to_string())]]
    );
    assert_eq!(
        messages.last(),
        Some(&Message::CommandComplete("SELECT 2".to_string()))
    );
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));

    // The same shape bound for the `jsonb` column is a JSON object. Reading it
    // as an array is what made the insert fail with
    // `cannot cast type text[] to jsonb`.
    let mut body = Vec::new();
    body.extend_from_slice(b"ins\0");
    body.extend_from_slice(
        b"INSERT INTO logs (id, payload) VALUES ($1, $2) RETURNING id, payload\0",
    );
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);
    // A `RETURNING` insert is row-returning, so `Describe` answers with its
    // list — a driver that only got `NoData` would have no column names.
    client.send(b'D', b"Sins\0").await;
    assert_eq!(
        client.read().await,
        Message::ParameterDescription(vec![0, 0])
    );
    let Message::RowDescription(fields) = client.read().await else {
        panic!("expected a RowDescription for the RETURNING list");
    };
    let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
    assert_eq!(names, vec!["id", "payload"]);
    client
        .send(
            b'B',
            &bind_body_with("portal", "ins", &[Some("4"), Some("{\"password\":\"x\"}")]),
        )
        .await;
    assert_eq!(client.read().await, Message::BindComplete);
    client.send(b'E', &execute_body("portal")).await;
    assert_eq!(
        rows(&[client.read().await]),
        vec![vec![
            Some("4".to_string()),
            Some("{\"password\":\"x\"}".to_string())
        ]]
    );
    assert_eq!(
        client.read().await,
        Message::CommandComplete("SELECT 1".to_string()),
        "the row count is what matters here; see the note on `RETURNING` tags below"
    );
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));
}

#[tokio::test]
async fn a_parameter_count_mismatch_is_reported_like_postgres() {
    let mut client = Client::connect().await;
    let mut body = Vec::new();
    body.extend_from_slice(b"stmt\0");
    body.extend_from_slice(b"SELECT $1::int\0");
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);

    // Binding none of the one parameter is refused at `Bind` time, in
    // PostgreSQL's words, rather than failing later on an unbound `$1`.
    client
        .send(b'B', &bind_body_with("portal", "stmt", &[]))
        .await;
    let fields = error_fields(&[client.read().await]);
    assert_eq!(fields.get("C").map(String::as_str), Some("08P01"));
    assert_eq!(
        fields.get("M").map(String::as_str),
        Some("bind message supplies 0 parameters, but prepared statement \"stmt\" requires 1")
    );
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));

    // A simple query has nowhere to supply a value, so PostgreSQL reports the
    // parameter itself as undefined.
    let fields = error_fields(&client.query_all("SELECT $1::int").await);
    assert_eq!(fields.get("C").map(String::as_str), Some("42P02"));
    assert_eq!(
        fields.get("M").map(String::as_str),
        Some("there is no parameter $1")
    );
}

#[tokio::test]
async fn prepared_statements_can_be_closed() {
    let mut client = Client::connect().await;
    let mut body = Vec::new();
    body.extend_from_slice(b"tmp\0");
    body.extend_from_slice(b"SELECT 1\0");
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);

    client.send(b'C', b"Stmp\0").await;
    assert_eq!(client.read().await, Message::CloseComplete);

    // Describing a closed statement is an error, not a panic.
    client.send(b'D', b"Stmp\0").await;
    let Message::Error(fields) = client.read().await else {
        panic!("expected an error");
    };
    assert!(fields.get("M").expect("message").contains("does not exist"));
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));
}

#[tokio::test]
async fn describe_returns_no_data_for_statements_without_rows() {
    let mut client = Client::connect().await;
    let mut body = Vec::new();
    body.extend_from_slice(b"ddl\0");
    body.extend_from_slice(b"CREATE TABLE t (a INT)\0");
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);

    client.send(b'D', b"Sddl\0").await;
    assert_eq!(client.read().await, Message::ParameterDescription(vec![]));
    assert_eq!(client.read().await, Message::NoData);
    client.send(b'S', &[]).await;
    assert_eq!(client.read().await, Message::ReadyForQuery('I'));
}

#[tokio::test]
async fn terminate_closes_the_connection_cleanly() {
    let mut client = Client::connect().await;
    client.send(b'X', &[]).await;
    let mut buffer = [0u8; 1];
    let read = client
        .stream
        .read(&mut buffer)
        .await
        .expect("read after terminate");
    assert_eq!(read, 0, "the server must close the socket");
}

#[tokio::test]
async fn an_unknown_database_is_fatal_and_closes_the_connection() {
    let mut client = Client::open().await;
    client.start("velocitysql", "no_such_database").await;
    let messages = client.read_startup().await;

    assert_eq!(
        messages.len(),
        1,
        "nothing is negotiated before the failure: {messages:?}"
    );
    let Message::Error(fields) = &messages[0] else {
        panic!("expected a FATAL error, got {messages:?}");
    };
    assert_eq!(fields.get("S").map(String::as_str), Some("FATAL"));
    assert_eq!(fields.get("C").map(String::as_str), Some("3D000"));
    assert_eq!(
        fields.get("M").map(String::as_str),
        Some("database \"no_such_database\" does not exist")
    );

    // ... and the socket is closed, the way PostgreSQL does it.
    let mut buffer = [0u8; 1];
    let read = client
        .stream
        .read(&mut buffer)
        .await
        .expect("read after FATAL");
    assert_eq!(read, 0, "the server must close the connection");
}

#[tokio::test]
async fn the_compatibility_database_accepts_connections() {
    let mut client = Client::open().await;
    client.start("velocitysql", "postgres").await;
    let messages = client.read_startup().await;
    assert_eq!(messages.last().unwrap(), &Message::ReadyForQuery('I'));

    let rows = rows(&client.query_all("SELECT current_database()").await);
    assert_eq!(
        rows,
        vec![vec![Some("postgres".to_string())]],
        "current_database() reports the connection, not a constant"
    );
}
#[tokio::test]
async fn scram_authenticates_the_bootstrap_superuser() {
    let mut client = Client::open_with(AuthMethod::ScramSha256).await;
    let messages = client
        .start_scram(DEFAULT_SUPERUSER, "velocitysql", DEFAULT_PASSWORD)
        .await;

    assert_eq!(messages[0], Message::AuthenticationOk);
    assert_eq!(messages.last().unwrap(), &Message::ReadyForQuery('I'));
    assert_eq!(
        rows(&client.query_all("SELECT current_user").await),
        vec![vec![Some(DEFAULT_SUPERUSER.to_string())]]
    );
}

#[tokio::test]
async fn scram_sends_authentication_ok_exactly_once() {
    // Regression: the SCRAM exchange sent `AuthenticationOk` itself and
    // `send_startup_reply` sent it again. Our own test client tolerated the
    // duplicate; libpq does not, so `psql` and `pgAdmin` failed with
    // `unexpected response from server; first received character was "R"`.
    let mut client = Client::open_with(AuthMethod::ScramSha256).await;
    let messages = client
        .start_scram(DEFAULT_SUPERUSER, "velocitysql", DEFAULT_PASSWORD)
        .await;

    assert_eq!(messages[0], Message::AuthenticationOk, "{messages:?}");
    assert_eq!(
        messages
            .iter()
            .filter(|message| **message == Message::AuthenticationOk)
            .count(),
        1,
        "a second AuthenticationOk is a protocol violation: {messages:?}"
    );
    assert!(!messages[1..].iter().any(is_authentication), "{messages:?}");
}

#[tokio::test]
async fn a_newly_created_role_can_log_in() {
    let mut admin = Client::open_with(AuthMethod::ScramSha256).await;
    admin
        .start_scram(DEFAULT_SUPERUSER, "velocitysql", DEFAULT_PASSWORD)
        .await;

    // Creating the account is a privilege the bootstrap superuser has.
    let created = admin
        .query_all("CREATE ROLE app LOGIN PASSWORD 's3cret'")
        .await;
    assert!(
        created.contains(&Message::CommandComplete("CREATE ROLE".to_string())),
        "{created:?}"
    );
    // `\\du` reads the same registry through the catalogue, not the parser.
    let roles = admin.query_all("SELECT current_user").await;
    assert_eq!(
        rows(&roles),
        vec![vec![Some(DEFAULT_SUPERUSER.to_string())]]
    );

    // The new account connects to the *same* server, with its own password.
    let mut client = admin.reconnect().await;
    let messages = client.start_scram("app", "velocitysql", "s3cret").await;
    assert_eq!(messages.last().unwrap(), &Message::ReadyForQuery('I'));
    assert_eq!(
        rows(&client.query_all("SELECT current_user").await),
        vec![vec![Some("app".to_string())]]
    );
    // ... and a different password is still refused.
    let mut wrong = admin.reconnect().await;
    wrong.start("app", "velocitysql").await;
    let Message::AuthenticationSasl(_) = wrong.read().await else {
        panic!("expected an AuthenticationSASL request");
    };
}

#[tokio::test]
async fn a_wrong_password_is_fatal() {
    let mut client = Client::open_with(AuthMethod::ScramSha256).await;
    client.start(DEFAULT_SUPERUSER, "velocitysql").await;
    let Message::AuthenticationSasl(_) = client.read().await else {
        panic!("expected an AuthenticationSASL request");
    };

    // Complete the exchange with a wrong password.
    let body = {
        let mut body = Vec::new();
        body.extend_from_slice(b"SCRAM-SHA-256\0");
        let initial = "n,,n=,r=abcdef";
        body.extend_from_slice(&(initial.len() as i32).to_be_bytes());
        body.extend_from_slice(initial.as_bytes());
        body
    };
    client.send(b'p', &body).await;
    let Message::AuthenticationSaslContinue(server_first) = client.read().await else {
        panic!("expected a server-first message");
    };
    let proof = scram::base64_encode(&[0u8; 32]);
    let client_final = format!(
        "c=biws,r={},p={proof}",
        scram::attribute(&server_first, "r").unwrap()
    );
    client.send(b'p', client_final.as_bytes()).await;

    let Message::Error(fields) = client.read().await else {
        panic!("expected a FATAL error");
    };
    assert_eq!(fields.get("S").map(String::as_str), Some("FATAL"));
    assert_eq!(fields.get("C").map(String::as_str), Some("28P01"));
    assert_eq!(
        fields.get("M").map(String::as_str),
        Some(format!("password authentication failed for user \"{DEFAULT_SUPERUSER}\"").as_str())
    );

    let mut buffer = [0u8; 1];
    let read = client
        .stream
        .read(&mut buffer)
        .await
        .expect("read after FATAL");
    assert_eq!(read, 0, "the server must close the connection");
}

#[tokio::test]
async fn an_unknown_role_looks_exactly_like_a_wrong_password() {
    let mut client = Client::open_with(AuthMethod::ScramSha256).await;
    // The challenge must be offered even for a role that does not exist, so the
    // handshake cannot be used to enumerate accounts.
    client.start("no_such_role", "velocitysql").await;
    let Message::AuthenticationSasl(mechanisms) = client.read().await else {
        panic!("expected an AuthenticationSASL request");
    };
    assert_eq!(mechanisms, vec![scram::MECHANISM.to_string()]);

    let body = {
        let mut body = Vec::new();
        body.extend_from_slice(b"SCRAM-SHA-256\0");
        let initial = "n,,n=,r=abcdef";
        body.extend_from_slice(&(initial.len() as i32).to_be_bytes());
        body.extend_from_slice(initial.as_bytes());
        body
    };
    client.send(b'p', &body).await;
    let Message::AuthenticationSaslContinue(server_first) = client.read().await else {
        panic!("expected a server-first message");
    };
    let client_final = format!(
        "c=biws,r={},p={}",
        scram::attribute(&server_first, "r").unwrap(),
        scram::base64_encode(&[0u8; 32])
    );
    client.send(b'p', client_final.as_bytes()).await;

    let Message::Error(fields) = client.read().await else {
        panic!("expected a FATAL error");
    };
    assert_eq!(fields.get("C").map(String::as_str), Some("28P01"));
    assert_eq!(
        fields.get("M").map(String::as_str),
        Some("password authentication failed for user \"no_such_role\"")
    );
}

#[tokio::test]
async fn a_role_without_login_cannot_connect() {
    let mut admin = Client::open_with(AuthMethod::ScramSha256).await;
    admin
        .start_scram(DEFAULT_SUPERUSER, "velocitysql", DEFAULT_PASSWORD)
        .await;
    admin.query_all("CREATE ROLE nosuchlogin").await;

    let mut client = admin.reconnect().await;
    client.start("nosuchlogin", "velocitysql").await;
    // No password is stored, so the challenge is a decoy and the proof fails.
    let Message::AuthenticationSasl(_) = client.read().await else {
        panic!("expected an AuthenticationSASL request");
    };
    let body = {
        let mut body = Vec::new();
        body.extend_from_slice(b"SCRAM-SHA-256\0");
        let initial = "n,,n=,r=abcdef";
        body.extend_from_slice(&(initial.len() as i32).to_be_bytes());
        body.extend_from_slice(initial.as_bytes());
        body
    };
    client.send(b'p', &body).await;
    let Message::AuthenticationSaslContinue(server_first) = client.read().await else {
        panic!("expected a server-first message");
    };
    let client_final = format!(
        "c=biws,r={},p={}",
        scram::attribute(&server_first, "r").unwrap(),
        scram::base64_encode(&[0u8; 32])
    );
    client.send(b'p', client_final.as_bytes()).await;
    let Message::Error(fields) = client.read().await else {
        panic!("expected a FATAL error");
    };
    assert_eq!(fields.get("C").map(String::as_str), Some("28P01"));
}

#[tokio::test]
async fn statements_are_logged_at_the_default_level() {
    let logs = CapturedLogs::default();
    // `fmt()` starts at `INFO`, which is also what the server's own filter shows
    // when `RUST_LOG` says nothing: finding out what a client sent must not
    // require an environment variable and a restart.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_target(false)
        .finish();
    // Another test may have installed one first; the buffer would then stay
    // empty and the assertions below say so.
    let _ = tracing::subscriber::set_global_default(subscriber);

    let mut client = Client::connect().await;
    client.query_all("SELECT 42").await;
    let logged = logs.contents();
    let statement = line_containing(&logged, "SELECT 42");
    assert!(statement.contains("INFO"), "not at `info`: {statement}");
    assert_duration(statement);

    // The extended protocol is what a driver uses, and it logs twice: the
    // `Parse` (planning) and the `Execute` (with the values it bound). lib/pq
    // runs every statement this way, so this is the pair an application sees.
    let mut body = Vec::new();
    body.extend_from_slice(b"ext\0");
    body.extend_from_slice(b"SELECT 42 + $1::int\0");
    body.extend_from_slice(&0i16.to_be_bytes());
    client.send(b'P', &body).await;
    assert_eq!(client.read().await, Message::ParseComplete);
    client
        .send(b'B', &bind_body_with("ext_portal", "ext", &[Some("1")]))
        .await;
    assert_eq!(client.read().await, Message::BindComplete);
    client.send(b'E', &execute_body("ext_portal")).await;
    client.send(b'S', &[]).await;
    client.read_until_ready().await;

    let logged = logs.contents();
    let parse = line_containing(&logged, "parse statement=ext");
    assert!(parse.contains("SELECT 42 + $1::int"), "no SQL: {parse}");
    assert_duration(parse);
    let execute = line_containing(&logged, "execute sql=SELECT 42 + $1::int");
    assert!(execute.contains("$1=1"), "no bound value: {execute}");
    assert_duration(execute);

    // A failure carries its SQL too: `column "x" does not exist` is not
    // actionable when the log does not say which statement produced it. This is
    // the shape a driver's error report arrives in.
    let fields = error_fields(&client.query_all("SELECT * FROM nope").await);
    assert!(fields.contains_key("M"), "expected an error");
    let logged = logs.contents();
    let failure = line_containing(&logged, "statement failed");
    assert!(failure.contains("SELECT * FROM nope"), "no SQL: {failure}");
    assert_duration(failure);

    // A role's password is masked: the statement log is not a place to leak it.
    client
        .query_all("CREATE ROLE logged_in LOGIN PASSWORD 's3cret'")
        .await;
    let logged = logs.contents();
    assert!(
        line_containing(&logged, "CREATE ROLE logged_in").contains("PASSWORD '***'"),
        "not masked: {logged}"
    );
    assert!(!logged.contains("s3cret"), "the password leaked: {logged}");
}

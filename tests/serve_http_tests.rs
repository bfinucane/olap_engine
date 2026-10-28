//! End-to-end HTTP tests for the Plateau transport (`src/bin/serve.rs`).
//!
//! ENGINE-PROTOCOL.md deliberately leaves the transport unspecified and makes only
//! the JSON normative (`wire_tests.rs` covers the JSON). This file therefore tests
//! the things the JSON tests *cannot*:
//!
//!   * the route table and method dispatch,
//!   * HTTP status codes actually on the wire (a `WireError::status()` that is never
//!     used is worthless),
//!   * the `{ "error", "code" }` envelope as a real response body,
//!   * CORS headers when `--cors` is set,
//!   * that a request body reaches the plan layer intact (Content-Length handling),
//!   * that a catalogue persisted by `save_to_disk` is what the server serves
//!     (`--db`), i.e. the round trip a real deployment depends on.
//!
//! ONE server process per test, each torn down when the test ends.
//!
//! The server is spawned with its stderr redirected to a FILE, never a pipe, and
//! every server is owned by an RAII guard, so a failing test cannot leave a
//! listening orphan behind. (The obvious alternative — a single `OnceLock<Server>`
//! shared by all tests — leaks the child forever, because a static is never
//! dropped; that mistake was made and observed here first.)

use olap_engine::catalog::catalog::Catalog;
use olap_engine::cube::node::CellValue;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

/// Owns the server process, its database file and its log. Killed on drop.
///
/// A FRESH server per test, inside the test's own stack frame. That is deliberate:
/// the obvious alternative — one `OnceLock<Server>` shared by every test — leaks the
/// child process forever, because a static is never dropped, and it makes teardown
/// depend on which test happens to run last. Starting a server costs about 100 ms,
/// which is worth paying for teardown that cannot be forgotten.
struct Server {
    child: Child,
    db_path: PathBuf,
    log_path: PathBuf,
    base: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(&self.log_path);
    }
}

impl Server {
    /// Starts a server on a free port against a freshly written fixture.
    fn start(cors: bool) -> Server {
        let (db_path, log_path) = unique_paths("fixture");
        write_fixture(&db_path);
        let server = spawn_server(&db_path, &log_path, cors);
        // This fixture always has data; prove the server can actually read the
        // catalogue it was given, not merely that it bound a socket.
        if get_json(&server.base, "/v1/Sales").is_none() {
            let stderr = read_log(&server.log_path);
            drop(server);
            panic!("server bound but did not answer GET /v1/Sales.\nserver stderr:\n{stderr}");
        }
        server
    }

    /// Starts a server with NO catalogue file, so it comes up empty.
    ///
    /// Used by the CORS test, where the point is the transport headers and the
    /// absence of data is irrelevant.
    fn empty(cors: bool) -> Server {
        let (db_path, log_path) = unique_paths("empty");
        spawn_server(&db_path, &log_path, cors)
    }
}

/// Monotonic counter making artifact names unique within one test process.
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Two unique artifact paths per server, so parallel tests never collide. The PID
/// alone is not enough: tests run in parallel threads within ONE process.
fn unique_paths(tag: &str) -> (PathBuf, PathBuf) {
    let dir = artifact_dir();
    let n = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stem = format!("serve_{tag}_{}_{n}", std::process::id());
    (dir.join(format!("{stem}.bin")), dir.join(format!("{stem}.log")))
}

/// Spawns `serve` and waits until it ANSWERS, not merely until it listens.
///
/// Waiting for `TcpStream::connect` alone is nearly useless here: the listening
/// socket exists before the catalogue is loaded, so a request can arrive while the
/// process is still reading `--db`. The probe is `/v1/capabilities`, which needs no
/// dataset, so this works for an empty server too.
fn spawn_server(db_path: &PathBuf, log_path: &PathBuf, cors: bool) -> Server {
    spawn_server_with_security(db_path, log_path, cors, None)
}

/// Spawns a server, optionally behind authentication.
///
/// `security = None` runs it in development mode (`--allow-anonymous`): this suite is
/// about the data transport, and every one of those tests would otherwise need a login.
/// The auth tests use `Some(path)` and get the REQUIRED behaviour the binary defaults to.
fn spawn_server_with_security(
    db_path: &PathBuf,
    log_path: &PathBuf,
    cors: bool,
    security: Option<&PathBuf>,
) -> Server {
    let exe = env!("CARGO_BIN_EXE_serve");
    let log = std::fs::File::create(log_path).expect("create server log");

    // ONE attempt. A retry loop on a taken port is not worth the diagnostic cost:
    // probe-then-bind loses to essentially nothing on a developer machine, and a
    // silent retry loop turns "the server never came up" into minutes of nothing.
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");

    let mut command = Command::new(exe);
    command
        .args(["--bind", &format!("127.0.0.1:{port}"), "--db"])
        .arg(db_path)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log.try_clone().expect("clone log handle")));
    match security {
        Some(path) => {
            command.arg("--security").arg(path);
        }
        None => {
            command.arg("--allow-anonymous");
        }
    }
    if cors {
        command.arg("--cors");
    }

    let child = command
        .spawn()
        .unwrap_or_else(|e| panic!("could not spawn {exe}: {e}"));

    let mut server = Server {
        child,
        db_path: db_path.clone(),
        log_path: log_path.clone(),
        base,
    };

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // Behind auth, the proof of life is a 401 on a protected route. NOT
        // /v1/capabilities: that one is deliberately public so a client can discover
        // authRequired before it has a token.
        let ready = match security {
            Some(_) => status_of(&server.base, "/v1/datasets") == Some(401),
            None => get_json(&server.base, "/v1/capabilities").is_some(),
        };
        if ready {
            return server;
        }
        if let Ok(Some(status)) = server.child.try_wait() {
            let stderr = read_log(&server.log_path);
            panic!("server exited early ({status}) without answering:\n{stderr}");
        }
        if Instant::now() >= deadline {
            let stderr = read_log(&server.log_path);
            drop(server);
            panic!("server never answered /v1/capabilities within 20s:\n{stderr}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The HTTP status of a GET, or `None` if the port is not answering.
fn status_of(base: &str, path: &str) -> Option<u16> {
    let addr = base.trim_start_matches("http://");
    TcpStream::connect(addr).ok()?;
    Some(request("GET", &format!("{base}{path}"), None).status)
}

/// A server with CORS enabled — what every routing test uses.
fn server() -> Server {
    Server::start(true)
}

/// Where this suite keeps its fixture database and server logs.
///
/// NOT the OS temp directory: the test binary runs under a file sandbox that
/// permits writes only under the session workspace, and `save_to_disk` into
/// `%TEMP%` fails with `PermissionDenied` (os error 5). `OLAP_TEST_ARTIFACTS`
/// overrides the location; the default mirrors the session layout so the suite
/// works as-is where it was written.
///
/// The PID is appended so two CONCURRENT `cargo test` runs cannot share file
/// names. Another session may be developing this engine at the same time, and a
/// shared artifact directory would produce failures that look like engine bugs.
fn artifact_dir() -> PathBuf {
    let base = match std::env::var_os("OLAP_TEST_ARTIFACTS") {
        Some(v) => PathBuf::from(v),
        None => {
            let home = std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            home.join("Documents")
                .join("deepseek-harness")
                .join("default-workspace")
                .join("olap-test-artifacts")
        }
    };
    let dir = base.join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
        panic!(
            "could not create the artifact directory {}: {e}. \
             Set OLAP_TEST_ARTIFACTS to a writable path.",
            dir.display()
        )
    });
    dir
}

/// Builds the fixture catalogue into `path`.

/// An ephemeral port, obtained by binding and immediately releasing it.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    l.local_addr().expect("local addr").port()
}

fn read_log(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The fixture, identical in shape to `wire_tests.rs` so the expected numbers are
/// the same in both files.
fn write_fixture(path: &PathBuf) {
    let mut catalog = Catalog::new();
    catalog.add_cube("Sales", &["Region", "Product", "Measure"], Some("Measure"), true);
    catalog.add_cube("Markets", &["Region"], None, true);

    {
        let cube = catalog.get_cube_mut("Sales").expect("Sales cube");
        let rows: [(&str, &str, &str, f64); 8] = [
            ("East", "Widget", "Sales", 100.0),
            ("East", "Widget", "Cost", 40.0),
            ("East", "Gadget", "Sales", 50.0),
            ("East", "Gadget", "Cost", 30.0),
            ("West", "Widget", "Sales", 200.0),
            ("West", "Widget", "Cost", 80.0),
            ("West", "Gadget", "Sales", 25.0),
            ("West", "Gadget", "Cost", 15.0),
        ];
        for (r, p, m, v) in rows {
            cube.write(&[r, p, m], CellValue::Numeric(v)).expect("write");
        }

        // FIXTURE SELF-CHECK — see the same check in `wire_tests.rs`. The import path
        // is under active development and shares this write API, so prove the
        // fixture's cells are readable, not merely written, before a test blames the
        // transport for a number that never landed.
        for (r, p, m, v) in rows {
            match cube.read_cell(&[r, p, m]) {
                Some(CellValue::Numeric(got)) => assert_eq!(
                    got, v,
                    "fixture self-check: {r}/{p}/{m} reads back {got}, expected {v}"
                ),
                other => panic!(
                    "fixture self-check: {r}/{p}/{m} did not read back as a numeric cell \
                     (got {other:?})"
                ),
            }
        }
    }
    {
        let cube = catalog.get_cube_mut("Markets").expect("Markets cube");
        cube.write(&["East"], CellValue::Numeric(10.0)).expect("write");
        cube.write(&["West"], CellValue::Numeric(20.0)).expect("write");
    }

    catalog.save_to_disk(path.to_str().expect("utf-8 path")).expect("save fixture");
}

// ---------------------------------------------------------------------------
// A minimal HTTP/1.1 client (no dependencies)
// ---------------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {:?}", self.body))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_string()
    }
}

/// Sends one request over a fresh connection (`Connection: close`). `body` is
/// sent as `application/json` when present.
fn request(method: &str, url: &str, body: Option<&str>) -> HttpResponse {
    request_with_auth(method, url, body, None)
}

/// As `request`, but carries a bearer token. `auth` is the full header value
/// (`Bearer <token>`), so the test can also send a malformed one.
fn request_with_auth(method: &str, url: &str, body: Option<&str>, auth: Option<&str>) -> HttpResponse {
    let addr = url.trim_start_matches("http://");
    let (authority, target) = match addr.split_once('/') {
        Some((a, t)) => (a, format!("/{t}")),
        None => (addr, "/".to_string()),
    };

    let mut stream = TcpStream::connect(authority)
        .unwrap_or_else(|e| panic!("connect {authority}: {e}"));
    stream.set_read_timeout(Some(Duration::from_secs(20))).expect("read timeout");

    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n"
    );
    if let Some(header) = auth {
        head.push_str(&format!("Authorization: {header}\r\n"));
    }
    if let Some(b) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).expect("write head");
    if let Some(b) = body {
        stream.write_all(b.as_bytes()).expect("write body");
    }
    stream.flush().expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    parse_response(&String::from_utf8_lossy(&raw))
}

fn parse_response(raw: &str) -> HttpResponse {
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("response has no header/body separator: {raw:?}"));

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status code in {status_line:?}"));

    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();

    HttpResponse { status, headers, body: body.to_string() }
}

fn get(url: &str) -> HttpResponse {
    request("GET", url, None)
}

fn post_json(url: &str, body: &str) -> HttpResponse {
    request("POST", url, Some(body))
}

/// `Some(json)` when GET succeeds, `None` when the server is not answering yet.
fn get_json(base: &str, path: &str) -> Option<serde_json::Value> {
    let addr = base.trim_start_matches("http://");
    TcpStream::connect(addr).ok()?;
    let resp = request("GET", &format!("{base}{path}"), None);
    (resp.status == 200).then(|| resp.json())
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[test]
fn get_capabilities_returns_200_and_the_declaration() {
    let srv = server();
    let resp = get(&format!("{}/v1/capabilities", srv.base));
    assert_eq!(resp.status, 200, "body: {}", resp.body);
    assert_eq!(resp.header("Content-Type"), Some("application/json"));

    let v = resp.json();
    assert_eq!(v["protocolVersion"], serde_json::json!(1));
    assert_eq!(v["supportsLod"], serde_json::json!(false));
    assert!(v["aggregations"].as_array().unwrap().contains(&serde_json::json!("SUM")));
}

#[test]
fn get_dataset_returns_the_catalog_response() {
    let srv = server();
    let resp = get(&format!("{}/v1/Sales", srv.base));
    assert_eq!(resp.status, 200, "body: {}", resp.body);

    let v = resp.json();
    assert_eq!(v["dataset"], serde_json::json!("Sales"));
    assert_eq!(v["rowCount"], serde_json::json!(8));
    assert_eq!(v["columns"].as_array().unwrap().len(), 3);
    assert_eq!(v["columns"][2]["suggestedKind"], serde_json::json!("measure"));
}

/// `GET /v1/datasets` is how Plateau DISCOVERS the cube names it may query: it
/// cannot guess them. The names must round-trip in their EXACT stored casing
/// (a caller does `GET /v1/{name}` with them verbatim), so a case-folded or
/// reordered answer is a real bug, not cosmetics.
#[test]
fn get_datasets_lists_the_catalog_names_in_stored_casing_sorted() {
    let srv = server();
    let resp = get(&format!("{}/v1/datasets", srv.base));
    assert_eq!(resp.status, 200, "body: {}", resp.body);
    assert_eq!(resp.header("Content-Type"), Some("application/json"));

    // The fixture builds `Sales` and `Markets`; the response is sorted. The `#` system
    // cubes are listed too (a client discovers the database through this list, and
    // `isSystem` in describe is what a cube picker filters on); `#` sorts first.
    assert_eq!(
        resp.json()["datasets"],
        serde_json::json!(["#Columns", "#Cubes", "#DimensionSecurity", "#Dimensions", "#Edges", "#Hierarchies", "#Members", "#TableSecurity", "Markets", "Sales"])
    );

    // Every advertised name must actually resolve as a dataset (the round trip
    // the client depends on), and the routing must NOT mistake `datasets` for a
    // dataset named "datasets".
    for name in ["Markets", "Sales"] {
        assert_eq!(
            get(&format!("{}/v1/{name}", srv.base)).status,
            200,
            "advertised dataset '{name}' must be describable"
        );
    }
}

/// The dataset list is a GET-only route; a POST must be refused with the
/// protocol's `invalid_argument`, not silently answered.
#[test]
fn post_to_datasets_is_rejected() {
    let srv = server();
    let resp = request("POST", &format!("{}/v1/datasets", srv.base), Some("{}"));
    assert_eq!(resp.status, 400, "body: {}", resp.body);
    assert_eq!(resp.code(), "invalid_argument");
}

#[test]
fn get_sample_honours_limit_offset_and_columns() {
    let srv = server();
    let b = &srv.base;

    let resp = get(&format!("{b}/v1/Sales/sample?limit=3"));
    assert_eq!(resp.status, 200, "body: {}", resp.body);
    let v = resp.json();
    assert_eq!(v["rowCount"], serde_json::json!(8), "total, not the page length");
    assert_eq!(v["rows"].as_array().unwrap().len(), 3);

    let windowed = get(&format!("{b}/v1/Sales/sample?limit=2&offset=2")).json();
    let full = get(&format!("{b}/v1/Sales/sample?limit=8")).json();
    assert_eq!(windowed["rows"][0], full["rows"][2], "offset skips rows in the transport too");

    let subset = get(&format!("{b}/v1/Sales/sample?limit=2&columns=Product")).json();
    assert_eq!(subset["columns"], serde_json::json!(["Product"]));
    for row in subset["rows"].as_array().unwrap() {
        assert_eq!(row.as_array().unwrap().len(), 1);
    }
}

// ---------------------------------------------------------------------------
// Query-string percent-decoding (`sample?columns=…`)
//
// A column name is DATA, and a client URL-encodes data: `URLSearchParams` writes
// "Row ID" as `Row+ID`, `encodeURIComponent` writes it as `Row%20ID`. The server
// used to take query values literally, so `columns=Row+ID` named a column called
// "Row+ID" that does not exist and answered 400. Plateau sends these requests, so
// the two encodings MUST name the same column, and a malformed escape must be a
// clean 400 rather than a panic or a silently different name.
// ---------------------------------------------------------------------------

/// A cube whose column names are the ones a client has to URL-encode.
///
/// "Row ID" is the multi-word column name the protocol's own `describe` example
/// uses, so it is the right probe: it needs an encoding in every client.
fn write_spaced_column_fixture(path: &PathBuf) {
    let mut catalog = Catalog::new();
    catalog.add_cube("Spaced", &["Region", "Row ID"], None, true);
    {
        let cube = catalog.get_cube_mut("Spaced").expect("Spaced cube");
        cube.write(&["East", "r-1"], CellValue::Numeric(7.0)).expect("write r-1");
        cube.write(&["West", "r-2"], CellValue::Numeric(9.0)).expect("write r-2");
    }
    catalog.save_to_disk(path.to_str().expect("utf-8 path")).expect("save spaced fixture");
}

/// A server holding `Spaced`, proved ready by a describe of THAT cube (not just by
/// a bound socket), so a decode failure cannot be mistaken for a startup failure.
fn server_with_spaced_columns() -> Server {
    let (db_path, log_path) = unique_paths("spaced");
    write_spaced_column_fixture(&db_path);
    let server = spawn_server(&db_path, &log_path, false);
    if get_json(&server.base, "/v1/Spaced").is_none() {
        let stderr = read_log(&server.log_path);
        drop(server);
        panic!("server bound but did not answer GET /v1/Spaced.\nserver stderr:\n{stderr}");
    }
    server
}

/// Every encoding a real client produces for `columns=Row ID` resolves to the one
/// column named exactly "Row ID". Covers `+` (form-style), `%20` (URI-style) and
/// hex case-insensitivity (`%52` = 'R', `%72` = 'r'; the column lookup is
/// case-insensitive, so both must work).
#[test]
fn sample_columns_percent_decode_plus_and_escaped_forms() {
    let srv = server_with_spaced_columns();
    let b = &srv.base;

    for encoded in ["Row+ID", "Row%20ID", "%52ow+ID", "%72ow%20ID"] {
        let resp = get(&format!("{b}/v1/Spaced/sample?limit=5&columns={encoded}"));
        assert_eq!(resp.status, 200, "columns={encoded}: {}", resp.body);
        let v = resp.json();
        assert_eq!(v["columns"], serde_json::json!(["Row ID"]), "columns={encoded}");
        assert_eq!(v["rowCount"], serde_json::json!(2), "columns={encoded}");
        let rows = v["rows"].as_array().expect("rows is an array");
        assert_eq!(rows.len(), 2, "columns={encoded}");
        // Row ORDER inside a sample is not part of the contract (the store is a hash
        // map), so assert the decoded projection, not a position.
        let ids: Vec<&str> = rows.iter().filter_map(|row| row[0].as_str()).collect();
        assert!(
            ids.contains(&"r-1") && ids.contains(&"r-2"),
            "columns={encoded}: both rows must carry the decoded column: {rows:?}"
        );
    }

    // The decode is applied to EVERY value, not just columns: an encoded offset is
    // still an offset (and `%31` is how a client may escape a digit).
    let windowed = get(&format!("{b}/v1/Spaced/sample?limit=5&offset=%31&columns=Row+ID"));
    assert_eq!(windowed.status, 200, "{}", windowed.body);
    let rows = windowed.json()["rows"].as_array().cloned().unwrap_or_default();
    assert_eq!(rows.len(), 1, "offset=%31 must decode to 1: {rows:?}");
    assert!(
        matches!(rows[0][0].as_str(), Some("r-1" | "r-2")),
        "decoded offset must return the remaining row: {rows:?}"
    );

    // A LITERAL plus is `%2B`, never a space: decoding must not conflate the two.
    // "Row+ID" is not a column here, so this is the unknown-column error.
    let literal_plus = get(&format!("{b}/v1/Spaced/sample?limit=5&columns=Row%2BID"));
    assert_eq!(literal_plus.status, 400, "{}", literal_plus.body);
    assert_eq!(literal_plus.code(), "invalid_argument");
    assert!(
        literal_plus.json()["error"].as_str().unwrap_or("").contains("Row+ID"),
        "the literal plus must survive decoding: {}",
        literal_plus.body
    );
}

/// A malformed escape is `invalid_argument` — a clean envelope, not a panic and not
/// a silently truncated/garbled name. Covers a non-hex pair, a truncated escape, a
/// lone `%`, and `%FF`, which is valid hex but not valid UTF-8.
#[test]
fn malformed_percent_encoding_in_a_query_value_is_a_clean_400() {
    let srv = server_with_spaced_columns();
    let b = &srv.base;

    for bad in ["Row%ZZ", "Row%2", "Row%", "%FF"] {
        let resp = get(&format!("{b}/v1/Spaced/sample?limit=5&columns={bad}"));
        assert_eq!(resp.status, 400, "columns={bad}: {}", resp.body);
        assert_eq!(resp.code(), "invalid_argument", "columns={bad}: {}", resp.body);
        assert!(
            resp.json()["error"].as_str().is_some_and(|s| !s.is_empty()),
            "columns={bad}: the envelope must carry a message: {}",
            resp.body
        );
    }

    // No panic, no wedged connection handling: ordinary requests still work after
    // four refusals on the same process.
    assert_eq!(get(&format!("{b}/v1/capabilities")).status, 200);
    assert_eq!(get(&format!("{b}/v1/Spaced/sample?limit=1")).status, 200);
}

#[test]
fn post_query_returns_the_full_query_response() {
    let srv = server();
    let body = serde_json::json!({
        "dataset": "Sales",
        "protocolVersion": 1,
        "groupBy": [{ "name": "Region" }],
        "measures": [
            { "name": "Sales", "aggregation": "SUM" },
            { "name": "Cost", "aggregation": "SUM" }
        ]
    })
    .to_string();

    let resp = post_json(&format!("{}/v1/Sales/query", srv.base), &body);
    assert_eq!(resp.status, 200, "body: {}", resp.body);

    let v = resp.json();
    assert_eq!(v["columns"], serde_json::json!(["Region", "Sales", "Cost"]));
    assert_eq!(
        v["measureLabels"],
        serde_json::json!(["SUM(Sales)", "SUM(Cost)"]),
        "the measure-position invariant must survive serialisation and transport"
    );
    assert_eq!(v["rows"], serde_json::json!([["East", 150.0, 70.0], ["West", 225.0, 95.0]]));
    assert_eq!(v["dimensionNames"], serde_json::json!(["Region"]));
    assert_eq!(v["measureNames"], serde_json::json!(["Sales", "Cost"]));
    assert_eq!(v["truncated"], serde_json::json!(false));
}

// ---------------------------------------------------------------------------
// Errors on the wire
// ---------------------------------------------------------------------------

#[test]
fn error_responses_carry_the_envelope_and_the_right_status() {
    let srv = server();
    let b = &srv.base;

    let cases: [(&str, &str, u16, &str); 8] = [
        ("/v1/Nope", "GET", 404, "unknown_dataset"),
        ("/v1/Nope/sample?limit=2", "GET", 404, "unknown_dataset"),
        ("/v1/Sales/sample", "GET", 400, "invalid_argument"),
        ("/v1/Sales/bogus", "GET", 400, "invalid_argument"),
        ("/nope", "GET", 400, "invalid_argument"),
        ("/v1/capabilities", "POST", 400, "invalid_argument"),
        ("/v1/Sales/query", "GET", 400, "invalid_argument"),
        ("/v1/Sales/sample?limit=abc", "GET", 400, "invalid_argument"),
    ];

    for (path, method, want_status, want_code) in cases {
        let resp = request(method, &format!("{b}{path}"), if method == "POST" { Some("{}") } else { None });
        assert_eq!(resp.status, want_status, "{method} {path}: body {}", resp.body);
        assert_eq!(resp.code(), want_code, "{method} {path}: body {}", resp.body);

        // §Errors: exactly two keys, both non-empty strings.
        let v = resp.json();
        let obj = v.as_object().expect("envelope is an object");
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["code", "error"], "{method} {path}: envelope keys");
        assert!(
            v["error"].as_str().is_some_and(|s| !s.is_empty()),
            "{method} {path}: error message must not be empty"
        );
    }
}

#[test]
fn post_query_error_cases_report_their_codes() {
    let srv = server();
    let b = &srv.base;

    // Version mismatch: the client is speaking a version we do not.
    let resp = post_json(
        &format!("{b}/v1/Sales/query"),
        &serde_json::json!({ "dataset": "Sales", "protocolVersion": 99 }).to_string(),
    );
    assert_eq!(resp.status, 400);
    assert_eq!(resp.code(), "version_mismatch");

    // Path and body disagree about the dataset — a client bug, refused.
    let resp = post_json(
        &format!("{b}/v1/Markets/query"),
        &serde_json::json!({ "dataset": "Sales", "protocolVersion": 1 }).to_string(),
    );
    assert_eq!(resp.status, 400);
    assert_eq!(resp.code(), "invalid_argument");

    // Malformed JSON body.
    let resp = post_json(&format!("{b}/v1/Sales/query"), "{\"dataset\":");
    assert_eq!(resp.status, 400);
    assert_eq!(resp.code(), "invalid_argument");

    // A body with no dataset at all.
    let resp = post_json(
        &format!("{b}/v1/Sales/query"),
        &serde_json::json!({ "protocolVersion": 1 }).to_string(),
    );
    assert_eq!(resp.status, 400);
    assert_eq!(resp.code(), "invalid_argument");

    // An unknown dataset named in the body (path matches, so it reaches the plan).
    let resp = post_json(
        &format!("{b}/v1/Nope/query"),
        &serde_json::json!({ "dataset": "Nope", "protocolVersion": 1 }).to_string(),
    );
    assert_eq!(resp.status, 404);
    assert_eq!(resp.code(), "unknown_dataset");
}

/// A `POST` body larger than one TCP segment must still arrive intact: the server
/// reads exactly `Content-Length` bytes. A silent truncation here would turn a
/// large query into a parse error only under load.
#[test]
fn post_body_larger_than_one_segment_is_read_completely() {
    let srv = server();
    // Pad the request with many no-op filters so the body is comfortably large.
    let filters: Vec<serde_json::Value> = (0..400)
        .map(|i| {
            serde_json::json!({
                "id": format!("pad-{i}"),
                "field": "Region",
                "scope": "dimension",
                "operator": "in",
                "values": ["East", "West"]
            })
        })
        .collect();

    let body = serde_json::json!({
        "dataset": "Sales",
        "protocolVersion": 1,
        "groupBy": [{ "name": "Region" }],
        "measures": [{ "name": "Sales", "aggregation": "SUM" }],
        "filters": filters
    })
    .to_string();
    assert!(body.len() > 16_384, "the body should exceed one segment: {} bytes", body.len());

    let resp = post_json(&format!("{}/v1/Sales/query", srv.base), &body);
    assert_eq!(resp.status, 200, "large body must round trip: {}", resp.body);
    assert_eq!(
        resp.json()["rows"],
        serde_json::json!([["East", 150.0], ["West", 225.0]]),
        "400 duplicate in-filters must all apply and leave the total correct"
    );
}

// ---------------------------------------------------------------------------
// Transport details
// ---------------------------------------------------------------------------

#[test]
fn cors_headers_are_present_when_enabled_and_absent_otherwise() {
    let srv = server();
    let b = &srv.base;

    // `--cors` was passed for this fixture.
    let resp = get(&format!("{b}/v1/capabilities"));
    assert_eq!(
        resp.header("Access-Control-Allow-Origin"),
        Some("*"),
        "--cors must emit the origin header"
    );

    // The preflight must succeed without reaching the route table.
    let preflight = request("OPTIONS", &format!("{b}/v1/Sales/query"), None);
    assert_eq!(preflight.status, 204, "preflight is 204");
    assert_eq!(preflight.header("Access-Control-Allow-Methods"), Some("GET, POST, OPTIONS"));
    // Authorization MUST be advertised: a browser sending a bearer token preflights, and a
    // header absent from this list means the authenticated request never leaves.
    assert_eq!(
        preflight.header("Access-Control-Allow-Headers"),
        Some("Content-Type, Authorization"),
        "a bearer token needs its header allowed through CORS"
    );
    assert!(preflight.body.is_empty(), "204 carries no body");

    // A server WITHOUT --cors emits none of them. A second, differently configured
    // process with no catalogue at all, also torn down by its guard.
    let plain_server = Server::empty(false);

    let plain = request("GET", &format!("{}/v1/capabilities", plain_server.base), None);
    assert_eq!(plain.status, 200);
    assert_eq!(
        plain.header("Access-Control-Allow-Origin"),
        None,
        "CORS is off by default"
    );
}

#[test]
fn trailing_slash_and_query_string_do_not_break_routing() {
    let srv = server();
    let b = &srv.base;

    // A trailing slash is stripped before matching.
    let s = get(&format!("{b}/v1/Sales/"));
    assert_eq!(s.status, 200, "trailing slash on describe: {}", s.body);
    assert_eq!(s.json()["dataset"], serde_json::json!("Sales"));

    // An unknown query parameter is ignored, not fatal.
    let u = get(&format!("{b}/v1/Sales/sample?limit=2&bogus=1"));
    assert_eq!(u.status, 200, "unknown params ignored: {}", u.body);
    assert_eq!(u.json()["rows"].as_array().unwrap().len(), 2);
}

#[test]
fn server_serves_the_catalog_persisted_by_save_to_disk() {
    // This is the deployment path: `--db` loads what `save_to_disk` wrote. Both
    // datasets and all 8 leaf cells must survive the bincode round trip and the
    // server's dimension re-attachment.
    let srv = server();
    let b = &srv.base;

    let sales = get(&format!("{b}/v1/Sales")).json();
    assert_eq!(sales["rowCount"], serde_json::json!(8));

    let markets = get(&format!("{b}/v1/Markets"));
    assert_eq!(markets.status, 200, "second dataset also loaded: {}", markets.body);
    assert_eq!(markets.json()["rowCount"], serde_json::json!(2));
}

/// Phase 1b M3: the server no longer holds a catalog-wide `Mutex`, so requests from
/// many threads must all be served, with consistent answers, and none may block another
/// behind a global lock.
///
/// This is a concurrency test over the TRANSPORT, not a throughput benchmark: it exists
/// to prove there is no deadlock and no shared-state corruption once the coarse lock is
/// gone. (These requests are all READS; the write endpoint has its own tests, and the
/// reader/writer race itself is covered in `concurrency_tests.rs`, against the same
/// `VersionStore`.)
#[test]
fn concurrent_requests_are_all_served_consistently() {
    let srv = server();
    let describe_url = format!("{}/v1/Sales", srv.base);
    let query_url = format!("{}/v1/Sales/query", srv.base);
    let body = r#"{"dataset":"Sales","protocolVersion":1,
                   "groupBy":[{"name":"Region"}],
                   "measures":[{"name":"Sales","aggregation":"SUM"}]}"#;

    let clients: Vec<_> = (0..8)
        .map(|_| {
            let describe_url = describe_url.clone();
            let query_url = query_url.clone();
            std::thread::spawn(move || {
                for i in 0..10 {
                    let d = get(&describe_url);
                    assert_eq!(d.status, 200, "describe failed on iteration {i}: {}", d.body);
                    assert_eq!(d.json()["dataset"], serde_json::json!("Sales"));

                    let q = post_json(&query_url, body);
                    assert_eq!(q.status, 200, "query failed on iteration {i}: {}", q.body);
                    let rows = q.json()["rows"].as_array().expect("rows is an array").clone();
                    assert_eq!(rows.len(), 2, "East and West, every time");
                    // East Sales = 100 + 50, independent of any other request's timing.
                    assert_eq!(rows[0][1], serde_json::json!(150.0));
                }
            })
        })
        .collect();

    for client in clients {
        client.join().expect("client thread must not panic");
    }
}

// ---------------------------------------------------------------------------
// The read-only SQL console (GET /sql, POST /v1/sql)
// ---------------------------------------------------------------------------

#[test]
fn the_two_sql_urls_point_at_each_other_when_used_wrongly() {
    // This is the mistake a browser makes: /v1/sql is the JSON endpoint, /sql is the
    // page. Both wrong-method answers must send the reader to the other URL, because
    // "Method not allowed; use POST." is true and useless.
    let srv = server();

    let api = get(&format!("{}/v1/sql", srv.base));
    assert_eq!(api.status, 400, "GET on the POST endpoint");
    assert_eq!(api.code(), "invalid_argument");
    assert!(
        api.body.contains("/sql"),
        "the API's wrong-method answer must point at the page: {}",
        api.body
    );

    let page = post_json(&format!("{}/sql", srv.base), r#"{"sql":"SELECT 1"}"#);
    assert_eq!(page.status, 400, "POST on the page URL");
    assert!(
        page.body.contains("/v1/sql"),
        "the page's wrong-method answer must point at the endpoint: {}",
        page.body
    );
}

#[test]
fn get_sql_serves_the_console_page() {
    let srv = server();
    let resp = get(&format!("{}/sql", srv.base));

    assert_eq!(resp.status, 200, "console page: {}", resp.body);
    assert_eq!(
        resp.header("content-type"),
        Some("text/html; charset=utf-8"),
        "the console is the one HTML response the server serves"
    );
    assert!(resp.body.contains("<textarea"), "it needs an input box");
    assert!(
        resp.body.to_lowercase().contains("read-only"),
        "the page must say it is read-only before the user tries an INSERT"
    );
    assert!(resp.body.contains("/v1/sql"), "it posts to the SQL endpoint");
}

#[test]
fn post_sql_runs_a_select() {
    let srv = server();
    // NOTE: `value` pivots ALL measures into columns, so it cannot be combined with the
    // measure dimension as an axis. This query is the pivot form: one column per measure
    // member (Sales, Cost), one row per Region x Product.
    let resp = post_json(
        &format!("{}/v1/sql", srv.base),
        r#"{"sql":"SELECT Region, Product, value FROM Sales"}"#,
    );

    assert_eq!(resp.status, 200, "select via the console: {}", resp.body);
    let out = resp.json()["output"].as_str().expect("output is a string").to_string();
    // The fixture has East/Widget, East/Gadget, West/Widget, West/Gadget.
    assert!(out.contains("Region"), "header row missing:\n{out}");
    assert!(out.contains("East") && out.contains("West"), "rows missing:\n{out}");
    assert!(out.contains("Widget"), "products missing:\n{out}");
    // The pivot: measure members became COLUMNS, not a Measure column.
    assert!(
        out.contains("Sales") && out.contains("Cost"),
        "value should expand to one column per measure:\n{out}"
    );
}

#[test]
fn post_sql_refuses_anything_but_a_select() {
    let srv = server();

    for (sql, kind) in [
        ("DROP TABLE Sales", "DROP"),
        ("INSERT INTO Sales VALUES ('East','Widget','Sales',1)", "INSERT"),
        ("CREATE TABLE T (A STRING, M MEASURE)", "CREATE TABLE"),
        ("SELECT 1; DROP TABLE Sales", "one statement"),
    ] {
        let body = serde_json::json!({ "sql": sql }).to_string();
        let resp = post_json(&format!("{}/v1/sql", srv.base), &body);
        assert_eq!(resp.status, 400, "must refuse: {sql}");
        assert_eq!(resp.code(), "invalid_argument", "wrong code for: {sql}");
        assert!(
            resp.body.contains(kind),
            "the message should mention {kind}: {}",
            resp.body
        );
    }
}

#[test]
fn the_sql_console_cannot_change_the_catalogue() {
    let srv = server();
    let before = get(&format!("{}/v1/datasets", srv.base)).json();

    // A refused write, then a legitimate read: the served catalogue must be untouched.
    let _ = post_json(
        &format!("{}/v1/sql", srv.base),
        r#"{"sql":"INSERT INTO Sales VALUES ('Nowhere','Nothing','Sales',999)"}"#,
    );
    let _ = post_json(
        &format!("{}/v1/sql", srv.base),
        r#"{"sql":"SELECT Region, Product, Measure, value FROM Sales"}"#,
    );

    let after = get(&format!("{}/v1/datasets", srv.base)).json();
    assert_eq!(before, after, "the dataset list must be unchanged");
    assert_eq!(
        get(&format!("{}/v1/Sales", srv.base)).json()["rowCount"],
        serde_json::json!(8),
        "still the 8 fixture cells"
    );
}

/// A `#`-prefixed dataset must be addressed as `%23` over HTTP.
///
/// `#` begins a URL fragment, so a raw one never reaches a server; without path decoding
/// a system cube would be reachable in SQL but not over the wire.
#[test]
fn a_hash_prefixed_dataset_needs_percent_encoding() {
    let srv = server();

    let resp = get(&format!("{}/v1/%23Cubes", srv.base));
    assert_eq!(resp.status, 200, "the request must route AND resolve: {}", resp.body);
    let body = resp.json();
    assert_eq!(body["dataset"], serde_json::json!("#Cubes"));
    // Flagged, so a UI can hide it while the client can still query it.
    assert_eq!(body["isSystem"], serde_json::json!(true));

    // An ordinary dataset is unaffected by decoding.
    assert_eq!(get(&format!("{}/v1/Sales", srv.base)).status, 200);
}
// ---------------------------------------------------------------------------
// S1: authentication (SECURITY-DESIGN.md §5)
// ---------------------------------------------------------------------------

impl Server {
    /// Starts a server that REQUIRES authentication, with the given users:
    /// `(name, password, roles)`.
    fn authenticated(users: &[(&str, &str, &[&str])]) -> Server {
        let (db_path, log_path) = unique_paths("authdb");
        write_fixture(&db_path);

        let security_path = log_path.with_extension("security.json");
        let mut file = olap_engine::security::store::SecurityFile::default();
        for (name, password, roles) in users {
            let roles: Vec<String> = roles.iter().map(|role| role.to_string()).collect();
            file.upsert_user(name, password, &roles).expect("create user");
        }
        file.save(&security_path).expect("save security file");

        spawn_server_with_security(&db_path, &log_path, true, Some(&security_path))
    }
}

fn login(base: &str, user: &str, password: &str) -> HttpResponse {
    post_json(
        &format!("{base}/v1/login"),
        &format!("{{\"user\":\"{user}\",\"password\":\"{password}\"}}"),
    )
}

fn auth_header(resp: &HttpResponse) -> String {
    let body = resp.json();
    let token = body["token"]
        .as_str()
        .unwrap_or_else(|| panic!("login response has no token: {}", resp.body));
    format!("Bearer {token}")
}

/// Without a token, every protected route answers 401 and says nothing else.
#[test]
fn a_request_without_a_token_is_unauthorised() {
    let srv = Server::authenticated(&[("alice", "wonderland", &["reader"])]);

    let resp = get(&format!("{}/v1/datasets", srv.base));
    assert_eq!(resp.status, 401, "body: {}", resp.body);
    assert_eq!(resp.code(), "unauthorized");
    assert!(
        !resp.body.contains("Sales"),
        "an unauthenticated response must not leak dataset names: {}",
        resp.body
    );

    // Capabilities is the ONE public data-adjacent route (feature flags only).
    assert_eq!(get(&format!("{}/v1/capabilities", srv.base)).status, 200);

    // An unknown route is indistinguishable from a known one: no structure leak.
    let unknown = get(&format!("{}/v1/does-not-exist", srv.base));
    assert_eq!(unknown.status, 401, "auth must run BEFORE routing: {}", unknown.body);
    assert_eq!(unknown.code(), "unauthorized");
}

/// A successful login yields a token that unlocks the data routes.
#[test]
fn login_issues_a_token_that_unlocks_the_api() {
    let srv = Server::authenticated(&[("alice", "wonderland", &["reader"])]);

    let resp = login(&srv.base, "alice", "wonderland");
    assert_eq!(resp.status, 200, "body: {}", resp.body);
    let body = resp.json();
    assert_eq!(body["user"], serde_json::json!("alice"));
    assert_eq!(body["roles"], serde_json::json!(["reader"]));
    assert_eq!(body["token"].as_str().map(str::len), Some(64), "32 bytes as hex");
    assert!(body["expiresAt"].as_u64().unwrap_or(0) > 0);

    let header = auth_header(&resp);
    let describe = request_with_auth("GET", &format!("{}/v1/Sales", srv.base), None, Some(&header));
    assert_eq!(describe.status, 200, "tokened request: {}", describe.body);
    assert_eq!(describe.json()["dataset"], serde_json::json!("Sales"));

    // The same route stays closed without it.
    assert_eq!(get(&format!("{}/v1/Sales", srv.base)).status, 401);
}

/// A wrong password and an unknown user must be INDISTINGUISHABLE: anything else lets an
/// attacker enumerate valid user names.
#[test]
fn a_wrong_password_and_an_unknown_user_are_indistinguishable() {
    let srv = Server::authenticated(&[("alice", "wonderland", &["reader"])]);

    let wrong_password = login(&srv.base, "alice", "not-the-password");
    let unknown_user = login(&srv.base, "nobody", "not-the-password");

    assert_eq!(wrong_password.status, 401);
    assert_eq!(unknown_user.status, 401);
    assert_eq!(wrong_password.code(), "unauthorized");
    assert_eq!(unknown_user.code(), "unauthorized");
    assert_eq!(
        wrong_password.body, unknown_user.body,
        "the two failures must look identical"
    );
}

/// A made-up or malformed token is rejected like a missing one.
#[test]
fn a_garbage_token_is_rejected() {
    let srv = Server::authenticated(&[("alice", "wonderland", &["reader"])]);
    let url = format!("{}/v1/datasets", srv.base);

    for header in ["Bearer deadbeef", "Bearer ", "Basic Zm9vOmJhcg==", "deadbeef"] {
        let resp = request_with_auth("GET", &url, None, Some(header));
        assert_eq!(resp.status, 401, "header {header:?}: {}", resp.body);
        assert_eq!(resp.code(), "unauthorized");
    }
}

/// Error codes are the contract: the client switches on them, not on prose.
#[test]
fn a_malformed_login_body_is_an_invalid_argument() {
    let srv = Server::authenticated(&[("alice", "wonderland", &["reader"])]);

    for body in ["not json", "{}", "{\"user\":\"alice\"}"] {
        let resp = post_json(&format!("{}/v1/login", srv.base), body);
        assert_eq!(resp.status, 400, "body {body:?}: {}", resp.body);
        assert_eq!(resp.code(), "invalid_argument");
    }

    // The login endpoint only accepts POST.
    let resp = get(&format!("{}/v1/login", srv.base));
    assert_eq!(resp.status, 405, "{}", resp.body);
}

/// Capabilities advertise whether a token is needed, so a client can show a login screen
/// instead of firing requests that all fail.
/// Capabilities must be readable WITHOUT a token: it is where a client learns that a
/// token is needed. Requiring auth here would hide `authRequired` exactly when it matters.
#[test]
fn capabilities_are_public_and_report_whether_authentication_is_required() {
    let srv = Server::authenticated(&[("alice", "wonderland", &["reader"])]);

    let resp = get(&format!("{}/v1/capabilities", srv.base));
    assert_eq!(resp.status, 200, "capabilities must not require a token: {}", resp.body);
    assert_eq!(resp.json()["authRequired"], serde_json::json!(true));
    // It still reveals nothing about the data.
    assert!(resp.json().get("datasets").is_none());
}

/// Development mode (`--allow-anonymous`) serves without a token and says so, so a client
/// written against a protected deployment still works in development.
#[test]
fn development_mode_serves_without_a_token_and_says_so() {
    let srv = server(); // the shared helper starts with --allow-anonymous
    let resp = get(&format!("{}/v1/capabilities", srv.base));
    assert_eq!(resp.status, 200);
    assert_eq!(resp.json()["authRequired"], serde_json::json!(false));
    assert_eq!(get(&format!("{}/v1/datasets", srv.base)).status, 200);
}
// ---------------------------------------------------------------------------
// S2: table access (SECURITY-DESIGN.md §3)
// ---------------------------------------------------------------------------

/// A server that requires auth AND has table rules written into the database.
///
/// `users` are `(name, password, roles)`; `rules` are `(role, cube, level)`. The catalogue is
/// loaded, the rules applied through the same API an admin's SQL INSERT reaches, and saved.
fn authenticated_server_with_rules(
    users: &[(&str, &str, &[&str])],
    rules: &[(&str, &str, &str)],
) -> Server {
    let (db_path, log_path) = unique_paths("tabledb");
    write_fixture(&db_path);

    if !rules.is_empty() {
        let mut catalog = Catalog::load_from_disk(db_path.to_str().expect("utf8 path")).expect("load fixture");
        for (role, cube, level) in rules {
            let level = olap_engine::security::access::Level::parse(level)
                .unwrap_or_else(|| panic!("bad level {level:?}"));
            olap_engine::security::access::set_rule(&mut catalog, role, cube, level)
                .expect("set rule");
        }
        catalog.save_to_disk(db_path.to_str().expect("utf8 path")).expect("save rules");
    }

    let security_path = log_path.with_extension("security.json");
    let mut file = olap_engine::security::store::SecurityFile::default();
    for (name, password, roles) in users {
        let roles: Vec<String> = roles.iter().map(|role| role.to_string()).collect();
        file.upsert_user(name, password, &roles).expect("create user");
    }
    file.save(&security_path).expect("save security file");

    spawn_server_with_security(&db_path, &log_path, true, Some(&security_path))
}

fn token_for(srv: &Server, user: &str, password: &str) -> String {
    let resp = login(&srv.base, user, password);
    assert_eq!(resp.status, 200, "login for {user}: {}", resp.body);
    auth_header(&resp)
}

/// A role whose `NONE` rule hides just one cube: hidden everywhere, and never advertised.
#[test]
fn a_none_rule_hides_the_cube_from_every_route() {
    let srv = authenticated_server_with_rules(
        &[
            ("reader", "wonderland", &["reader"]),
            ("admin", "adminpw", &["admin"]),
        ],
        &[("reader", "Sales", "NONE")],
    );

    let reader = token_for(&srv, "reader", "wonderland");
    let admin = token_for(&srv, "admin", "adminpw");

    // The admin has no rule: DEFAULT ACCEPT keeps the cube visible.
    let admin_describe = request_with_auth("GET", &format!("{}/v1/Sales", srv.base), None, Some(&admin));
    assert_eq!(admin_describe.status, 200, "admin must still see Sales");

    // The reader is refused on every route that names the cube, with the SAME answer a
    // missing cube gives: the response must not reveal that it exists.
    for path in ["/v1/Sales", "/v1/Sales/sample?limit=1", "/v1/Sales/query"] {
        let resp = if path.ends_with("/query") {
            request_with_auth(
                "POST",
                &format!("{}{path}", srv.base),
                Some(r#"{"dataset":"Sales","groupBy":[]}"#),
                Some(&reader),
            )
        } else {
            request_with_auth("GET", &format!("{}{path}", srv.base), None, Some(&reader))
        };
        assert_eq!(resp.status, 404, "{path}: {}", resp.body);
        assert_eq!(resp.code(), "unknown_dataset", "{path}: {}", resp.body);
    }

    // Indistinguishable in status and code. The MESSAGE names the requested cube (the caller
    // already knows what they asked for); what must not differ is whether the engine treats
    // it as non-existent.
    let missing = request_with_auth("GET", &format!("{}/v1/NoSuchCube", srv.base), None, Some(&reader));
    let hidden_resp = request_with_auth("GET", &format!("{}/v1/Sales", srv.base), None, Some(&reader));
    assert_eq!(missing.status, hidden_resp.status);
    assert_eq!(missing.code(), hidden_resp.code());

    // The dataset list does not advertise it, but still lists everything else.
    let listed = request_with_auth("GET", &format!("{}/v1/datasets", srv.base), None, Some(&reader));
    let names: Vec<String> = listed.json()["datasets"]
        .as_array()
        .expect("datasets array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();
    assert!(!names.contains(&"Sales".to_string()), "Sales must be hidden: {names:?}");
    assert!(names.contains(&"Markets".to_string()), "Markets stays visible: {names:?}");

    // And it does not block anything else.
    assert_eq!(
        request_with_auth("GET", &format!("{}/v1/Markets", srv.base), None, Some(&reader)).status,
        200
    );
}

/// SQL is the obvious bypass: the same query wrapped in a statement. It must be refused
/// BEFORE execution, and the error must look like a missing dataset.
#[test]
fn sql_cannot_bypass_table_access() {
    let srv = authenticated_server_with_rules(
        &[("reader", "wonderland", &["reader"])],
        &[("reader", "Sales", "NONE")],
    );
    let reader = token_for(&srv, "reader", "wonderland");

    let run = |sql: &str| {
        request_with_auth(
            "POST",
            &format!("{}/v1/sql", srv.base),
            Some(&format!("{{\"sql\":\"{sql}\"}}")),
            Some(&reader),
        )
    };

    let denied = run("SELECT Region, Product, Measure, value FROM Sales");
    assert_eq!(denied.status, 404, "SQL bypass: {}", denied.body);
    assert_eq!(denied.code(), "unknown_dataset");

    // A JOIN reaches a second cube, so both names are checked.
    let joined = run("SELECT Region, value FROM Markets JOIN Sales USING (Region)");
    assert_eq!(joined.status, 404, "join bypass: {}", joined.body);

    // Everything else still runs.
    let allowed = run("SELECT Region, value FROM Markets");
    assert_eq!(allowed.status, 200, "permitted SQL must still work: {}", allowed.body);
}

/// The user-chosen default: no rule means access. This is the Infor behaviour, and it is
/// pinned so a later "secure by default" change is a deliberate decision, not a drift.
#[test]
fn a_role_with_no_rule_keeps_full_access() {
    let srv = authenticated_server_with_rules(
        &[("analyst", "wonderland", &["analyst"])],
        &[("other", "Sales", "NONE")],
    );
    let analyst = token_for(&srv, "analyst", "wonderland");

    assert_eq!(
        request_with_auth("GET", &format!("{}/v1/Sales", srv.base), None, Some(&analyst)).status,
        200,
        "DEFAULT ACCEPT: a role with no rule sees everything"
    );

    let listed = request_with_auth("GET", &format!("{}/v1/datasets", srv.base), None, Some(&analyst));
    let names = listed.json()["datasets"].as_array().cloned().unwrap_or_default();
    assert!(
        names.iter().any(|v| v.as_str() == Some("Sales")),
        "Sales must be listed for a role with no rule"
    );
}

/// A second role must not undo a restriction. With default-accept, taking the most
/// PERMISSIVE role would make NONE impossible to enforce.
#[test]
fn a_second_ruleless_role_does_not_undo_a_restriction() {
    let srv = authenticated_server_with_rules(
        &[("readonly", "wonderland", &["reader", "analyst"])],
        &[("reader", "Sales", "NONE")],
    );
    let token = token_for(&srv, "readonly", "wonderland");
    assert_eq!(
        request_with_auth("GET", &format!("{}/v1/Sales", srv.base), None, Some(&token)).status,
        404,
        "the restriction in ANY held role binds"
    );
}
// ---------------------------------------------------------------------------
// S3: dimension access — totals change, nothing errors
// ---------------------------------------------------------------------------

/// Like `authenticated_server_with_rules`, plus dimension rules
/// `(role, dimension, member, level)`.
fn authenticated_server_with_dimension_rules(
    users: &[(&str, &str, &[&str])],
    table_rules: &[(&str, &str, &str)],
    dimension_rules: &[(&str, &str, &str, &str)],
) -> Server {
    let (db_path, log_path) = unique_paths("dimdb");
    write_fixture(&db_path);

    {
        let mut catalog = Catalog::load_from_disk(db_path.to_str().expect("utf8 path"))
            .expect("load fixture");
        for (role, cube, level) in table_rules {
            let level = olap_engine::security::access::Level::parse(level)
                .unwrap_or_else(|| panic!("bad level {level:?}"));
            olap_engine::security::access::set_rule(&mut catalog, role, cube, level)
                .expect("set table rule");
        }
        for (role, dimension, member, level) in dimension_rules {
            let level = olap_engine::security::access::Level::parse(level)
                .unwrap_or_else(|| panic!("bad level {level:?}"));
            olap_engine::security::access::set_dimension_rule(
                &mut catalog, role, dimension, member, level,
            )
            .expect("set dimension rule");
        }
        catalog.save_to_disk(db_path.to_str().expect("utf8 path")).expect("save rules");
    }

    let security_path = log_path.with_extension("security.json");
    let mut file = olap_engine::security::store::SecurityFile::default();
    for (name, password, roles) in users {
        let roles: Vec<String> = roles.iter().map(|role| role.to_string()).collect();
        file.upsert_user(name, password, &roles).expect("create user");
    }
    file.save(&security_path).expect("save security file");

    spawn_server_with_security(&db_path, &log_path, true, Some(&security_path))
}

/// A denied member disappears from rows, from the domain, and from aggregates — on every
/// front door (query, sample, SQL), and with NO error anywhere.
#[test]
fn a_denied_member_changes_totals_without_erroring() {
    let srv = authenticated_server_with_dimension_rules(
        &[("reader", "wonderland", &["reader"])],
        &[],
        &[("reader", "Region", "East", "NONE")],
    );
    let token = token_for(&srv, "reader", "wonderland");

    // --- POST query: the row is gone, the rest stays ---
    let query = r#"{"dataset":"Sales","groupBy":[{"name":"Region"}],
                    "measures":[{"name":"Sales","aggregation":"SUM"}]}"#;
    let resp = request_with_auth(
        "POST",
        &format!("{}/v1/Sales/query", srv.base),
        Some(query),
        Some(&token),
    );
    assert_eq!(resp.status, 200, "a restriction must NOT be an error: {}", resp.body);
    let body = resp.json();
    let rows = body["rows"].as_array().cloned().unwrap_or_default();
    for row in &rows {
        assert_ne!(
            row[0],
            serde_json::json!("East"),
            "a denied member must not appear: {rows:?}"
        );
    }
    assert!(
        rows.iter().any(|row| row[0] == serde_json::json!("West")),
        "allowed members stay: {rows:?}"
    );

    // --- sample: cells with the denied coordinate are not paged out to the client ---
    let sample = request_with_auth(
        "GET",
        &format!("{}/v1/Sales/sample?limit=100", srv.base),
        None,
        Some(&token),
    );
    assert_eq!(sample.status, 200, "{}", sample.body);
    let sample_rows = sample.json()["rows"].as_array().cloned().unwrap_or_default();
    for row in &sample_rows {
        assert_ne!(row[0], serde_json::json!("East"), "sample leaked East: {sample_rows:?}");
    }
    assert!(
        !sample_rows.is_empty(),
        "the allowed rows must still be served: {sample_rows:?}"
    );

    // --- SQL: the same statement must not see it either ---
    let sql = request_with_auth(
        "POST",
        &format!("{}/v1/sql", srv.base),
        Some(r#"{"sql":"SELECT Region, value FROM Sales"}"#),
        Some(&token),
    );
    assert_eq!(sql.status, 200, "{}", sql.body);
    let output = sql.json()["output"].as_str().unwrap_or("").to_string();
    assert!(output.contains("West"), "allowed rows in SQL output:\n{output}");
    assert!(
        !output.contains("East"),
        "SQL must not reveal a denied member:\n{output}"
    );
}

/// The denial applies to the DIMENSION, not to the chosen axis: grouping by Product must
/// also drop the denied Region's cells.
#[test]
fn a_denied_member_is_hidden_through_other_axes() {
    let srv = authenticated_server_with_dimension_rules(
        &[("reader", "wonderland", &["reader"])],
        &[],
        &[("reader", "Region", "East", "NONE")],
    );
    let token = token_for(&srv, "reader", "wonderland");

    let query = r#"{"dataset":"Sales","groupBy":[{"name":"Product"}],
                    "measures":[{"name":"Sales","aggregation":"SUM"}]}"#;
    let resp = request_with_auth(
        "POST",
        &format!("{}/v1/Sales/query", srv.base),
        Some(query),
        Some(&token),
    );
    assert_eq!(resp.status, 200, "{}", resp.body);
    // East contributes 150 to the Sales total; West contributes 225. With East hidden the
    // Product totals must reflect only West's cells.
    let rows = resp.json()["rows"].as_array().cloned().unwrap_or_default();
    let total: f64 = rows
        .iter()
        .map(|row| row[1].as_f64().unwrap_or(0.0))
        .sum();
    assert!(
        (total - 225.0).abs() < 1e-6,
        "totals over the DENIED dimension must change: expected 225 from West only, got {total} ({rows:?})"
    );
}

/// A dimension denied in FULL hides every cube that uses it - the cube is unreachable rather
/// than silently empty.
#[test]
fn a_whole_dimension_denial_hides_the_cubes_that_use_it() {
    let srv = authenticated_server_with_dimension_rules(
        &[("reader", "wonderland", &["reader"])],
        &[],
        &[("reader", "Region", "#ALL", "NONE")],
    );
    let token = token_for(&srv, "reader", "wonderland");

    // Sales uses Region -> hidden, on every route.
    for path in ["/v1/Sales", "/v1/Sales/sample?limit=1"] {
        let resp = request_with_auth("GET", &format!("{}{path}", srv.base), None, Some(&token));
        assert_eq!(resp.status, 404, "{path}: {}", resp.body);
        assert_eq!(resp.code(), "unknown_dataset", "{path}");
    }
    let sql = request_with_auth(
        "POST",
        &format!("{}/v1/sql", srv.base),
        Some(r#"{"sql":"SELECT Region, value FROM Sales"}"#),
        Some(&token),
    );
    assert_eq!(sql.status, 404, "SQL must not reach the cube: {}", sql.body);

    // The dataset list does not advertise it.
    let listed = request_with_auth("GET", &format!("{}/v1/datasets", srv.base), None, Some(&token));
    let names = listed.json()["datasets"].as_array().cloned().unwrap_or_default();
    assert!(
        !names.iter().any(|v| v == &serde_json::json!("Sales")),
        "a cube behind a denied dimension must not be listed: {names:?}"
    );
}

/// A caller with no dimension rules is unaffected: default ACCEPT.
#[test]
fn dimension_access_is_opt_in() {
    let srv = authenticated_server_with_dimension_rules(
        &[("analyst", "wonderland", &["analyst"])],
        &[],
        &[("other", "Region", "East", "NONE")],
    );
    let token = token_for(&srv, "analyst", "wonderland");
    let query = r#"{"dataset":"Sales","groupBy":[{"name":"Region"}],
                    "measures":[{"name":"Sales","aggregation":"SUM"}]}"#;
    let resp = request_with_auth(
        "POST",
        &format!("{}/v1/Sales/query", srv.base),
        Some(query),
        Some(&token),
    );
    assert_eq!(resp.status, 200, "{}", resp.body);
    let rows = resp.json()["rows"].as_array().cloned().unwrap_or_default();
    assert!(
        rows.iter().any(|row| row[0] == serde_json::json!("East")),
        "another role's rule must not apply: {rows:?}"
    );
}
// ---------------------------------------------------------------------------
// Writeback (WRITE-API-DESIGN.md): write, permissions, splash, save, join check
// ---------------------------------------------------------------------------

/// The fixture plus an attribute cube and a consolidation in the Region default hierarchy,
/// so the write tests can exercise joins and splashing.
fn write_rich_fixture(path: &PathBuf) {
    write_fixture(path);

    let mut catalog = Catalog::load_from_disk(path.to_str().expect("utf8 path"))
        .expect("load fixture");
    catalog.add_cube("RegionAttr", &["Region", "Attribute"], Some("Attribute"), false);
    {
        let cube = catalog.get_cube_mut("RegionAttr").expect("RegionAttr");
        cube.write(&["East", "Owner"], CellValue::String("Alice".to_string()))
            .expect("attr East");
        cube.write(&["West", "Owner"], CellValue::String("Bob".to_string()))
            .expect("attr West");
    }
    {
        let dim = catalog.get_dimension("Region").expect("Region");
        let mut dim = dim.write().expect("lock");
        dim.add_component_in(None, "Total", "East", 1.0).expect("East under Total");
        dim.add_component_in(None, "Total", "West", 1.0).expect("West under Total");
    }
    catalog.save_to_disk(path.to_str().expect("utf8 path")).expect("save");
}

/// A server on the rich fixture, behind auth, with optional table rules.
fn rich_server(
    users: &[(&str, &str, &[&str])],
    table_rules: &[(&str, &str, &str)],
) -> Server {
    let (db_path, log_path) = unique_paths("writedb");
    write_rich_fixture(&db_path);

    if !table_rules.is_empty() {
        let mut catalog = Catalog::load_from_disk(db_path.to_str().expect("utf8 path"))
            .expect("load");
        for (role, cube, level) in table_rules {
            let level = olap_engine::security::access::Level::parse(level)
                .unwrap_or_else(|| panic!("bad level {level:?}"));
            olap_engine::security::access::set_rule(&mut catalog, role, cube, level)
                .expect("table rule");
        }
        catalog.save_to_disk(db_path.to_str().expect("utf8 path")).expect("save rules");
    }

    let security_path = log_path.with_extension("security.json");
    let mut file = olap_engine::security::store::SecurityFile::default();
    for (name, password, roles) in users {
        let roles: Vec<String> = roles.iter().map(|role| role.to_string()).collect();
        file.upsert_user(name, password, &roles).expect("user");
    }
    file.save(&security_path).expect("security file");

    spawn_server_with_security(&db_path, &log_path, true, Some(&security_path))
}

fn write_body(cells: &str, splash: Option<&str>, create_missing: bool) -> String {
    format!(
        "{{\"cells\":{cells}{}{}}}",
        splash.map(|mode| format!(",\"splash\":\"{mode}\"")).unwrap_or_default(),
        format!(",\"createMissing\":{create_missing}")
    )
}

/// A write lands, is readable through the query API, and survives an explicit save.
#[test]
fn a_write_lands_and_persists_after_an_explicit_save() {
    let srv = rich_server(&[("admin", "adminpw", &["admin"])], &[]);
    let token = token_for(&srv, "admin", "adminpw");

    let cells = r#"[{"coords":{"Region":"East","Product":"Widget","Measure":"Sales"},"value":999}]"#;
    let resp = request_with_auth(
        "POST",
        &format!("{}/v1/Sales/write", srv.base),
        Some(&write_body(cells, None, false)),
        Some(&token),
    );
    assert_eq!(resp.status, 200, "write failed: {}", resp.body);
    assert_eq!(resp.json()["written"], serde_json::json!(1));

    // Readable immediately: the write bumped the version, so no stale cache.
    let query = r#"{"dataset":"Sales","groupBy":[{"name":"Product"}],
                    "measures":[{"name":"Sales","aggregation":"SUM"}],
                    "filters":[{"field":"Region","operator":"in","values":["East"]}]}"#;
    let read = request_with_auth(
        "POST",
        &format!("{}/v1/Sales/query", srv.base),
        Some(query),
        Some(&token),
    );
    assert_eq!(read.status, 200, "{}", read.body);
    let rows = read.json()["rows"].as_array().cloned().unwrap_or_default();
    let widget = rows
        .iter()
        .find(|row| row[0] == serde_json::json!("Widget"))
        .expect("Widget row");
    assert_eq!(widget[1], serde_json::json!(999.0), "the write must be visible: {rows:?}");

    // NOT persisted yet: writes live in memory until an explicit save.
    let before_save = Catalog::load_from_disk(srv.db_path.to_str().expect("utf8 path"))
        .expect("reload")
        .get_cube("Sales")
        .expect("Sales")
        .read_cell(&["East", "Widget", "Sales"]);
    assert_eq!(
        before_save,
        Some(CellValue::Numeric(100.0)),
        "without a save, the FILE must still hold the original value"
    );

    // Save, then the file holds the write.
    let saved = request_with_auth(
        "POST",
        &format!("{}/v1/save", srv.base),
        None,
        Some(&token),
    );
    assert_eq!(saved.status, 200, "save failed: {}", saved.body);
    let after_save = Catalog::load_from_disk(srv.db_path.to_str().expect("utf8 path"))
        .expect("reload")
        .get_cube("Sales")
        .expect("Sales")
        .read_cell(&["East", "Widget", "Sales"]);
    assert_eq!(after_save, Some(CellValue::Numeric(999.0)));
}

/// A role with READ but not WRITE is refused loudly - 403, never a fake success.
#[test]
fn a_read_only_role_cannot_write() {
    let srv = rich_server(
        &[("reader", "wonderland", &["reader"])],
        &[("reader", "Sales", "READ")],
    );
    let token = token_for(&srv, "reader", "wonderland");

    // Reading still works.
    assert_eq!(
        request_with_auth("GET", &format!("{}/v1/Sales", srv.base), None, Some(&token)).status,
        200
    );

    let cells = r#"[{"coords":{"Region":"East","Product":"Widget","Measure":"Sales"},"value":1}]"#;
    let resp = request_with_auth(
        "POST",
        &format!("{}/v1/Sales/write", srv.base),
        Some(&write_body(cells, None, false)),
        Some(&token),
    );
    assert_eq!(resp.status, 403, "{}", resp.body);
    assert_eq!(resp.code(), "forbidden");
}

/// Strict member handling by default; the client opts into creation.
#[test]
fn creating_members_is_the_clients_choice() {
    let srv = rich_server(&[("admin", "adminpw", &["admin"])], &[]);
    let token = token_for(&srv, "admin", "adminpw");
    let url = format!("{}/v1/Sales/write", srv.base);
    // "Widgets" does not exist.
    let cells = r#"[{"coords":{"Region":"East","Product":"Widgets","Measure":"Sales"},"value":1}]"#;

    let strict = request_with_auth("POST", &url, Some(&write_body(cells, None, false)), Some(&token));
    assert_eq!(strict.status, 400, "{}", strict.body);
    assert_eq!(strict.code(), "unknown_member");

    let permissive =
        request_with_auth("POST", &url, Some(&write_body(cells, None, true)), Some(&token));
    assert_eq!(permissive.status, 200, "{}", permissive.body);
}

/// A consolidated coordinate needs the client to say what it means; `replace` sets the total.
#[test]
fn writing_a_consolidated_cell_needs_an_explicit_splash() {
    let srv = rich_server(&[("admin", "adminpw", &["admin"])], &[]);
    let token = token_for(&srv, "admin", "adminpw");
    let url = format!("{}/v1/Sales/write", srv.base);
    let cells = r#"[{"coords":{"Region":"Total","Product":"Widget","Measure":"Sales"},"value":200}]"#;

    let refused = request_with_auth("POST", &url, Some(&write_body(cells, None, false)), Some(&token));
    assert_eq!(refused.status, 400, "{}", refused.body);
    assert_eq!(refused.code(), "consolidated_write");

    let replaced = request_with_auth(
        "POST",
        &url,
        Some(&write_body(cells, Some("replace"), false)),
        Some(&token),
    );
    assert_eq!(replaced.status, 200, "{}", replaced.body);

    // The rollup now reads 200 (100 from each leaf).
    let query = r#"{"dataset":"Sales","groupBy":[{"name":"Product"}],
                    "measures":[{"name":"Sales","aggregation":"SUM"}],
                    "filters":[{"field":"Region","operator":"in","values":["Total"]}]}"#;
    let read = request_with_auth(
        "POST",
        &format!("{}/v1/Sales/query", srv.base),
        Some(query),
        Some(&token),
    );
    assert_eq!(read.status, 200, "{}", read.body);
    let rows = read.json()["rows"].as_array().cloned().unwrap_or_default();
    let widget = rows
        .iter()
        .find(|row| row[0] == serde_json::json!("Widget"))
        .expect("Widget row");
    assert_eq!(widget[1], serde_json::json!(200.0), "splash replace must set the total: {rows:?}");
}

/// S5: a join reaches a second cube, so the join target is access-checked too.
#[test]
fn a_join_cannot_reach_a_denied_cube() {
    let srv = rich_server(
        &[
            ("admin", "adminpw", &["admin"]),
            ("reader", "wonderland", &["reader"]),
        ],
        &[("reader", "RegionAttr", "NONE")],
    );
    let admin = token_for(&srv, "admin", "adminpw");
    let reader = token_for(&srv, "reader", "wonderland");

    let query = r#"{"dataset":"Sales","groupBy":[{"name":"Region"}],
                    "measures":[{"name":"Sales","aggregation":"SUM"}],
                    "join":{"cube":"RegionAttr","attributes":["Owner"]}}"#;
    let url = format!("{}/v1/Sales/query", srv.base);

    let allowed = request_with_auth("POST", &url, Some(query), Some(&admin));
    assert_eq!(allowed.status, 200, "admin join: {}", allowed.body);
    assert!(
        allowed.body.contains("Owner"),
        "the join must attach the attribute for an allowed role: {}",
        allowed.body
    );

    let denied = request_with_auth("POST", &url, Some(query), Some(&reader));
    assert_eq!(denied.status, 404, "a denied join target must look missing: {}", denied.body);
    assert_eq!(denied.code(), "unknown_dataset");
}

/// Saving is a server-maintenance action: an ordinary role must not be able to do it.
#[test]
fn saving_requires_the_admin_role() {
    let srv = rich_server(&[("reader", "wonderland", &["reader"])], &[]);
    let token = token_for(&srv, "reader", "wonderland");
    let resp = request_with_auth(
        "POST",
        &format!("{}/v1/save", srv.base),
        None,
        Some(&token),
    );
    assert_eq!(resp.status, 403, "{}", resp.body);
    assert_eq!(resp.code(), "forbidden");
}
/// PROTOCOL §11.5: a member created by an HTTP write must be visible in `#Members` without a
/// restart. The refresh is additive and in place, so a concurrent reader sees no torn state.
#[test]
fn a_member_created_by_a_write_appears_in_the_system_cubes() {
    let srv = rich_server(&[("admin", "adminpw", &["admin"])], &[]);
    let token = token_for(&srv, "admin", "adminpw");

    let body = r#"{"cells":[{"coords":{"Region":"East","Product":"Gizmo","Measure":"Sales"},"value":1}],
                    "createMissing":true}"#;
    let write = request_with_auth(
        "POST",
        &format!("{}/v1/Sales/write", srv.base),
        Some(body),
        Some(&token),
    );
    assert_eq!(write.status, 200, "write failed: {}", write.body);

    let query = r##"{"dataset":"#Members","groupBy":[{"name":"#Member"}],
                    "filters":[{"field":"#Dimension","operator":"in","values":["Product"]}]}"##;
    let members = request_with_auth(
        "POST",
        &format!("{}/v1/%23Members/query", srv.base),
        Some(query),
        Some(&token),
    );
    assert_eq!(members.status, 200, "{}", members.body);
    let rows = members.json()["rows"].as_array().cloned().unwrap_or_default();
    assert!(
        rows.iter().any(|row| row[0] == serde_json::json!("Gizmo")),
        "a member created by a write must appear in #Members without a restart: {rows:?}"
    );
}
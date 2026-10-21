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
        if get_json(&server.base, "/v1/capabilities").is_some() {
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

    // The fixture builds `Sales` and `Markets`; the response is sorted, so the
    // order is `Markets` then `Sales` regardless of insertion order.
    assert_eq!(resp.json()["datasets"], serde_json::json!(["Markets", "Sales"]));

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
/// gone. (The engine exposes no write endpoint, so the reader/writer race itself is
/// covered in `concurrency_tests.rs`, against the same `VersionStore`.)
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
    let resp = post_json(
        &format!("{}/v1/sql", srv.base),
        r#"{"sql":"SELECT Region, Product, Measure, value FROM Sales"}"#,
    );

    assert_eq!(resp.status, 200, "select via the console: {}", resp.body);
    let out = resp.json()["output"].as_str().expect("output is a string").to_string();
    // The fixture has East/Widget, East/Gadget, West/Widget, West/Gadget for Sales.
    assert!(out.contains("Region"), "header row missing:\n{out}");
    assert!(out.contains("East") && out.contains("West"), "rows missing:\n{out}");
    assert!(out.contains("Widget"), "measures missing:\n{out}");
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

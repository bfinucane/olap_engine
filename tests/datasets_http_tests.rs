//! Tests for dataset DISCOVERY: `GET /v1/datasets`.
//!
//! WHY THIS FILE EXISTS SEPARATELY
//!   Discovery is the FIRST call Plateau makes — it cannot guess cube names, so
//!   it asks here and then uses each returned name verbatim in `/v1/{dataset}`.
//!   Keeping these tests in their own file (rather than appending to
//!   `serve_http_tests.rs`) avoids colliding with another session that owns
//!   that file. The fixture and the tiny HTTP client are therefore duplicated on
//!   purpose: this file stands alone.
//!
//! WHAT IS AT STAKE
//!   * The names must round-trip in their EXACT stored casing. Returning a
//!     case-folded `sales` for a cube named `Sales` would make every follow-up
//!     `/v1/sales` request a 404.
//!   * The list is sorted, so the response is deterministic.
//!   * `datasets` must NOT be mistaken for a dataset literally named "datasets"
//!     by the router (it is matched before the `/v1/{dataset}` split).
//!   * Discovery is read-only and refusable: a POST is `invalid_argument`.

use olap_engine::catalog::catalog::Catalog;
use olap_engine::cube::node::CellValue;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Server lifecycle (self-contained; mirrors serve_http_tests but is independent)
// ---------------------------------------------------------------------------

/// Owns the server process, its database file and its log. Killed on drop.
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
    fn start() -> Server {
        let (db_path, log_path) = unique_paths("datasets");
        write_fixture(&db_path);
        let server = spawn_server(&db_path, &log_path);
        // The fixture always has data, so prove the server reads the catalogue
        // it was given before any test blames the route table.
        if get_json(&server.base, "/v1/capabilities").is_none() {
            let stderr = read_log(&server.log_path);
            drop(server);
            panic!("server bound but did not answer /v1/capabilities.\nserver stderr:\n{stderr}");
        }
        server
    }
}

/// Monotonic counter making artifact names unique within one test process.
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Unique artifact paths per server, so parallel tests never collide.
fn unique_paths(tag: &str) -> (PathBuf, PathBuf) {
    let dir = artifact_dir();
    let n = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stem = format!("serve_{tag}_{}_{n}", std::process::id());
    (dir.join(format!("{stem}.bin")), dir.join(format!("{stem}.log")))
}

/// Where this suite keeps its fixture database and server logs.
///
/// NOT the OS temp directory: the test binary may run under a file sandbox that
/// permits writes only under the session workspace. `OLAP_TEST_ARTIFACTS`
/// overrides the location; the PID keeps concurrent `cargo test` runs apart.
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

/// An ephemeral port, obtained by binding and immediately releasing it.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    l.local_addr().expect("local addr").port()
}

fn read_log(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Spawns `serve` and waits until it ANSWERS (`/v1/capabilities` needs no dataset).
fn spawn_server(db_path: &PathBuf, log_path: &PathBuf) -> Server {
    let exe = env!("CARGO_BIN_EXE_serve");
    let log = std::fs::File::create(log_path).expect("create server log");

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");

    let child = Command::new(exe)
        .args(["--bind", &format!("127.0.0.1:{port}"), "--db"])
        .arg(db_path)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log.try_clone().expect("clone log handle")))
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

/// Builds the fixture catalogue into `path`.
///
/// Two cubes with deliberately NON-alphabetical-in-insertion names, so that a
/// test asserting sorted output is meaningful (if the code preserved insertion
/// order, `Sales` would lead). The casing is mixed (`Sales`, `Markets`) so a
/// case-folding bug is caught too.
fn write_fixture(path: &PathBuf) {
    let mut catalog = Catalog::new();
    catalog.add_cube("Sales", &["Region", "Product", "Measure"], Some("Measure"), true);
    catalog.add_cube("Markets", &["Region"], None, true);

    {
        let cube = catalog.get_cube_mut("Sales").expect("Sales cube");
        cube.write(&["East", "Widget", "Sales"], CellValue::Numeric(100.0))
            .expect("write Sales");
    }
    {
        let cube = catalog.get_cube_mut("Markets").expect("Markets cube");
        cube.write(&["East"], CellValue::Numeric(10.0)).expect("write Markets");
    }

    catalog.save_to_disk(path.to_str().expect("utf-8 path")).expect("save fixture");
}

// ---------------------------------------------------------------------------
// A minimal HTTP/1.1 client (self-contained, no dependencies)
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

/// Sends one request over a fresh connection (`Connection: close`).
fn request(method: &str, url: &str, body: Option<&str>) -> HttpResponse {
    let addr = url.trim_start_matches("http://");
    let (authority, target) = match addr.split_once('/') {
        Some((a, t)) => (a, format!("/{t}")),
        None => (addr, "/".to_string()),
    };

    let mut stream =
        TcpStream::connect(authority).unwrap_or_else(|e| panic!("connect {authority}: {e}"));
    stream.set_read_timeout(Some(Duration::from_secs(20))).expect("read timeout");

    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
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

/// `Some(json)` when GET succeeds, `None` when the server is not answering yet.
fn get_json(base: &str, path: &str) -> Option<serde_json::Value> {
    let addr = base.trim_start_matches("http://");
    TcpStream::connect(addr).ok()?;
    let resp = request("GET", &format!("{base}{path}"), None);
    (resp.status == 200).then(|| resp.json())
}

// ---------------------------------------------------------------------------
// The unit function
// ---------------------------------------------------------------------------

/// The pure `wire::datasets` function: sorted, stored-casing names straight from
/// the catalog. This is the contract the HTTP route merely serialises.
#[test]
fn wire_datasets_returns_sorted_stored_casing_names() {
    let mut catalog = Catalog::new();
    // Insert in an order that is NOT already sorted, so sorting is proven.
    catalog.add_cube("Sales", &["Region"], None, true);
    catalog.add_cube("Markets", &["Region"], None, true);
    catalog.add_cube("Inventory", &["Region"], None, true);

    let resp = olap_engine::engine::wire::datasets(&catalog);
    assert_eq!(
        resp.datasets,
        vec![
            "Inventory".to_string(),
            "Markets".to_string(),
            "Sales".to_string(),
        ],
        "names are sorted and keep their exact stored casing"
    );
}

/// An empty catalog yields an empty list — not an error, not a null.
#[test]
fn wire_datasets_on_empty_catalog_is_an_empty_list() {
    let catalog = Catalog::new();
    let resp = olap_engine::engine::wire::datasets(&catalog);
    assert!(resp.datasets.is_empty(), "no cubes means no datasets, not a failure");
}

// ---------------------------------------------------------------------------
// The HTTP route
// ---------------------------------------------------------------------------

/// `GET /v1/datasets` answers 200 with a JSON `datasets` array in sorted,
/// stored-casing order, and every advertised name actually resolves.
#[test]
fn get_datasets_lists_the_catalog_names_in_stored_casing_sorted() {
    let srv = Server::start();
    let resp = get(&format!("{}/v1/datasets", srv.base));
    assert_eq!(resp.status, 200, "body: {}", resp.body);
    assert_eq!(resp.header("Content-Type"), Some("application/json"));

    // Fixture inserts `Sales` then `Markets`; the response must be sorted.
    //
    // The `#`-prefixed system cubes are listed too, deliberately: a client discovers the
    // database by listing datasets, and `isSystem` in `describe` is what tells a cube
    // picker to hide them. `#` sorts before letters, so they lead.
    assert_eq!(
        resp.json()["datasets"],
        serde_json::json!(["#Cubes", "#Dimensions", "#Hierarchies", "#Members", "Markets", "Sales"])
    );

    // Every advertised name must be describable via the verbatim name — the
    // round trip Plateau depends on. (`#` must be percent-encoded in the URL.)
    for (name, url) in [("Markets", "Markets"), ("Sales", "Sales"), ("#Cubes", "%23Cubes")] {
        let d = get(&format!("{}/v1/{}", srv.base, url));
        assert_eq!(d.status, 200, "advertised dataset '{name}' must be describable");
        assert_eq!(d.json()["dataset"], serde_json::json!(name));
    }
}

/// The router must match `/v1/datasets` BEFORE the `/v1/{dataset}` split; if it
/// did not, the request would be treated as `describe` for a dataset literally
/// named "datasets" and return `unknown_dataset` (404) instead of the list.
#[test]
fn datasets_route_is_not_shadowed_by_the_dataset_route() {
    let srv = Server::start();
    let resp = get(&format!("{}/v1/datasets", srv.base));
    assert_eq!(resp.status, 200, "must be the list, not a describe of 'datasets': {}", resp.body);
    assert!(
        resp.json()["datasets"].is_array(),
        "the answer must be a datasets array, got: {}",
        resp.body
    );
}

/// Discovery is GET-only. A POST is the client's fault and must be refused with
/// the protocol's `invalid_argument`, not silently answered.
#[test]
fn post_to_datasets_is_rejected() {
    let srv = Server::start();
    let resp = request("POST", &format!("{}/v1/datasets", srv.base), Some("{}"));
    assert_eq!(resp.status, 400, "body: {}", resp.body);
    assert_eq!(resp.code(), "invalid_argument");
}

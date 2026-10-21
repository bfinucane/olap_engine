//! `serve` — the HTTP/JSON transport for ENGINE-PROTOCOL.md v1.
//! 
//!
//! WHY A HAND-ROLLED SERVER
//!   The engine has no async runtime and no web-crate dependency, and adding one
//!   is a large, slow step. The protocol is tiny (four routes, no streaming, no
//!   keep-alive requirements), so a blocking `std::net::TcpListener` with a
//!   thread per connection is enough to get Plateau talking to the engine
//!   today. The ONLY normative thing is the JSON (the protocol explicitly leaves
//!   the transport unspecified), so this file is deliberately swappable.
//!
//! ROUTES (ENGINE-PROTOCOL.md "Transport")
//!   GET  /v1/capabilities            -> capabilities
//!   GET  /v1/datasets                -> the list of dataset names
//!   GET  /v1/{dataset}               -> describe
//!   GET  /v1/{dataset}/sample?limit=N&offset=M[&columns=a,b]  -> sample
//!   POST /v1/{dataset}/query         -> query   (body is a QueryRequest)
//!
//! USAGE
//!   cargo run --bin serve -- [--bind 127.0.0.1:8080] [--db database.bin] [--cors]
//!
//! CORS is off by default. Turn it on (`--cors`, or `OLAP_CORS=1`) while the
//! Plateau dev server runs on a different origin.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;

use olap_engine::catalog::catalog::Catalog;
use olap_engine::engine::plan::QueryRequest;
use olap_engine::engine::wire::{
    self, capabilities, datasets, describe, query, sample, ErrorResponse, SampleQuery, WireError,
};

/// Body of `POST /v1/sql`.
#[derive(serde::Deserialize)]
struct SqlRequest {
    sql: String,
}

/// The read-only SQL console page, served at `GET /sql`.
///
/// Deliberately a single self-contained string: no assets, no build step, no
/// framework. It talks to `POST /v1/sql`, which accepts exactly one SELECT.
const SQL_CONSOLE_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>OLAP SQL console</title>
<style>
  body { font: 15px/1.5 -apple-system, Segoe UI, Roboto, sans-serif; margin: 2rem auto; max-width: 60rem; padding: 0 1rem; color: #1a1a1a; }
  h1 { font-size: 1.25rem; margin: 0 0 .25rem; }
  .note { color: #555; margin: 0 0 1rem; }
  textarea { width: 100%; height: 8rem; font: 13px/1.45 ui-monospace, Consolas, monospace; padding: .6rem; border: 1px solid #ccc; border-radius: 6px; box-sizing: border-box; }
  button { font: inherit; padding: .45rem 1rem; border: 1px solid #1a1a1a; background: #1a1a1a; color: #fff; border-radius: 6px; cursor: pointer; }
  button:hover { background: #333; }
  #status { color: #555; margin-left: .75rem; }
  pre { background: #f6f6f6; border: 1px solid #e2e2e2; border-radius: 6px; padding: .75rem; overflow: auto; min-height: 3rem; white-space: pre; }
  code { background: #f0f0f0; padding: .05rem .25rem; border-radius: 3px; }
</style>
</head>
<body>
<h1>OLAP SQL console</h1>
<p class="note">
  Read-only: only <code>SELECT</code> is accepted. Omitted dimensions default to their
  first member, so name every dimension you want included in a total.
  Ctrl+Enter runs the query.
</p>
<textarea id="sql" spellcheck="false">SELECT Region, Segment, Category, value FROM Sales</textarea>
<div style="margin-top:.6rem">
  <button id="run">Run</button><span id="status"></span>
</div>
<pre id="out"></pre>
<script>
const out = document.getElementById('out');
const status = document.getElementById('status');
async function run() {
  status.textContent = 'running\u2026';
  try {
    const r = await fetch('/v1/sql', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ sql: document.getElementById('sql').value })
    });
    const j = await r.json();
    out.textContent = ('output' in j) ? j.output : (j.error || JSON.stringify(j, null, 2));
    status.textContent = r.ok ? (j.elapsedMs + ' ms') : ('error ' + r.status + ' (' + j.code + ')');
  } catch (e) {
    status.textContent = 'request failed';
    out.textContent = String(e);
  }
}
document.getElementById('run').onclick = run;
document.getElementById('sql').addEventListener('keydown', e => {
  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); run(); }
});
</script>
</body>
</html>
"#;

fn main() {
    let (bind, db_path, cors) = parse_args();

    let catalog = load_catalog(&db_path);
    // No catalog-wide lock (Phase 1b M3). Reads take an O(1) snapshot of a cube's
    // `VersionStore` and never need the write path; the engine exposes no write
    // endpoint over HTTP, so the only writer is the REPL/import that owns the
    // catalog outright. A `Mutex<Catalog>` here would serialise every request
    // behind one lock for no correctness benefit.
    let shared = Arc::new(catalog);

    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Could not bind {}: {}", bind, e);
            std::process::exit(1);
        }
    };

    eprintln!(
        "olap_engine protocol v{} listening on http://{}",
        wire::protocol_version(),
        bind
    );
    if cors {
        eprintln!("CORS enabled (Access-Control-Allow-Origin: *).");
    }
    eprintln!("Load 'http://{bind}/v1' as the engine base URL in Plateau.");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, shared, cors) {
                        // A client that hangs up mid-response is normal; do not
                        // spam the log for it.
                        if e.kind() != std::io::ErrorKind::BrokenPipe {
                            eprintln!("connection error: {}", e);
                        }
                    }
                });
            }
            Err(e) => eprintln!("accept error: {}", e),
        }
    }
}

/// Parses `--bind`, `--db`, `--cors` / `OLAP_CORS`.
fn parse_args() -> (String, String, bool) {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut bind = "127.0.0.1:8080".to_string();
    let mut db = "database.bin".to_string();
    let mut cors =
        std::env::var("OLAP_CORS").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--bind" if i + 1 < argv.len() => {
                bind = argv[i + 1].clone();
                i += 1;
            }
            "--db" if i + 1 < argv.len() => {
                db = argv[i + 1].clone();
                i += 1;
            }
            "--cors" => cors = true,
            _ => {}
        }
        i += 1;
    }
    (bind, db, cors)
}

/// Loads `db_path` if it exists, else starts empty (matching the REPL's
/// behaviour). A load failure is reported but does not stop the server.
fn load_catalog(db_path: &str) -> Catalog {
    if Path::new(db_path).exists() {
        match Catalog::load_from_disk(db_path) {
            Ok(c) => {
                eprintln!("Loaded catalog from '{}'.", db_path);
                return c;
            }
            Err(e) => {
                eprintln!("Could not load '{}': {} — starting empty.", db_path, e);
            }
        }
    } else {
        eprintln!("No '{}' found; starting with an empty catalog.", db_path);
    }
    Catalog::new()
}

/// A minimal parsed HTTP request.
struct Request {
    method: String,
    /// The path, WITHOUT the query string.
    path: String,
    /// Raw query string (everything after `?`), if any.
    query: String,
    body: Vec<u8>,
}

impl Request {
    /// The `k=v` params, split on `&`. Values here are simple names, so
    /// percent-decoding is skipped (a later step).
    fn params(&self) -> Vec<(String, String)> {
        self.query
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (pair.to_string(), String::new()),
            })
            .collect()
    }
}

/// Reads one request, dispatches it, and writes one response. Connection close
/// after the response keeps the parser trivial (no keep-alive state to maintain).
fn handle_connection(
    mut stream: TcpStream,
    shared: Arc<Catalog>,
    cors: bool,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    // --- Request line ---
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(()); // client opened and closed without sending anything
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    // --- Headers (we only need Content-Length) ---
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    // --- Body ---
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    let request = Request { method, path, query, body };

    // --- CORS preflight ---
    if request.method == "OPTIONS" {
        write_response(&mut stream, 204, "text/plain", "", cors)?;
        return Ok(());
    }

    // --- The SQL console page (the only non-JSON response) ---
    if request.method == "GET" && request.path.trim_end_matches('/') == "/sql" {
        write_response(&mut stream, 200, "text/html; charset=utf-8", SQL_CONSOLE_HTML, cors)?;
        return Ok(());
    }

    match route(&request, &shared) {
        Ok((status, value)) => {
            let text = value.to_string();
            write_json(&mut stream, status, &text, cors)?;
        }
        Err(e) => {
            let env = ErrorResponse {
                error: e.message.clone(),
                code: e.code.to_string(),
            };
            let text = serde_json::to_string(&env).unwrap_or_else(|_| {
                format!("{{\"error\":{:?},\"code\":{:?}}}", e.message, e.code)
            });
            write_json(&mut stream, e.status(), &text, cors)?;
        }
    }
    Ok(())
}

/// Dispatches a parsed request to the wire operations. Returns a JSON value on
/// success or a typed `WireError` on failure.
fn route(
    req: &Request,
    shared: &Arc<Catalog>,
) -> Result<(u16, serde_json::Value), WireError> {
    // Strip a trailing slash for uniform matching.
    let path = req.path.trim_end_matches('/');

    // GET /v1/capabilities
    if path == "/v1/capabilities" {
        if req.method != "GET" {
            return Err(method_not_allowed("GET"));
        }
        return Ok((200, serde_json::to_value(capabilities()).unwrap()));
    }

    // GET /v1/datasets — the list of queryable dataset names. Checked BEFORE the
    // /v1/{dataset} split so "datasets" is never read as a dataset named
    // "datasets".
    if path == "/v1/datasets" {
        if req.method != "GET" {
            return Err(method_not_allowed("GET"));
        }
        return Ok((200, serde_json::to_value(datasets(shared)).unwrap()));
    }

    // POST /v1/sql — the read-only SQL console's endpoint.
    //
    // Deliberately restricted to a single SELECT (see `sql::ensure_read_only`): the
    // port has no authentication, so a write path here would let anything on the
    // machine rewrite the catalogue. The CLI keeps that power.
    if path == "/v1/sql" {
        if req.method != "POST" {
            return Err(method_not_allowed("POST"));
        }
        let body: SqlRequest = serde_json::from_slice(&req.body).map_err(|e| WireError {
            code: "invalid_argument",
            message: format!("Malformed body; expected {{\"sql\": \"SELECT ...\"}}: {}", e),
        })?;

        olap_engine::engine::sql::ensure_read_only(&body.sql).map_err(|message| WireError {
            code: "invalid_argument",
            message,
        })?;

        let started = std::time::Instant::now();
        let mut catalog = (**shared).clone();
        let output = olap_engine::engine::sql::execute_sql(&mut catalog, &body.sql)
            .map_err(|message| WireError {
                code: "invalid_argument",
                message,
            })?;

        return Ok((
            200,
            serde_json::json!({
                "sql": body.sql,
                "output": output,
                "elapsedMs": started.elapsed().as_millis() as u64,
            }),
        ));
    }

    // Everything else is under /v1/{dataset}[/sample|/query].
    let rest = match path.strip_prefix("/v1/") {
        Some(r) => r,
        None => {
            return Err(WireError {
                code: "invalid_argument",
                message: format!("Unknown path '{}'.", req.path),
            });
        }
    };

    // Split "{dataset}" or "{dataset}/{operation}".
    let (dataset, operation) = match rest.split_once('/') {
        Some((d, op)) => (d, op),
        None => (rest, ""),
    };

    match (req.method.as_str(), operation) {
        // GET /v1/{dataset} -> describe
        ("GET", "") => {
            let resp = describe(shared, dataset)?;
            Ok((200, serde_json::to_value(resp).unwrap()))
        }
        // GET /v1/{dataset}/sample
        ("GET", "sample") => {
            let mut params = SampleQuery::default();
            let mut columns: Vec<String> = Vec::new();
            for (k, v) in req.params() {
                match k.as_str() {
                    "limit" => params.limit = v.parse().ok(),
                    "offset" => params.offset = v.parse().ok(),
                    "columns" => columns = v.split(',').map(|s| s.to_string()).collect(),
                    _ => {}
                }
            }
            params.columns = columns;
            let resp = sample(shared, dataset, &params)?;
            Ok((200, serde_json::to_value(resp).unwrap()))
        }
        // POST /v1/{dataset}/query
        ("POST", "query") => {
            let plan: QueryRequest = serde_json::from_slice(&req.body).map_err(|e| WireError {
                code: "invalid_argument",
                message: format!("Malformed query body: {}", e),
            })?;
            if plan.dataset.is_empty() {
                return Err(WireError {
                    code: "invalid_argument",
                    message: "Query body must include a 'dataset'.".to_string(),
                });
            }
            if plan.dataset != dataset {
                return Err(WireError {
                    code: "invalid_argument",
                    message: format!(
                        "Path dataset '{}' does not match body dataset '{}'.",
                        dataset, plan.dataset
                    ),
                });
            }
            let resp = query(shared, &plan)?;
            Ok((200, serde_json::to_value(resp).unwrap()))
        }
        ("GET", _) | ("POST", _) => Err(WireError {
            code: "invalid_argument",
            message: format!("Unknown operation '{}' for dataset '{}'.", operation, dataset),
        }),
        (other, _) => Err(WireError {
            code: "invalid_argument",
            message: format!("Method {} not allowed here.", other),
        }),
    }
}

fn method_not_allowed(allowed: &str) -> WireError {
    WireError {
        code: "invalid_argument",
        message: format!("Method not allowed; use {}.", allowed),
    }
}

/// Writes a JSON response with the given status. `cors` adds the headers the
/// Plateau dev server needs while running on another origin.
fn write_json(stream: &mut TcpStream, status: u16, body: &str, cors: bool) -> std::io::Result<()> {
    write_response(stream, status, "application/json", body, cors)
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
    cors: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        status,
        reason,
        content_type,
        body.len()
    );
    if cors {
        head.push_str("Access-Control-Allow-Origin: *\r\n");
        head.push_str("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n");
        head.push_str("Access-Control-Allow-Headers: Content-Type\r\n");
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes())?;
    if status != 204 {
        stream.write_all(body.as_bytes())?;
    }
    stream.flush()
}

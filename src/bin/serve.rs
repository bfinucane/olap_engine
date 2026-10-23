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
//!   POST /v1/login                   -> session token (PUBLIC: it is how you get one)
//!
//! AUTHENTICATION (SECURITY-DESIGN.md §5)
//!   Every /v1 route except /v1/login requires `Authorization: Bearer <token>`.
//!   Required by default; `--allow-anonymous` turns it off for development.
//!   Users live in `security.json` beside the database, created with
//!   `serve.exe --add-user <name> --role <role>`.
//!
//! USAGE
//!   cargo run --bin serve -- [--bind 127.0.0.1:8080] [--db database.bin] [--cors]
//!                             [--security security.json] [--allow-anonymous]
//!   cargo run --bin serve -- --add-user alice --role reader --password secret
//!   cargo run --bin serve -- --list-users
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
use olap_engine::security::session::{Session, SessionStore};
use olap_engine::security::store::{default_path as default_security_path, SecurityFile};

/// Everything the request path needs to authenticate a caller.
///
/// The security file is loaded once at startup. Creating a user requires a restart, which
/// is deliberate for S1: a permission change should be an explicit act, not something that
/// silently takes effect while requests are in flight.
struct SecurityState {
    file: SecurityFile,
    sessions: SessionStore,
    /// When false the server is in development mode and every request is anonymous.
    require_auth: bool,
}

impl SecurityState {
    fn new(file: SecurityFile, require_auth: bool) -> Self {
        Self {
            file,
            sessions: SessionStore::new(12 * 60 * 60), // 12 hours
            require_auth,
        }
    }

    /// Resolves an `Authorization` header to a live session.
    ///
    /// The scheme is matched case-insensitively because HTTP auth schemes are tokens and
    /// clients differ on the casing.
    fn session_for(&self, header: &str) -> Option<Session> {
        let (scheme, token) = header.trim().split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        self.sessions.validate(token.trim())
    }
}

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
    let config = parse_args();

    // Admin utilities act on the security file and exit; they never start a server.
    if config.list_users || config.add_user.is_some() {
        std::process::exit(run_admin_command(&config));
    }

    let security_path = config
        .security
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| default_security_path(Path::new(&config.db)));
    let security_file = match SecurityFile::load(&security_path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("Could not load the security file: {e}");
            std::process::exit(2);
        }
    };
    if security_file.is_empty() && !config.allow_anonymous {
        eprintln!("No users are defined in '{}'.", security_path.display());
        eprintln!("Authentication is REQUIRED by default, so nobody could call this server.");
        eprintln!();
        eprintln!("Create the first user, then start it again:");
        eprintln!(
            "  serve.exe --add-user admin --role admin --security {}",
            security_path.display()
        );
        eprintln!("...or start in development mode with --allow-anonymous.");
        std::process::exit(2);
    }
    eprintln!(
        "Authentication: {} ({} user(s) in '{}').",
        if config.allow_anonymous {
            "DISABLED (--allow-anonymous)"
        } else {
            "required (bearer token)"
        },
        security_file.users.len(),
        security_path.display()
    );
    let security = Arc::new(SecurityState::new(security_file, !config.allow_anonymous));

    let catalog = load_catalog(&config.db);
    // No catalog-wide lock (Phase 1b M3). Reads take an O(1) snapshot of a cube's
    // `VersionStore` and never need the write path; the engine exposes no write
    // endpoint over HTTP, so the only writer is the REPL/import that owns the
    // catalog outright. A `Mutex<Catalog>` here would serialise every request
    // behind one lock for no correctness benefit.
    let shared = Arc::new(catalog);

    let listener = match TcpListener::bind(&config.bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Could not bind {}: {}", config.bind, e);
            std::process::exit(1);
        }
    };

    eprintln!(
        "olap_engine protocol v{} listening on http://{}",
        wire::protocol_version(),
        config.bind
    );
    if config.cors {
        eprintln!("CORS enabled (Access-Control-Allow-Origin: *).");
    }
    eprintln!("Load 'http://{}/v1' as the engine base URL in Plateau.", config.bind);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = Arc::clone(&shared);
                let security = Arc::clone(&security);
                let cors = config.cors;
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, shared, cors, security) {
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

/// Command-line configuration.
struct Config {
    bind: String,
    db: String,
    cors: bool,
    /// Development mode: no token required. Off unless asked for.
    allow_anonymous: bool,
    security: Option<String>,
    add_user: Option<String>,
    roles: Vec<String>,
    password: Option<String>,
    list_users: bool,
}

/// Parses `--bind`, `--db`, `--cors` / `OLAP_CORS`, `--security`, `--allow-anonymous`,
/// `--add-user`, `--role`, `--password`, `--list-users`.
fn parse_args() -> Config {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut config = Config {
        bind: "127.0.0.1:8080".to_string(),
        db: "database.bin".to_string(),
        cors: std::env::var("OLAP_CORS").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")),
        allow_anonymous: false,
        security: None,
        add_user: None,
        roles: Vec::new(),
        password: None,
        list_users: false,
    };

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--bind" if i + 1 < argv.len() => {
                config.bind = argv[i + 1].clone();
                i += 1;
            }
            "--db" if i + 1 < argv.len() => {
                config.db = argv[i + 1].clone();
                i += 1;
            }
            "--security" if i + 1 < argv.len() => {
                config.security = Some(argv[i + 1].clone());
                i += 1;
            }
            "--add-user" if i + 1 < argv.len() => {
                config.add_user = Some(argv[i + 1].clone());
                i += 1;
            }
            "--role" if i + 1 < argv.len() => {
                config.roles.push(argv[i + 1].clone());
                i += 1;
            }
            "--password" if i + 1 < argv.len() => {
                config.password = Some(argv[i + 1].clone());
                i += 1;
            }
            "--cors" => config.cors = true,
            "--allow-anonymous" => config.allow_anonymous = true,
            "--list-users" => config.list_users = true,
            _ => {}
        }
        i += 1;
    }
    config
}

/// `--add-user` / `--list-users`. Returns the process exit code.
fn run_admin_command(config: &Config) -> i32 {
    let path = config
        .security
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| default_security_path(Path::new(&config.db)));
    let mut file = match SecurityFile::load(&path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    if config.list_users {
        if file.is_empty() {
            println!("No users in '{}'.", path.display());
        } else {
            println!("Users in '{}':", path.display());
            let mut names: Vec<String> = file.users.iter().map(|u| u.name.clone()).collect();
            names.sort();
            for name in names {
                if let Some(user) = file.user(&name) {
                    let roles = if user.roles.is_empty() {
                        "(no roles)".to_string()
                    } else {
                        user.roles.join(", ")
                    };
                    println!("  {:<20} {}", name, roles);
                }
            }
        }
        if !file.roles.is_empty() {
            println!("Roles: {}", file.roles.join(", "));
        }
        return 0;
    }

    let Some(name) = &config.add_user else {
        return 0;
    };

    // The password is read from --password when given (scriptable), else from stdin
    // (one line). Neither is echoed securely: a shell history or a piped stdout can
    // capture it, so this is documented as a development-grade bootstrap.
    let password = match &config.password {
        Some(password) => password.clone(),
        None => {
            eprint!("Password for '{name}': ");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                eprintln!("Could not read a password from stdin.");
                return 1;
            }
            line.trim_end_matches(['\r', '\n']).to_string()
        }
    };

    match file.upsert_user(name, &password, &config.roles) {
        Ok(()) => match file.save(&path) {
            Ok(()) => {
                println!(
                    "User '{name}' saved to '{}' with role(s): {}",
                    path.display(),
                    if config.roles.is_empty() {
                        "(none)".to_string()
                    } else {
                        config.roles.join(", ")
                    }
                );
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        Err(e) => {
            eprintln!("Could not create the user: {e}");
            1
        }
    }
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
    /// The raw `Authorization` header value (empty when absent).
    authorization: String,
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
    security: Arc<SecurityState>,
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
    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    // Percent-decode the path. A client addressing a `#`-prefixed system cube MUST send
    // `%23` (a raw `#` starts a fragment and never reaches a server), so without this the
    // cube would be unreachable over HTTP while still working in SQL.
    let path = percent_decode(&raw_path);

    // --- Headers (Content-Length and Authorization) ---
    let mut content_length = 0usize;
    let mut authorization = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = value.trim().to_string();
            }
        }
    }

    // --- Body ---
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    let request = Request { method, path, query, body, authorization };

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

    // --- Login: the one route that cannot require a token, because it issues them ---
    if request.path.trim_end_matches('/') == "/v1/login" {
        let (status, value) = login_response(&request, &security);
        write_json(&mut stream, status, &value.to_string(), cors)?;
        return Ok(());
    }

    // --- Authentication ---
    //
    // Checked BEFORE routing, so an unauthenticated caller learns nothing about which
    // datasets exist: every route (including an unknown one) answers 401 identically.
    //
    // EXCEPT `GET /v1/capabilities`: it is PUBLIC because it is how a client discovers
    // that a token is needed at all. Requiring auth for it would make `authRequired`
    // unreadable exactly when it matters, and it leaks only feature flags, never data.
    let public_route =
        request.method == "GET" && request.path.trim_end_matches('/') == "/v1/capabilities";
    let session = if public_route {
        // A token is optional here, but a presented one must still be valid.
        if request.authorization.is_empty() {
            None
        } else {
            match security.session_for(&request.authorization) {
                Some(session) => Some(session),
                None => {
                    write_json(&mut stream, 401, &unauthorized_body(), cors)?;
                    return Ok(());
                }
            }
        }
    } else if security.require_auth {
        match security.session_for(&request.authorization) {
            Some(session) => Some(session),
            None => {
                write_json(&mut stream, 401, &unauthorized_body(), cors)?;
                return Ok(());
            }
        }
    } else {
        // Development mode: a token is optional, but if one is presented it must be
        // valid - a stale token failing silently would hide a client bug.
        if request.authorization.is_empty() {
            None
        } else {
            match security.session_for(&request.authorization) {
                Some(session) => Some(session),
                None => {
                    write_json(&mut stream, 401, &unauthorized_body(), cors)?;
                    return Ok(());
                }
            }
        }
    };

    match route(&request, &shared, &security, session.as_ref()) {
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
    security: &SecurityState,
    session: Option<&Session>,
) -> Result<(u16, serde_json::Value), WireError> {
    // The session is not yet used for authorisation (table and dimension permissions land
    // in S2/S3), but it is resolved and threaded here so enforcement has one place to go.
    let _ = session;
    // Strip a trailing slash for uniform matching.
    let path = req.path.trim_end_matches('/');

    // GET /v1/capabilities
    if path == "/v1/capabilities" {
        if req.method != "GET" {
            return Err(method_not_allowed("GET"));
        }
        let mut caps = capabilities();
        // The binary knows how it was started; the library cannot.
        caps.auth_required = security.require_auth;
        return Ok((200, serde_json::to_value(caps).unwrap()));
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
            // A browser hitting this URL is almost certainly looking for the console
            // page, which lives at /sql. "Method not allowed; use POST." is true and
            // useless; say what to do instead.
            return Err(WireError {
                code: "invalid_argument",
                message: "This is the SQL JSON endpoint: POST {\"sql\": \"SELECT ...\"}. \
                          For the browser console, open /sql instead."
                    .to_string(),
            });
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

    // POST (or anything but GET) to the page's own URL: same confusion, other way.
    if path == "/sql" {
        return Err(WireError {
            code: "invalid_argument",
            message: "The SQL console PAGE is GET /sql. To run a query programmatically, \
                      POST {\"sql\": \"SELECT ...\"} to /v1/sql."
                .to_string(),
        });
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
        // Authorization MUST be allowed: a browser sends a preflight before an authenticated
        // request, and a header absent from this list makes the request never leave.
        head.push_str("Access-Control-Allow-Headers: Content-Type, Authorization\r\n");
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes())?;
    if status != 204 {
        stream.write_all(body.as_bytes())?;
    }
    stream.flush()
}

/// Percent-decodes a URL path (`%23` -> `#`, `%20` -> space).
///
/// Needed for `#`-prefixed system cubes: a raw `#` begins a URL fragment, so a client
/// must send `/v1/%23Cubes`. Decoding the whole path is correct here because the router
/// compares literal segments; there is no user-supplied path template.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
                && let Ok(byte) = u8::from_str_radix(hex, 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
// ---------------------------------------------------------------------------
// Authentication endpoints and helpers
// ---------------------------------------------------------------------------

/// Body of `POST /v1/login`.
#[derive(serde::Deserialize)]
struct LoginRequest {
    user: String,
    password: String,
}

/// Successful `POST /v1/login` response.
#[derive(serde::Serialize)]
struct LoginResponse {
    token: String,
    user: String,
    roles: Vec<String>,
    #[serde(rename = "expiresAt")]
    expires_at: u64,
}

/// The single unauthorised response, so every failure looks the same.
fn unauthorized_body() -> String {
    // Deliberately one message for "no token", "unknown token" and "expired token":
    // a caller learns only that they must log in, never why the token was rejected.
    let env = ErrorResponse {
        error: "Authentication required. POST /v1/login to obtain a bearer token.".to_string(),
        code: "unauthorized".to_string(),
    };
    serde_json::to_string(&env).unwrap_or_else(|_| {
        "{\"error\":\"Authentication required.\",\"code\":\"unauthorized\"}".to_string()
    })
}

/// Handles `POST /v1/login`.
fn login_response(req: &Request, security: &SecurityState) -> (u16, serde_json::Value) {
    if req.method != "POST" {
        let env = ErrorResponse {
            error: "POST a JSON body {\"user\": ..., \"password\": ...} to log in.".to_string(),
            code: "method_not_allowed".to_string(),
        };
        return (405, serde_json::to_value(env).unwrap());
    }

    let parsed: Result<LoginRequest, _> = serde_json::from_slice(&req.body);
    let Ok(LoginRequest { user, password }) = parsed else {
        let env = ErrorResponse {
            error: "Login body must be JSON with 'user' and 'password'.".to_string(),
            code: "invalid_argument".to_string(),
        };
        return (400, serde_json::to_value(env).unwrap());
    };

    match security.file.authenticate(&user, &password) {
        Some(roles) => {
            let (token, session) = security.sessions.issue(&user, roles);
            let body = LoginResponse {
                token,
                user: session.user,
                roles: session.roles,
                expires_at: session.expires_at,
            };
            (200, serde_json::to_value(body).unwrap())
        }
        None => {
            // Same message for an unknown user and a wrong password: the response must
            // not tell an attacker which half was right.
            let env = ErrorResponse {
                error: "Invalid user name or password.".to_string(),
                code: "unauthorized".to_string(),
            };
            (401, serde_json::to_value(env).unwrap())
        }
    }
}
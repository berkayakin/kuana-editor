//! Oracle Database through Oracle's pure-Rust driver (`oracledb`, thin mode:
//! no Oracle Client libraries). The driver is blocking, so each live
//! connection is a thread of its own that owns it and runs jobs in order
//! (`Session::run`); the UI awaits their results.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use futures::channel::oneshot;

use super::{ColumnInfo, ConnectionConfig, DbObject, ObjectKind, Role, Subprogram};

type Job = Box<dyn FnOnce(&mut oracledb::Connection) + Send>;

/// A live connection: its thread runs one job at a time.
pub struct Session {
    jobs: mpsc::Sender<Job>,
    /// The connected user (the default schema), uppercase.
    pub user: String,
    pub version: String,
}

/// How many rows a query result keeps.
pub const MAX_ROWS: usize = 1000;
/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// The connect string for a configuration: its own, else `host:port/service`
/// (EZConnect), or a descriptor with a SID.
pub fn connect_string(config: &ConnectionConfig) -> String {
    if !config.connect_string.trim().is_empty() {
        return config.connect_string.trim().to_string();
    }
    let port = if config.port == 0 { 1521 } else { config.port };
    if config.use_sid {
        format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST={})(PORT={port}))(CONNECT_DATA=(SID={})))",
            config.host.trim(),
            config.service.trim()
        )
    } else {
        format!("{}:{port}/{}", config.host.trim(), config.service.trim())
    }
}

impl Session {
    /// Connects on a new thread; resolves once connected (or not).
    pub fn connect(config: &ConnectionConfig, password: String) -> oneshot::Receiver<Result<Session, String>> {
        let (done, result) = oneshot::channel();
        let connect = connect_string(config);
        let user = config.user.trim().to_string();
        let role = config.role;
        let name = format!("db-{}", config.name);
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            let mut options = oracledb::Config::default()
                .set_credentials(&user, &password)
                .set_driver_name("Kuana")
                .set_transport_connect_timeout(Some(CONNECT_TIMEOUT));
            // The program name Oracle shows in V$SESSION.
            options = options.clone().set_program("Kuana").unwrap_or(options);
            options = match role {
                Role::Default => options,
                Role::Sysdba => options.set_auth_mode(oracledb::AUTH_MODE_SYSDBA),
                Role::Sysoper => options.set_auth_mode(oracledb::AUTH_MODE_SYSOPER),
            };
            let options = match options.set_connect_string(&connect) {
                Ok(options) => options,
                Err(err) => {
                    let _ = done.send(Err(message(&err)));
                    return;
                }
            };
            let mut conn = match oracledb::connect(options) {
                Ok(conn) => conn,
                Err(err) => {
                    let _ = done.send(Err(message(&err)));
                    return;
                }
            };
            let user = conn
                .query_row("select sys_context('USERENV', 'CURRENT_SCHEMA') from dual", &[])
                .and_then(|row| row.get::<String>(0))
                .unwrap_or_else(|_| user.to_uppercase());
            let version = conn.version().map(|v| v.to_string()).unwrap_or_default();
            prepare(&conn);
            let (jobs, queue) = mpsc::channel::<Job>();
            if done.send(Ok(Session { jobs, user, version })).is_err() {
                return;
            }
            // Until every handle is dropped (disconnect, quit).
            for job in queue {
                job(&mut conn);
            }
            let _ = conn.close();
        });
        if let Err(err) = spawned {
            let (done, result) = oneshot::channel();
            let _ = done.send(Err(err.to_string()));
            return result;
        }
        result
    }

    /// Runs `job` with the connection, after the jobs before it.
    pub fn run<T, F>(&self, job: F) -> impl Future<Output = Result<T, String>> + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce(&mut oracledb::Connection) -> Result<T, String> + Send + 'static,
    {
        let (done, result) = oneshot::channel();
        let sent = self.jobs.send(Box::new(move |conn| {
            let _ = done.send(job(conn));
        }));
        async move {
            if sent.is_err() {
                return Err("The connection was closed".to_string());
            }
            result.await.unwrap_or_else(|_| Err("The connection was closed".to_string()))
        }
    }
}

/// Session setup: DBMS_OUTPUT on, DDL from DBMS_METADATA without storage
/// clauses and with its terminators.
fn prepare(conn: &oracledb::Connection) {
    let _ = conn.execute("begin dbms_output.enable(null); end;", &[]);
    let _ = conn.execute(
        "begin
           dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'SQLTERMINATOR', true);
           dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'PRETTY', true);
           dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'SEGMENT_ATTRIBUTES', false);
           dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'STORAGE', false);
         end;",
        &[],
    );
}

/// An error as a person reads it (Oracle's own "ORA-…" line when there is one).
pub fn message(err: &oracledb::Error) -> String {
    match err.kind() {
        oracledb::ErrorKind::DbError(db) => db.message().trim().to_string(),
        _ => err.to_string(),
    }
}

/// An Oracle error's number (ORA-nnnnn).
fn code(err: &oracledb::Error) -> Option<usize> {
    match err.kind() {
        oracledb::ErrorKind::DbError(db) => Some(db.code()),
        _ => None,
    }
}

/// An Oracle error's character offset into the statement, when it has one.
fn error_offset(err: &oracledb::Error) -> Option<usize> {
    match err.kind() {
        oracledb::ErrorKind::DbError(db) if db.offset() > 0 => Some(db.offset()),
        _ => None,
    }
}

// ---- The catalog ------------------------------------------------------------------

/// The schemas (users) the connection can see.
pub fn schemas(conn: &mut oracledb::Connection) -> Result<Vec<String>, String> {
    let rows = conn.query("select username from all_users order by username", &[]).map_err(|e| message(&e))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| message(&e))?.get::<String>(0).map_err(|e| message(&e))?);
    }
    Ok(out)
}

/// The objects in `owner` that the Database view lists.
pub fn objects(conn: &mut oracledb::Connection, owner: &str) -> Result<Vec<DbObject>, String> {
    let types = ObjectKind::ALL.iter().map(|k| format!("'{}'", k.object_type())).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "select object_name, object_type, status from all_objects
         where owner = :1 and object_type in ({types}, 'PACKAGE BODY', 'TYPE BODY')
           and object_name not like 'BIN$%' and nvl(generated, 'N') = 'N'
         order by object_name, object_type"
    );
    let rows = conn.query(&sql, &[&owner]).map_err(|e| message(&e))?;
    let mut out: Vec<DbObject> = Vec::new();
    for row in rows {
        let row = row.map_err(|e| message(&e))?;
        let name: String = row.get(0).map_err(|e| message(&e))?;
        let kind: String = row.get(1).map_err(|e| message(&e))?;
        let valid = row.get::<Option<String>>(2).ok().flatten().is_none_or(|s| s == "VALID");
        match kind.as_str() {
            // A body belongs to its spec, listed just before it.
            "PACKAGE BODY" | "TYPE BODY" => {
                let spec = if kind == "PACKAGE BODY" { ObjectKind::Package } else { ObjectKind::Type };
                if let Some(o) = out.iter_mut().rev().find(|o| o.name == name && o.kind == spec) {
                    o.has_body = true;
                    o.body_valid = valid;
                }
            }
            _ => {
                if let Some(kind) = ObjectKind::from_object_type(&kind) {
                    out.push(DbObject { name, kind, valid, has_body: false, body_valid: true });
                }
            }
        }
    }
    Ok(out)
}

/// A table's or view's columns.
pub fn columns(conn: &mut oracledb::Connection, owner: &str, table: &str) -> Result<Vec<ColumnInfo>, String> {
    let sql = "select c.column_name, c.data_type, c.data_length, c.data_precision, c.data_scale, c.nullable,
                      (select count(*) from all_cons_columns cc join all_constraints k
                         on k.owner = cc.owner and k.constraint_name = cc.constraint_name
                       where k.constraint_type = 'P' and cc.owner = c.owner and cc.table_name = c.table_name
                         and cc.column_name = c.column_name)
               from all_tab_columns c
               where c.owner = :1 and c.table_name = :2
               order by c.column_id";
    let rows = conn.query(sql, &[&owner, &table]).map_err(|e| message(&e))?;
    let mut out = Vec::new();
    for row in rows {
        let row = row.map_err(|e| message(&e))?;
        let name: String = row.get(0).map_err(|e| message(&e))?;
        let data_type: String = row.get::<Option<String>>(1).ok().flatten().unwrap_or_default();
        let length: Option<i64> = row.get(2).ok().flatten();
        let precision: Option<i64> = row.get(3).ok().flatten();
        let scale: Option<i64> = row.get(4).ok().flatten();
        let nullable = row.get::<Option<String>>(5).ok().flatten().is_none_or(|n| n == "Y");
        let primary = row.get::<i64>(6).unwrap_or(0) > 0;
        let shown = match (data_type.as_str(), precision, scale) {
            ("NUMBER", Some(p), Some(0)) => format!("NUMBER({p})"),
            ("NUMBER", Some(p), Some(s)) => format!("NUMBER({p},{s})"),
            ("VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" | "RAW", _, _) => format!("{data_type}({})", length.unwrap_or(0)),
            _ => data_type,
        };
        out.push(ColumnInfo { name, data_type: shown, nullable, primary });
    }
    Ok(out)
}

/// A package's (or type's) procedures and functions, in declaration order.
pub fn subprograms(conn: &mut oracledb::Connection, owner: &str, package: &str) -> Result<Vec<Subprogram>, String> {
    let sql = "select p.procedure_name,
                      (select count(*) from all_arguments a
                        where a.owner = p.owner and a.package_name = p.object_name and a.object_name = p.procedure_name
                          and nvl(a.overload, '0') = nvl(p.overload, '0') and a.position = 0 and a.argument_name is null)
               from all_procedures p
               where p.owner = :1 and p.object_name = :2 and p.procedure_name is not null
               order by p.subprogram_id";
    let rows = conn.query(sql, &[&owner, &package]).map_err(|e| message(&e))?;
    let mut out: Vec<Subprogram> = Vec::new();
    for row in rows {
        let row = row.map_err(|e| message(&e))?;
        let name: String = row.get(0).map_err(|e| message(&e))?;
        let function = row.get::<i64>(1).unwrap_or(0) > 0;
        // Overloads show once.
        if !out.iter().any(|s| s.name == name && s.function == function) {
            out.push(Subprogram { name, function });
        }
    }
    Ok(out)
}

/// An object's source as a script that creates it: PL/SQL from ALL_SOURCE
/// (`CREATE OR REPLACE ` + its text, the name qualified when it's in another
/// schema), anything else from DBMS_METADATA.
pub fn source(conn: &mut oracledb::Connection, user: &str, owner: &str, name: &str, kind: ObjectKind, body: bool) -> Result<String, String> {
    let Some(source_type) = kind.source_type(body) else {
        let ddl: Option<String> = conn
            .query_row("select dbms_metadata.get_ddl(:1, :2, :3) from dual", &[&kind.metadata_type(), &name, &owner])
            .and_then(|row| row.get(0))
            .map_err(|e| message(&e))?;
        return Ok(ddl.unwrap_or_default().trim_start_matches('\n').trim_end().to_string() + "\n");
    };
    let rows = conn
        .query("select text from all_source where owner = :1 and name = :2 and type = :3 order by line", &[&owner, &name, &source_type])
        .map_err(|e| message(&e))?;
    let mut text = String::new();
    for row in rows {
        let line: Option<String> = row.map_err(|e| message(&e))?.get(0).map_err(|e| message(&e))?;
        text.push_str(&line.unwrap_or_default());
    }
    if text.is_empty() {
        return Err(format!("{owner}.{name} has no {} source (it may be wrapped, or you may lack the privileges to read it)", source_type.to_lowercase()));
    }
    let text = if owner == user { text } else { qualify(&text, owner, name) };
    let mut script = format!("CREATE OR REPLACE {}", text.trim_end());
    script.push_str("\n/\n");
    Ok(script)
}

/// `PACKAGE foo AS …` → `PACKAGE "OWNER"."FOO" AS …`: the unit's name with
/// its schema, so compiling it from another user's connection replaces that one.
fn qualify(text: &str, owner: &str, name: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let Some(mut at) = find_word_ci(&lower, &name.to_ascii_lowercase()) else {
        return text.to_string();
    };
    let mut len = name.len();
    // Quoted: the quotes go too.
    if at > 0 && text[..at].ends_with('"') && text[at + len..].starts_with('"') {
        at -= 1;
        len += 2;
    }
    // Already qualified ("OWNER"."NAME" or owner.name)?
    if text[..at].trim_end().ends_with('.') {
        return text.to_string();
    }
    format!("{}\"{owner}\".\"{name}\"{}", &text[..at], &text[at + len..])
}

/// The first occurrence of `word` as a whole identifier.
fn find_word_ci(text: &str, word: &str) -> Option<usize> {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$' || c == '#';
    let mut from = 0;
    while let Some(i) = text[from..].find(word) {
        let at = from + i;
        let before = text[..at].chars().next_back().is_none_or(|c| !ident(c));
        let after = text[at + word.len()..].chars().next().is_none_or(|c| !ident(c));
        if before && after {
            return Some(at);
        }
        from = at + word.len();
    }
    None
}

/// A compile error from ALL_ERRORS: 1-based line and column in the unit's source.
#[derive(Clone, Debug)]
pub struct CompileError {
    pub line: usize,
    pub column: usize,
    pub message: String,
    pub warning: bool,
}

pub fn errors(conn: &mut oracledb::Connection, owner: &str, name: &str, source_type: &str) -> Result<Vec<CompileError>, String> {
    let rows = conn
        .query(
            "select line, position, text, attribute from all_errors where owner = :1 and name = :2 and type = :3 order by sequence",
            &[&owner, &name, &source_type],
        )
        .map_err(|e| message(&e))?;
    let mut out = Vec::new();
    for row in rows {
        let row = row.map_err(|e| message(&e))?;
        out.push(CompileError {
            line: row.get::<i64>(0).unwrap_or(1).max(1) as usize,
            column: row.get::<i64>(1).unwrap_or(1).max(1) as usize,
            message: row.get::<Option<String>>(2).ok().flatten().unwrap_or_default().trim().to_string(),
            warning: row.get::<Option<String>>(3).ok().flatten().is_some_and(|a| a == "WARNING"),
        });
    }
    Ok(out)
}

/// Runs a unit's `CREATE OR REPLACE …` and reads back its errors. Oracle
/// creates a unit that doesn't compile (ORA-24344), so that's not a failure.
pub fn compile(conn: &mut oracledb::Connection, sql: &str, owner: &str, name: &str, source_type: &str) -> Result<Vec<CompileError>, String> {
    if let Err(err) = conn.execute(sql, &[])
        && code(&err) != Some(24344)
    {
        return Err(message(&err));
    }
    errors(conn, owner, name, source_type)
}

/// ALTER … COMPILE for an object, then its errors.
pub fn recompile(conn: &mut oracledb::Connection, owner: &str, name: &str, kind: ObjectKind, body: bool) -> Result<Vec<CompileError>, String> {
    let what = match (kind, body) {
        (ObjectKind::Package, true) => "PACKAGE {} COMPILE BODY".to_string(),
        (ObjectKind::Type, true) => "TYPE {} COMPILE BODY".to_string(),
        (ObjectKind::View, _) => "VIEW {} COMPILE".to_string(),
        _ => format!("{} {{}} COMPILE", kind.object_type()),
    };
    let sql = what.replace("{}", &format!("\"{owner}\".\"{name}\""));
    compile(conn, &sql, owner, name, kind.source_type(body).unwrap_or(kind.object_type()))
}

// ---- Running statements ---------------------------------------------------------------

/// What one statement did.
#[derive(Clone, Debug)]
pub enum Outcome {
    Rows(Grid),
    /// DML: rows changed (not committed yet).
    Changed(u64),
    /// DDL and PL/SQL.
    Done,
    /// A PL/SQL unit created with compile errors.
    Compiled(Vec<CompileError>),
    Failed { message: String, offset: Option<usize> },
}

/// A query's result: column names and types, and its rows as text (None: NULL).
#[derive(Clone, Debug, Default)]
pub struct Grid {
    pub columns: Vec<String>,
    pub types: Vec<String>,
    /// Columns shown right-aligned (numbers).
    pub numeric: Vec<bool>,
    pub rows: Vec<Vec<Option<String>>>,
    /// More rows than `MAX_ROWS` were there.
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct Executed {
    pub outcome: Outcome,
    /// DBMS_OUTPUT's lines.
    pub output: Vec<String>,
    pub elapsed: Duration,
}

/// Runs one statement (SQL without its `;`, or a PL/SQL block).
pub fn execute(conn: &mut oracledb::Connection, sql: &str) -> Executed {
    let started = Instant::now();
    let outcome = match run_statement(conn, sql) {
        Ok(outcome) => outcome,
        Err(err) => Outcome::Failed { message: message(&err), offset: error_offset(&err) },
    };
    let elapsed = started.elapsed();
    let output = dbms_output(conn);
    Executed { outcome, output, elapsed }
}

fn run_statement(conn: &mut oracledb::Connection, sql: &str) -> Result<Outcome, oracledb::Error> {
    let mut statement = conn.statement(sql)?.prefetch_rows(200).fetch_array_size(200).build()?;
    if statement.is_query() {
        let cursor = statement.query(&[])?;
        let mut grid = Grid::default();
        for column in cursor.columns() {
            grid.columns.push(column.name().to_string());
            grid.types.push(column.data_type());
            let numeric = [oracledb::DB_TYPE_NUMBER, oracledb::DB_TYPE_BINARY_DOUBLE, oracledb::DB_TYPE_BINARY_FLOAT, oracledb::DB_TYPE_BINARY_INTEGER]
                .iter()
                .any(|t| std::ptr::eq(column.db_type(), *t) || column.db_type() == *t);
            grid.numeric.push(numeric);
        }
        for row in cursor {
            if grid.rows.len() == MAX_ROWS {
                grid.truncated = true;
                break;
            }
            let row = row?;
            grid.rows.push((0..grid.columns.len()).map(|i| cell(&row, i)).collect());
        }
        return Ok(Outcome::Rows(grid));
    }
    let dml = statement.is_dml();
    let plsql = statement.is_plsql();
    let ddl = statement.is_ddl();
    let result = match statement.execute(&[]) {
        Ok(result) => result,
        // A PL/SQL unit created with compile errors.
        Err(err) if code(&err) == Some(24344) => return Ok(Outcome::Compiled(unit_errors(conn, sql))),
        Err(err) => return Err(err),
    };
    if dml {
        return Ok(Outcome::Changed(result.rows_affected()));
    }
    if ddl || plsql {
        let errors = unit_errors(conn, sql);
        if !errors.is_empty() {
            return Ok(Outcome::Compiled(errors));
        }
    }
    Ok(Outcome::Done)
}

/// After `CREATE … PACKAGE x` and the like: that unit's errors.
fn unit_errors(conn: &mut oracledb::Connection, sql: &str) -> Vec<CompileError> {
    let Some((source_type, owner, name)) = created_unit(sql) else { return Vec::new() };
    let owner = owner.unwrap_or_else(|| {
        conn.query_row("select sys_context('USERENV', 'CURRENT_SCHEMA') from dual", &[])
            .and_then(|row| row.get::<String>(0))
            .unwrap_or_default()
    });
    errors(conn, &owner, &name, &source_type).unwrap_or_default()
}

/// The unit a `CREATE [OR REPLACE] … PACKAGE [BODY] [schema.]name` statement
/// creates: its ALL_ERRORS type, owner (if given) and name.
pub fn created_unit(sql: &str) -> Option<(String, Option<String>, String)> {
    let mut words = Vec::new();
    let mut rest = sql.trim_start();
    // Enough words to get past CREATE OR REPLACE EDITIONABLE PACKAGE BODY.
    while words.len() < 8 && !rest.is_empty() {
        rest = rest.trim_start();
        // A word ends at a space, `(` or `;` outside quotes ("HR"."V" is one).
        let mut quoted = false;
        let end = rest
            .char_indices()
            .find(|&(_, c)| {
                if c == '"' {
                    quoted = !quoted;
                }
                !quoted && (c.is_whitespace() || c == '(' || c == ';')
            })
            .map_or(rest.len(), |(i, _)| i);
        if end == 0 {
            break;
        }
        words.push(&rest[..end]);
        rest = &rest[end..];
    }
    let mut it = words.iter().map(|w| w.to_ascii_uppercase()).peekable();
    if it.next()? != "CREATE" {
        return None;
    }
    let mut ty = String::new();
    while let Some(w) = it.next() {
        match w.as_str() {
            "OR" | "REPLACE" | "EDITIONABLE" | "NONEDITIONABLE" | "FORCE" | "NONFORCE" => continue,
            "PACKAGE" | "TYPE" if it.peek().is_some_and(|n| n == "BODY") => {
                it.next();
                ty = format!("{w} BODY");
            }
            "PACKAGE" | "PROCEDURE" | "FUNCTION" | "TRIGGER" | "TYPE" | "VIEW" => ty = w,
            _ => return None,
        }
        break;
    }
    if ty.is_empty() {
        return None;
    }
    let target = words.iter().find(|w| {
        let upper = w.to_ascii_uppercase();
        !["CREATE", "OR", "REPLACE", "EDITIONABLE", "NONEDITIONABLE", "FORCE", "NONFORCE", "PACKAGE", "BODY", "PROCEDURE", "FUNCTION", "TRIGGER", "TYPE", "VIEW"]
            .contains(&upper.as_str())
    })?;
    let unquote = |s: &str| if s.starts_with('"') { s.trim_matches('"').to_string() } else { s.to_ascii_uppercase() };
    match split_qualified(target) {
        (Some(owner), name) => Some((ty, Some(unquote(owner)), unquote(name))),
        (None, name) => Some((ty, None, unquote(name))),
    }
}

/// `"A"."B"` / `a.b` → (Some(a), b), outside quotes.
fn split_qualified(text: &str) -> (Option<&str>, &str) {
    let mut quoted = false;
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '.' if !quoted => return (Some(&text[..i]), &text[i + 1..]),
            _ => {}
        }
    }
    (None, text)
}

/// DBMS_OUTPUT's waiting lines.
fn dbms_output(conn: &mut oracledb::Connection) -> Vec<String> {
    let mut lines = Vec::new();
    while lines.len() < 10_000 {
        let Ok(mut result) = conn.execute("begin dbms_output.get_line(:1, :2); end;", &[&oracledb::DB_TYPE_VARCHAR, &oracledb::DB_TYPE_NUMBER]) else {
            break;
        };
        let out = result.out_bind_data();
        if out.get::<Option<i64>>(1).ok().flatten() != Some(0) {
            break;
        }
        lines.push(out.get::<Option<String>>(0).ok().flatten().unwrap_or_default());
    }
    lines
}

/// A value as the grid shows it.
fn cell(row: &oracledb::Row, i: usize) -> Option<String> {
    if let Ok(v) = row.get::<Option<String>>(i) {
        return v;
    }
    if let Ok(v) = row.get::<Option<oracledb::OracleTimestamp>>(i) {
        return v.map(|v| v.to_string());
    }
    if let Ok(v) = row.get::<Option<f64>>(i) {
        return v.map(|v| v.to_string());
    }
    if let Ok(v) = row.get::<Option<bool>>(i) {
        return v.map(|v| if v { "TRUE".into() } else { "FALSE".into() });
    }
    if let Ok(v) = row.get::<Option<oracledb::OracleIntervalDS>>(i) {
        return v.map(|v| v.to_string());
    }
    if let Ok(v) = row.get::<Option<oracledb::OracleIntervalYM>>(i) {
        return v.map(|v| v.to_string());
    }
    if let Ok(v) = row.get::<Option<Vec<u8>>>(i) {
        return v.map(|bytes| {
            let mut hex: String = bytes.iter().take(256).map(|b| format!("{b:02X}")).collect();
            if bytes.len() > 256 {
                hex.push('…');
            }
            hex
        });
    }
    if let Ok(v) = row.get::<Option<oracledb::JsonValue>>(i) {
        return v.map(|v| format!("{v:?}"));
    }
    Some(format!("({})", row.columns().get(i).map(|c| c.data_type()).unwrap_or_default()))
}

pub fn commit(conn: &mut oracledb::Connection) -> Result<(), String> {
    conn.commit().map_err(|e| message(&e))
}

pub fn rollback(conn: &mut oracledb::Connection) -> Result<(), String> {
    conn.rollback().map_err(|e| message(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_created() {
        assert_eq!(created_unit("CREATE OR REPLACE PACKAGE BODY hr.pkg AS"), Some(("PACKAGE BODY".into(), Some("HR".into()), "PKG".into())));
        assert_eq!(created_unit("create or replace editionable procedure \"Mixed\"(a number) is"), Some(("PROCEDURE".into(), None, "Mixed".into())));
        assert_eq!(created_unit("create table t (a number)"), None);
        assert_eq!(created_unit("CREATE OR REPLACE FORCE VIEW \"HR\".\"V\" AS select 1"), Some(("VIEW".into(), Some("HR".into()), "V".into())));
    }

    #[test]
    fn qualifies_other_schemas_units() {
        assert_eq!(qualify("PACKAGE order_api AS\n  x number;\nEND order_api;", "HR", "ORDER_API"), "PACKAGE \"HR\".\"ORDER_API\" AS\n  x number;\nEND order_api;");
        assert_eq!(qualify("package body \"ORDER_API\" as", "HR", "ORDER_API"), "package body \"HR\".\"ORDER_API\" as");
        assert_eq!(qualify("PACKAGE hr.order_api AS", "HR", "ORDER_API"), "PACKAGE hr.order_api AS");
    }

    /// Against a real database: `KUANA_ORACLE_TEST=user/password@host:port/service
    /// cargo test oracle_end_to_end -- --ignored` (e.g. a gvenzl/oracle-free container).
    #[test]
    #[ignore]
    fn oracle_end_to_end() {
        let spec = std::env::var("KUANA_ORACLE_TEST").expect("KUANA_ORACLE_TEST=user/password@connect");
        let (credentials, connect) = spec.split_once('@').unwrap();
        let (user, password) = credentials.split_once('/').unwrap();
        let config = ConnectionConfig { connect_string: connect.into(), user: user.into(), ..Default::default() };
        let session = futures::executor::block_on(Session::connect(&config, password.into())).unwrap().unwrap();
        assert_eq!(session.user, user.to_uppercase());
        let run = |job: Box<dyn FnOnce(&mut oracledb::Connection) -> Result<Executed, String> + Send>| futures::executor::block_on(session.run(job)).unwrap();
        let exec = |sql: &'static str| run(Box::new(move |c| Ok(execute(c, sql))));

        let _ = exec("drop table kuana_t purge");
        assert!(matches!(exec("create table kuana_t (id number(10) primary key, name varchar2(20), born date, price number(8,2))").outcome, Outcome::Done));
        assert!(matches!(exec("insert into kuana_t values (1, 'Ada', date '1815-12-10', 12.5)").outcome, Outcome::Changed(1)));
        assert!(matches!(exec("insert into kuana_t (id) values (2)").outcome, Outcome::Changed(1)));
        let Outcome::Rows(grid) = exec("select * from kuana_t order by id").outcome else { panic!() };
        assert_eq!(grid.columns, ["ID", "NAME", "BORN", "PRICE"]);
        assert_eq!(grid.numeric, [true, false, false, true]);
        assert_eq!(grid.rows[0][0].as_deref(), Some("1"));
        assert_eq!(grid.rows[0][1].as_deref(), Some("Ada"));
        assert!(grid.rows[0][2].as_deref().unwrap().contains("1815"), "{:?}", grid.rows[0][2]);
        assert_eq!(grid.rows[0][3].as_deref(), Some("12.5"));
        assert_eq!(grid.rows[1][1], None);

        // DBMS_OUTPUT, and an error's position.
        let done = exec("begin dbms_output.put_line('hello'); dbms_output.put_line('world'); end;");
        assert!(matches!(done.outcome, Outcome::Done));
        assert_eq!(done.output, ["hello", "world"]);
        let Outcome::Failed { message, offset } = exec("select nosuch from kuana_t").outcome else { panic!() };
        assert!(message.contains("ORA-00904"), "{message}");
        assert_eq!(offset, Some(7));

        // A package with an error, then fixed; the catalog lists it.
        let bad = "create or replace package kuana_p as\n  procedure a;\nend;";
        assert!(matches!(exec(bad).outcome, Outcome::Done));
        let body = "create or replace package body kuana_p as\n  procedure a is\n  begin\n    nosuch := 1;\n  end;\nend;";
        let Outcome::Compiled(errors) = exec(body).outcome else { panic!() };
        assert_eq!(errors[0].line, 4, "{errors:?}");
        let user_owned = user.to_uppercase();
        let (u, u2) = (user_owned.clone(), user_owned.clone());
        let objects = futures::executor::block_on(session.run(move |c| objects(c, &u))).unwrap();
        let package = objects.iter().find(|o| o.name == "KUANA_P").unwrap();
        assert!(package.has_body && !package.body_valid);
        let fixed = "create or replace package body kuana_p as\n  procedure a is\n  begin\n    null;\n  end;\nend;";
        let errors = futures::executor::block_on(session.run(move |c| compile(c, fixed, &u2, "KUANA_P", "PACKAGE BODY"))).unwrap();
        assert!(errors.is_empty(), "{errors:?}");

        // Source as a script, and DDL; columns and subprograms.
        let (u, u2, u3) = (user_owned.clone(), user_owned.clone(), user_owned.clone());
        let user_for_source = user_owned.clone();
        let spec = futures::executor::block_on(session.run(move |c| source(c, &user_for_source, &u, "KUANA_P", ObjectKind::Package, true))).unwrap();
        assert!(spec.starts_with("CREATE OR REPLACE package body kuana_p"), "{spec}");
        assert!(spec.ends_with("\n/\n"));
        let user_for_ddl = user_owned.clone();
        let ddl = futures::executor::block_on(session.run(move |c| source(c, &user_for_ddl, &u2, "KUANA_T", ObjectKind::Table, false))).unwrap();
        assert!(ddl.contains("CREATE TABLE"), "{ddl}");
        let cols = futures::executor::block_on(session.run(move |c| columns(c, &u3, "KUANA_T"))).unwrap();
        assert_eq!(cols[0], ColumnInfo { name: "ID".into(), data_type: "NUMBER(10)".into(), nullable: false, primary: true });
        assert_eq!(cols[1].data_type, "VARCHAR2(20)");
        let u4 = user_owned.clone();
        let subs = futures::executor::block_on(session.run(move |c| subprograms(c, &u4, "KUANA_P"))).unwrap();
        assert_eq!(subs, [Subprogram { name: "A".into(), function: false }]);
        let schemas = futures::executor::block_on(session.run(schemas)).unwrap();
        assert!(schemas.contains(&user_owned));

        let _ = exec("drop package kuana_p");
        let _ = exec("drop table kuana_t purge");
    }

    #[test]
    fn connect_strings() {
        let mut config = ConnectionConfig { host: "db".into(), port: 0, service: "FREEPDB1".into(), ..Default::default() };
        assert_eq!(connect_string(&config), "db:1521/FREEPDB1");
        config.use_sid = true;
        assert!(connect_string(&config).contains("(SID=FREEPDB1)"));
        config.connect_string = " mydb_high ".into();
        assert_eq!(connect_string(&config), "mydb_high");
    }
}

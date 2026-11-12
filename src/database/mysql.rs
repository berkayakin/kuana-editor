//! MySQL and MariaDB through the `mysql` crate (blocking, pure Rust). Queries
//! use the text protocol, so values arrive as text; a long result is read up
//! to `MAX_ROWS`, then stopped with `KILL QUERY` from a second connection.
//! A statement's warnings are its output.

use std::time::Instant;

use mysql::{Conn, Opts, OptsBuilder, SslOpts, consts::ColumnType, prelude::Queryable};

use super::{
    ColumnInfo, ConnectionConfig, DbObject, ObjectFile, ObjectKind, SslMode,
    backend::{Backend, CONNECT_TIMEOUT, Connected, Executed, FileProblem, Grid, MAX_ROWS, Outcome, char_to_byte, is_dml},
};

/// Schemas only the server uses.
const SYSTEM_SCHEMAS: [&str; 2] = ["information_schema", "performance_schema"];

/// Connects (blocking, on the session's thread).
pub fn connect(config: &ConnectionConfig, password: &str) -> Result<Connected, String> {
    // The driver builds its TLS setup from the process's default (Kuana has
    // two in its dependencies, so none is picked for it).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let database = config.database.trim();
    let base = OptsBuilder::new()
        .ip_or_hostname(Some(config.host.trim()))
        .tcp_port(if config.port == 0 { 3306 } else { config.port })
        .user(Some(config.user.trim()))
        .pass(Some(password))
        .db_name((!database.is_empty()).then_some(database))
        .tcp_connect_timeout(Some(CONNECT_TIMEOUT))
        .prefer_socket(false);
    // As with PostgreSQL: Prefer and Require don't check the certificate.
    let unchecked = SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true);
    let open = |opts: OptsBuilder| Conn::new(opts.clone()).map(|conn| (conn, Opts::from(opts)));
    let (mut conn, opts) = match config.ssl {
        SslMode::Disable => open(base),
        SslMode::Prefer => match open(base.clone().ssl_opts(unchecked)) {
            Err(mysql::Error::DriverError(mysql::DriverError::TlsNotSupported)) => open(base),
            other => other,
        },
        SslMode::Require => open(base.ssl_opts(unchecked)),
        SslMode::Verify => open(base.ssl_opts(SslOpts::default())),
    }
    .map_err(|e| message(&e))?;
    let (schema, version): (Option<String>, String) = conn.query_first("select database(), version()").map_err(|e| message(&e))?.unwrap_or((None, String::new()));
    let version = match version.split_once("-MariaDB") {
        Some((number, _)) => format!("MariaDB {}", number.rsplit('-').next().unwrap_or(number)),
        None => format!("MySQL {}", version.split('-').next().unwrap_or(&version)),
    };
    let schema = schema.unwrap_or_default();
    let label = if schema.is_empty() { config.user.trim().to_string() } else { schema.clone() };
    Ok(Connected { backend: Box::new(Mysql { conn, opts }), schema, label, version })
}

struct Mysql {
    conn: Conn,
    /// For the connection that stops a long query.
    opts: Opts,
}

/// An error as the mysql client shows it.
fn message(err: &mysql::Error) -> String {
    match err {
        mysql::Error::MySqlError(e) => format!("ERROR {} ({}): {}", e.code, e.state, e.message),
        other => other.to_string(),
    }
}

/// Where a syntax error is, in characters into `sql`, from MySQL's
/// "… near 'rest of the statement' at line N".
fn error_offset(sql: &str, err: &mysql::Error) -> Option<usize> {
    let mysql::Error::MySqlError(e) = err else { return None };
    let (_, after) = e.message.rsplit_once(" near '")?;
    let (near, line) = after.rsplit_once("' at line ")?;
    let line: usize = line.trim().parse().ok()?;
    let line_start: usize = sql.split_inclusive('\n').take(line.saturating_sub(1)).map(str::len).sum();
    if near.is_empty() {
        return Some(sql.chars().count());
    }
    // MySQL cuts what it quotes; a prefix of it finds the place.
    let probe: String = near.chars().take(24).collect();
    let at = line_start + sql.get(line_start..)?.find(probe.as_str())?;
    Some(sql[..at].chars().count())
}

impl Mysql {
    fn rows(&mut self, sql: &str, params: impl Into<mysql::Params>) -> Result<Vec<mysql::Row>, String> {
        self.conn.exec(sql, params).map_err(|e| message(&e))
    }

    fn run(&mut self, sql: &str) -> Result<(Option<Grid>, u64, u16), mysql::Error> {
        let id = self.conn.connection_id();
        let mut stop = false;
        let mut result = self.conn.query_iter(sql)?;
        let columns: Vec<mysql::Column> = result.columns().as_ref().to_vec();
        let mut grid = None;
        if !columns.is_empty() {
            let mut g = Grid {
                columns: columns.iter().map(|c| c.name_str().into_owned()).collect(),
                types: columns.iter().map(|c| type_name(c.column_type())).collect(),
                numeric: columns.iter().map(|c| numeric(c.column_type())).collect(),
                ..Default::default()
            };
            if let Some(set) = result.iter() {
                for row in set {
                    let row = row?;
                    if g.rows.len() == MAX_ROWS {
                        g.truncated = true;
                        stop = true;
                        break;
                    }
                    g.rows.push((0..row.len()).map(|i| row.as_ref(i).and_then(text)).collect());
                }
            }
            grid = Some(g);
        }
        let affected = result.affected_rows();
        // Dropping the result reads the rest of it: a long one is stopped
        // first, before anything else can run on this connection.
        if stop && let Ok(mut other) = Conn::new(self.opts.clone()) {
            let _ = other.query_drop(format!("KILL QUERY {id}"));
        }
        drop(result);
        // The last packet's count (a result set's ends with it).
        Ok((grid, affected, self.conn.warnings()))
    }

    /// SHOW WARNINGS, as lines.
    fn warnings(&mut self) -> Vec<String> {
        let rows: Vec<(String, u32, String)> = self.conn.query("SHOW WARNINGS").unwrap_or_default();
        rows.into_iter().map(|(level, code, text)| format!("{level} {code}: {text}")).collect()
    }

    /// `SHOW CREATE …`'s statement (in column `column`).
    fn show_create(&mut self, what: &str, schema: &str, name: &str, column: usize) -> Result<String, String> {
        let sql = format!("SHOW CREATE {what} {}.{}", quote(schema), quote(name));
        let row = self.conn.query_first::<mysql::Row, _>(sql).map_err(|e| message(&e))?.ok_or_else(|| format!("{schema}.{name} isn't there any more"))?;
        row.as_ref(column).and_then(text).ok_or_else(|| format!("You may lack the privileges to read {schema}.{name}'s definition"))
    }
}

fn quote(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

/// A value as the grid shows it (the text protocol sends text).
fn text(value: &mysql::Value) -> Option<String> {
    match value {
        mysql::Value::NULL => None,
        mysql::Value::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

fn numeric(ty: ColumnType) -> bool {
    use ColumnType::*;
    matches!(ty, MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_INT24 | MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE | MYSQL_TYPE_YEAR)
}

fn type_name(ty: ColumnType) -> String {
    format!("{ty:?}").trim_start_matches("MYSQL_TYPE_").to_lowercase()
}

/// `CREATE … VIEW `v`` → `CREATE … VIEW `schema`.`v``: SHOW CREATE leaves
/// routines, triggers and views unqualified, and they'd be made again in
/// the session's database otherwise.
fn qualify(create: &str, keyword: &str, schema: &str, name: &str) -> String {
    let upper = create.to_ascii_uppercase();
    let Some(at) = upper.find(&format!(" {keyword} ")) else { return create.to_string() };
    let after = at + keyword.len() + 2;
    let rest = &create[after..];
    let quoted = quote(name);
    match rest.strip_prefix(&quoted) {
        Some(tail) if !tail.starts_with('.') => format!("{}{}.{quoted}{tail}", &create[..after], quote(schema)),
        _ => create.to_string(),
    }
}

/// The keyword SHOW CREATE and DROP use for an object kind.
fn keyword(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::View => "VIEW",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Trigger => "TRIGGER",
        _ => "TABLE",
    }
}

impl Backend for Mysql {
    fn schemas(&mut self) -> Result<Vec<String>, String> {
        let names: Vec<String> = self.conn.query("SHOW DATABASES").map_err(|e| message(&e))?;
        Ok(names.into_iter().filter(|n| !SYSTEM_SCHEMAS.contains(&n.to_ascii_lowercase().as_str())).collect())
    }

    fn objects(&mut self, schema: &str) -> Result<Vec<DbObject>, String> {
        let sql = "select table_name, if(table_type = 'VIEW', 'VIEW', 'TABLE') from information_schema.tables where table_schema = ? and table_type in ('BASE TABLE', 'VIEW')
                   union all select routine_name, routine_type from information_schema.routines where routine_schema = ?
                   union all select trigger_name, 'TRIGGER' from information_schema.triggers where trigger_schema = ?
                   order by 1";
        let rows: Vec<(String, String)> = self.conn.exec(sql, (schema, schema, schema)).map_err(|e| message(&e))?;
        Ok(rows
            .into_iter()
            .map(|(name, kind)| {
                let kind = match kind.as_str() {
                    "VIEW" => ObjectKind::View,
                    "PROCEDURE" => ObjectKind::Procedure,
                    "FUNCTION" => ObjectKind::Function,
                    "TRIGGER" => ObjectKind::Trigger,
                    _ => ObjectKind::Table,
                };
                DbObject { name, kind, valid: true, has_body: false, body_valid: true }
            })
            .collect())
    }

    fn columns(&mut self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>, String> {
        let rows = self.rows(
            "select column_name, column_type, is_nullable, column_key from information_schema.columns where table_schema = ? and table_name = ? order by ordinal_position",
            (schema, table),
        )?;
        Ok(rows
            .iter()
            .map(|r| {
                let get = |i: usize| r.as_ref(i).and_then(text).unwrap_or_default();
                ColumnInfo { name: get(0), data_type: get(1), nullable: get(2) == "YES", primary: get(3) == "PRI" }
            })
            .collect())
    }

    fn source(&mut self, schema: &str, name: &str, kind: ObjectKind, _body: bool) -> Result<String, String> {
        let what = keyword(kind);
        let text = match kind {
            ObjectKind::View => {
                let create = self.show_create("VIEW", schema, name, 1)?;
                // CREATE ALGORITHM=… VIEW → CREATE OR REPLACE ALGORITHM=… VIEW
                let replaced = match create.get(..7) {
                    Some(start) if start.eq_ignore_ascii_case("CREATE ") => format!("CREATE OR REPLACE {}", &create[7..]),
                    _ => create,
                };
                qualify(&replaced, what, schema, name)
            }
            ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Trigger => qualify(&self.show_create(what, schema, name, 2)?, what, schema, name),
            _ => {
                let create = self.show_create("TABLE", schema, name, 1)?;
                qualify(&create, "TABLE", schema, name)
            }
        };
        Ok(format!("{};\n", text.trim_end().trim_end_matches(';')))
    }

    /// A view is replaced in place; a routine or trigger, which MySQL can't
    /// replace, is dropped and created, and if the new one fails the old one
    /// is put back.
    fn save_object(&mut self, object: &ObjectFile, text: &str) -> Result<Vec<FileProblem>, String> {
        let sql = text.trim().trim_end_matches(';').trim_end();
        let lead = text.len() - text.trim_start().len();
        let problem = |err: &mysql::Error| FileProblem { offset: lead + error_offset(sql, err).map_or(0, |at| char_to_byte(sql, at)), message: message(err), warning: false };
        if object.kind == ObjectKind::View {
            return Ok(match self.conn.query_drop(sql) {
                Ok(()) => Vec::new(),
                Err(err) => vec![problem(&err)],
            });
        }
        let what = keyword(object.kind);
        let old = self.source(&object.owner, &object.name, object.kind, false).ok();
        let target = format!("{}.{}", quote(&object.owner), quote(&object.name));
        self.conn.query_drop(format!("DROP {what} IF EXISTS {target}")).map_err(|e| message(&e))?;
        match self.conn.query_drop(sql) {
            Ok(()) => Ok(Vec::new()),
            Err(err) => {
                if let Some(old) = old {
                    let restored = self.conn.query_drop(old.trim_end().trim_end_matches(';'));
                    if let Err(again) = restored {
                        return Err(format!("{}; putting back the old one failed too: {}", message(&err), message(&again)));
                    }
                }
                Ok(vec![problem(&err)])
            }
        }
    }

    fn execute(&mut self, sql: &str) -> Executed {
        let started = Instant::now();
        let (outcome, warnings) = match self.run(sql) {
            Ok((Some(grid), _, warnings)) => (Outcome::Rows(grid), warnings),
            Ok((None, rows, warnings)) if is_dml(sql) => (Outcome::Changed { rows, pending: false }, warnings),
            Ok((None, _, warnings)) => (Outcome::Done, warnings),
            Err(err) => (Outcome::Failed { message: message(&err), offset: error_offset(sql, &err) }, 0),
        };
        let output = if warnings > 0 { self.warnings() } else { Vec::new() };
        Executed { outcome, output, elapsed: started.elapsed() }
    }

    fn commit(&mut self) -> Result<(), String> {
        self.conn.query_drop("COMMIT").map_err(|e| message(&e))
    }

    fn rollback(&mut self) -> Result<(), String> {
        self.conn.query_drop("ROLLBACK").map_err(|e| message(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_and_finds_errors() {
        assert_eq!(
            qualify("CREATE DEFINER=`u`@`%` PROCEDURE `p`(in a int)\nBEGIN\n  SELECT a;\nEND", "PROCEDURE", "shop", "p"),
            "CREATE DEFINER=`u`@`%` PROCEDURE `shop`.`p`(in a int)\nBEGIN\n  SELECT a;\nEND"
        );
        assert_eq!(qualify("CREATE VIEW `shop`.`v` AS select 1", "VIEW", "shop", "v"), "CREATE VIEW `shop`.`v` AS select 1");
        let err = mysql::Error::MySqlError(mysql::MySqlError {
            state: "42000".into(),
            message: "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'frm t' at line 2".into(),
            code: 1064,
        });
        let sql = "select a\n  frm t";
        assert_eq!(error_offset(sql, &err), Some(11));
    }

    /// Against a real server: `KUANA_MYSQL_TEST=user:password@host:port/database
    /// cargo test mysql_end_to_end -- --ignored` (MySQL or MariaDB).
    #[test]
    #[ignore]
    fn mysql_end_to_end() {
        let spec = std::env::var("KUANA_MYSQL_TEST").expect("KUANA_MYSQL_TEST=user:password@host:port/database");
        let (login, place) = spec.split_once('@').unwrap();
        let (user, password) = login.split_once(':').unwrap();
        let (address, database) = place.split_once('/').unwrap();
        let (host, port) = address.split_once(':').unwrap();
        let config = ConnectionConfig {
            kind: super::super::DbKind::Mysql,
            host: host.into(),
            port: port.parse().unwrap(),
            database: database.into(),
            user: user.into(),
            ssl: SslMode::Prefer,
            ..Default::default()
        };
        let Connected { mut backend, schema, version, .. } = connect(&config, password).unwrap();
        let db = backend.as_mut();
        assert_eq!(schema, database);
        assert!(version.starts_with("MySQL ") || version.starts_with("MariaDB "), "{version}");

        for sql in ["drop view if exists kuana_cheap", "drop procedure if exists kuana_touch", "drop table if exists kuana_items"] {
            db.execute(sql);
        }
        let create = "create table kuana_items (id int auto_increment primary key, name varchar(20) not null, price decimal(8,2), born date)";
        assert!(matches!(db.execute(create).outcome, Outcome::Done));
        let inserted = db.execute("insert into kuana_items (name, price, born) values ('Ada', 12.5, '1815-12-10'), ('Bob', null, null)");
        assert!(matches!(inserted.outcome, Outcome::Changed { rows: 2, pending: false }), "{:?}", inserted.outcome);
        let Outcome::Rows(grid) = db.execute("select id, name, price, born from kuana_items order by id").outcome else { panic!() };
        assert_eq!(grid.columns, ["id", "name", "price", "born"]);
        assert_eq!(grid.rows[0], [Some("1".into()), Some("Ada".into()), Some("12.50".into()), Some("1815-12-10".into())]);
        assert_eq!(grid.rows[1][2], None);
        assert_eq!(grid.numeric, [true, false, true, false]);

        // A long result stops at MAX_ROWS and the connection carries on.
        let big = "select a.n from (select 1 n union all select 2 union all select 3 union all select 4 union all select 5 union all select 6 union all select 7 union all select 8 union all select 9 union all select 10) a, (select 1 n union all select 2 union all select 3 union all select 4 union all select 5 union all select 6 union all select 7 union all select 8 union all select 9 union all select 10) b, (select 1 n union all select 2 union all select 3 union all select 4 union all select 5 union all select 6 union all select 7 union all select 8 union all select 9 union all select 10) c, (select 1 n union all select 2 union all select 3) d";
        let Outcome::Rows(rows) = db.execute(big).outcome else { panic!() };
        assert!(rows.truncated && rows.rows.len() == MAX_ROWS);
        assert!(matches!(db.execute("select 1").outcome, Outcome::Rows(_)));

        // Errors and warnings.
        let Outcome::Failed { message, offset } = db.execute("select name\nfrm kuana_items").outcome else { panic!() };
        assert!(message.contains("1064"), "{message}");
        // `frm` reads as an alias for `name`: MySQL points at the table.
        assert_eq!(offset, Some(16));
        let warned = db.execute("select 1/0");
        assert!(warned.output.iter().any(|w| w.contains("1365")), "{:?}", warned.output);

        // The catalog, sources, and saving them back.
        db.execute("create view kuana_cheap as select name from kuana_items where price < 20");
        db.execute("create procedure kuana_touch() begin update kuana_items set name = name; select 1; end");
        let objects = db.objects(database).unwrap();
        let has = |name: &str, kind| objects.iter().any(|o| o.name == name && o.kind == kind);
        assert!(has("kuana_items", ObjectKind::Table) && has("kuana_cheap", ObjectKind::View) && has("kuana_touch", ObjectKind::Procedure), "{objects:?}");
        let cols = db.columns(database, "kuana_items").unwrap();
        // MariaDB still shows display widths: int(11).
        assert!(cols[0].name == "id" && cols[0].data_type.starts_with("int") && !cols[0].nullable && cols[0].primary, "{:?}", cols[0]);
        assert!(db.source(database, "kuana_items", ObjectKind::Table, false).unwrap().contains("CREATE TABLE"));
        let view = db.source(database, "kuana_cheap", ObjectKind::View, false).unwrap();
        assert!(view.starts_with("CREATE OR REPLACE"), "{view}");
        let file = ObjectFile { connection: "c".into(), owner: database.into(), name: "kuana_cheap".into(), kind: ObjectKind::View, body: false };
        assert!(db.save_object(&file, &view.replace("< 20", "< 30")).unwrap().is_empty());
        let procedure = db.source(database, "kuana_touch", ObjectKind::Procedure, false).unwrap();
        let file = ObjectFile { connection: "c".into(), owner: database.into(), name: "kuana_touch".into(), kind: ObjectKind::Procedure, body: false };
        let problems = db.save_object(&file, &procedure.replace("select 1", "selec 1")).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        // The old one was put back.
        assert!(db.source(database, "kuana_touch", ObjectKind::Procedure, false).unwrap().contains("select 1"));
        assert!(db.save_object(&file, &procedure.replace("select 1", "select 2")).unwrap().is_empty());
        assert!(matches!(db.execute("call kuana_touch()").outcome, Outcome::Rows(ref g) if g.rows[0][0].as_deref() == Some("2")));

        for sql in ["drop view kuana_cheap", "drop procedure kuana_touch", "drop table kuana_items"] {
            db.execute(sql);
        }
    }
}

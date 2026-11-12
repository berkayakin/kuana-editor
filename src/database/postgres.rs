//! PostgreSQL through tokio-postgres, driven on the session's thread by a
//! runtime of its own. Statements go by the simple query protocol, so every
//! value arrives as text, row by row: a result is read up to `MAX_ROWS`, then
//! the server is asked to stop. Notices (`RAISE NOTICE`) are the output.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use futures::StreamExt;
use rustls::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use tokio_postgres::{
    AsyncMessage, Client, NoTls, SimpleQueryMessage, Socket,
    error::ErrorPosition,
    tls::{MakeTlsConnect, TlsConnect},
};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::{
    ColumnInfo, ConnectionConfig, DbObject, ObjectFile, ObjectKind, SslMode,
    backend::{Backend, CONNECT_TIMEOUT, Connected, Executed, FileProblem, Grid, MAX_ROWS, Outcome, char_to_byte, is_dml},
    script::{self, Dialect},
};

/// Connects (blocking, on the session's thread).
pub fn connect(config: &ConnectionConfig, password: &str) -> Result<Connected, String> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let mut pg = tokio_postgres::Config::new();
    pg.host(config.host.trim())
        .port(if config.port == 0 { 5432 } else { config.port })
        .user(config.user.trim())
        .password(password)
        .application_name("Kuana")
        .connect_timeout(CONNECT_TIMEOUT)
        .ssl_mode(match config.ssl {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::Verify => tokio_postgres::config::SslMode::Require,
        });
    if !config.database.trim().is_empty() {
        pg.dbname(config.database.trim());
    }
    let notices = Arc::new(Mutex::new(Vec::new()));
    let tls = match config.ssl {
        SslMode::Disable => Tls::None,
        mode => Tls::Rustls(MakeRustlsConnect::new(tls_config(mode == SslMode::Verify)?)),
    };
    let client = match &tls {
        Tls::None => start(&runtime, &pg, NoTls, notices.clone()),
        Tls::Rustls(tls) => start(&runtime, &pg, tls.clone(), notices.clone()),
    }
    .map_err(|e| message(&e))?;
    let mut db = Postgres { client, tls, notices, runtime };
    let row = db.query_one("select current_database()::text, coalesce(current_schema()::text, 'public'), current_setting('server_version')", &[])?;
    let (database, schema, version): (String, String, String) = (row.get(0), row.get(1), row.get(2));
    let version = format!("PostgreSQL {}", version.split_whitespace().next().unwrap_or(&version));
    Ok(Connected { backend: Box::new(db), schema, label: database, version })
}

/// Connects and starts the task that drives the connection (it runs while
/// the session's thread waits on the runtime) and collects its notices.
fn start<T>(runtime: &tokio::runtime::Runtime, config: &tokio_postgres::Config, tls: T, notices: Arc<Mutex<Vec<String>>>) -> Result<Client, tokio_postgres::Error>
where
    T: MakeTlsConnect<Socket> + 'static,
    T::Stream: Send + 'static,
    T::TlsConnect: Send,
    <T::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    let (client, mut connection) = runtime.block_on(config.connect(tls))?;
    runtime.spawn(async move {
        let mut messages = futures::stream::poll_fn(move |cx| connection.poll_message(cx));
        while let Some(message) = messages.next().await {
            match message {
                Ok(AsyncMessage::Notice(notice)) => {
                    let line = match notice.severity() {
                        "NOTICE" | "INFO" | "LOG" => notice.message().to_string(),
                        severity => format!("{severity}: {}", notice.message()),
                    };
                    notices.lock().unwrap_or_else(|e| e.into_inner()).push(line);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    Ok(client)
}

/// TLS as libpq does it: encrypted, the certificate checked only for Verify
/// (against the system's certificates, and the host name).
fn tls_config(verify: bool) -> Result<ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions().map_err(|e| e.to_string())?;
    if verify {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        Ok(builder.with_root_certificates(roots).with_no_client_auth())
    } else {
        Ok(builder.dangerous().with_custom_certificate_verifier(Arc::new(AnyCertificate(provider))).with_no_client_auth())
    }
}

/// Accepts the server's certificate whatever it is (libpq's `prefer` and
/// `require`), still checking that the handshake is signed by it.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

enum Tls {
    None,
    Rustls(MakeRustlsConnect),
}

struct Postgres {
    // Dropped before the runtime: the connection closes on it.
    client: Client,
    tls: Tls,
    notices: Arc<Mutex<Vec<String>>>,
    runtime: tokio::runtime::Runtime,
}

/// An error as psql shows it: severity and message, then its detail and hint.
fn message(err: &tokio_postgres::Error) -> String {
    let Some(db) = err.as_db_error() else { return err.to_string() };
    let mut text = format!("{}: {}", db.severity(), db.message());
    if let Some(detail) = db.detail() {
        text.push_str(&format!("\nDETAIL: {detail}"));
    }
    if let Some(hint) = db.hint() {
        text.push_str(&format!("\nHINT: {hint}"));
    }
    text
}

/// Where the server points in the statement, in characters.
fn position(err: &tokio_postgres::Error) -> Option<usize> {
    match err.as_db_error()?.position()? {
        ErrorPosition::Original(at) => Some((*at as usize).saturating_sub(1)),
        ErrorPosition::Internal { .. } => None,
    }
}

impl Postgres {
    fn query(&mut self, sql: &str, params: &[&(dyn tokio_postgres::types::ToSql + Sync)]) -> Result<Vec<tokio_postgres::Row>, String> {
        self.runtime.block_on(self.client.query(sql, params)).map_err(|e| message(&e))
    }

    fn query_one(&mut self, sql: &str, params: &[&(dyn tokio_postgres::types::ToSql + Sync)]) -> Result<tokio_postgres::Row, String> {
        self.runtime.block_on(self.client.query_one(sql, params)).map_err(|e| message(&e))
    }

    fn batch(&mut self, sql: &str) -> Result<(), tokio_postgres::Error> {
        self.runtime.block_on(self.client.batch_execute(sql))
    }

    /// Runs one statement through the simple protocol.
    fn run(&mut self, sql: &str) -> Result<(Option<Grid>, u64), tokio_postgres::Error> {
        let cancel = self.client.cancel_token();
        let tls = match &self.tls {
            Tls::None => None,
            Tls::Rustls(tls) => Some(tls.clone()),
        };
        let client = &self.client;
        self.runtime.block_on(async move {
            let stream = client.simple_query_raw(sql).await?;
            futures::pin_mut!(stream);
            let mut grid: Option<Grid> = None;
            let mut count = 0;
            let mut cancelled = false;
            while let Some(message) = stream.next().await {
                match message {
                    Ok(SimpleQueryMessage::RowDescription(columns)) => {
                        grid.get_or_insert_with(|| Grid {
                            columns: columns.iter().map(|c| c.name().to_string()).collect(),
                            types: vec![String::new(); columns.len()],
                            ..Default::default()
                        });
                    }
                    Ok(SimpleQueryMessage::Row(row)) => {
                        let Some(grid) = grid.as_mut() else { continue };
                        if grid.rows.len() < MAX_ROWS {
                            grid.rows.push((0..row.len()).map(|i| row.get(i).map(str::to_string)).collect());
                        } else if !cancelled {
                            // Enough: the server stops sending the rest.
                            grid.truncated = true;
                            cancelled = true;
                            let _ = match &tls {
                                None => cancel.cancel_query(NoTls).await,
                                Some(tls) => cancel.cancel_query(tls.clone()).await,
                            };
                        }
                    }
                    Ok(SimpleQueryMessage::CommandComplete(n)) => count = n,
                    Ok(_) => {}
                    Err(_) if cancelled => break,
                    Err(err) => return Err(err),
                }
            }
            Ok((grid, count))
        })
    }

    fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn regclass(schema: &str, name: &str) -> String {
        format!("{}.{}", quote(schema), quote(name))
    }
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

impl Backend for Postgres {
    fn schemas(&mut self) -> Result<Vec<String>, String> {
        let rows = self.query("select nspname::text from pg_namespace where nspname !~ '^pg_' and nspname <> 'information_schema' order by 1", &[])?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    fn objects(&mut self, schema: &str) -> Result<Vec<DbObject>, String> {
        // What an extension installed isn't listed (`pg_depend` 'e').
        let sql = "select c.relname::text, c.relkind::text from pg_class c join pg_namespace n on n.oid = c.relnamespace
                   where n.nspname = $1 and c.relkind in ('r', 'p', 'f', 'v', 'm', 'S') and not c.relispartition
                     and not exists (select 1 from pg_depend d where d.objid = c.oid and d.deptype = 'e')
                   union all
                   select p.proname::text, case p.prokind when 'p' then 'proc' else 'func' end from pg_proc p join pg_namespace n on n.oid = p.pronamespace
                   where n.nspname = $1 and p.prokind in ('f', 'p')
                     and not exists (select 1 from pg_depend d where d.objid = p.oid and d.deptype = 'e')
                   union all
                   select t.tgname::text, 'trigger' from pg_trigger t join pg_class c on c.oid = t.tgrelid join pg_namespace n on n.oid = c.relnamespace
                   where n.nspname = $1 and not t.tgisinternal
                   order by 1";
        let mut out: Vec<DbObject> = Vec::new();
        for row in self.query(sql, &[&schema])? {
            let (name, kind): (String, String) = (row.get(0), row.get(1));
            let kind = match kind.as_str() {
                "r" | "p" | "f" => ObjectKind::Table,
                "v" => ObjectKind::View,
                "m" => ObjectKind::MaterializedView,
                "S" => ObjectKind::Sequence,
                "proc" => ObjectKind::Procedure,
                "func" => ObjectKind::Function,
                _ => ObjectKind::Trigger,
            };
            // Overloads are one function.
            if !out.iter().any(|o| o.name == name && o.kind == kind) {
                out.push(DbObject { name, kind, valid: true, has_body: false, body_valid: true });
            }
        }
        Ok(out)
    }

    fn columns(&mut self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>, String> {
        let sql = "select a.attname::text, format_type(a.atttypid, a.atttypmod), not a.attnotnull,
                          coalesce((select true from pg_index i where i.indrelid = a.attrelid and i.indisprimary and a.attnum = any(i.indkey)), false)
                   from pg_attribute a join pg_class c on c.oid = a.attrelid join pg_namespace n on n.oid = c.relnamespace
                   where n.nspname = $1 and c.relname = $2 and a.attnum > 0 and not a.attisdropped
                   order by a.attnum";
        let rows = self.query(sql, &[&schema, &table])?;
        Ok(rows.iter().map(|r| ColumnInfo { name: r.get(0), data_type: r.get(1), nullable: r.get(2), primary: r.get(3) }).collect())
    }

    fn source(&mut self, schema: &str, name: &str, kind: ObjectKind, _body: bool) -> Result<String, String> {
        let target = Self::regclass(schema, name);
        match kind {
            ObjectKind::View | ObjectKind::MaterializedView => {
                let def: String = self.query_one("select pg_get_viewdef($1::text::regclass, true)", &[&target])?.get(0);
                let create = if kind == ObjectKind::View { "CREATE OR REPLACE VIEW" } else { "CREATE MATERIALIZED VIEW" };
                Ok(format!("{create} {target} AS\n{};\n", def.trim().trim_end_matches(';')))
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let prokind = if kind == ObjectKind::Procedure { "p" } else { "f" };
                let rows = self.query(
                    "select pg_get_functiondef(p.oid) from pg_proc p join pg_namespace n on n.oid = p.pronamespace
                     where n.nspname = $1 and p.proname = $2 and p.prokind::text = $3 order by p.oid",
                    &[&schema, &name, &prokind],
                )?;
                if rows.is_empty() {
                    return Err(format!("{schema}.{name} isn't there any more"));
                }
                Ok(rows.iter().map(|r| format!("{};\n", r.get::<_, String>(0).trim_end())).collect::<Vec<_>>().join("\n"))
            }
            ObjectKind::Trigger => {
                let row = self.query_one(
                    "select pg_get_triggerdef(t.oid, true) from pg_trigger t join pg_class c on c.oid = t.tgrelid join pg_namespace n on n.oid = c.relnamespace
                     where n.nspname = $1 and t.tgname = $2 and not t.tgisinternal limit 1",
                    &[&schema, &name],
                )?;
                Ok(format!("{};\n", row.get::<_, String>(0)))
            }
            ObjectKind::Sequence => {
                let r = self.query_one(
                    "select data_type::text, increment_by, min_value, max_value, start_value, cache_size, cycle from pg_sequences where schemaname = $1 and sequencename = $2",
                    &[&schema, &name],
                )?;
                let (ty, increment, min, max, start, cache, cycle): (String, i64, i64, i64, i64, i64, bool) =
                    (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5), r.get(6));
                let cycle = if cycle { "CYCLE" } else { "NO CYCLE" };
                Ok(format!("CREATE SEQUENCE {target}\n    AS {ty}\n    INCREMENT BY {increment}\n    MINVALUE {min}\n    MAXVALUE {max}\n    START WITH {start}\n    CACHE {cache}\n    {cycle};\n"))
            }
            _ => self.table_ddl(schema, name),
        }
    }

    /// Runs the file's statements in one transaction (PostgreSQL's DDL is
    /// transactional): all of them, or none and the first error.
    fn save_object(&mut self, _object: &ObjectFile, text: &str) -> Result<Vec<FileProblem>, String> {
        let statements: Vec<_> = script::split(text, Dialect::Postgres).into_iter().filter(|s| s.kind != script::Kind::Client).collect();
        // Inside a transaction the user began, behind a savepoint (which
        // PostgreSQL refuses outside one); else in a transaction of its own.
        let nested = match self.batch("SAVEPOINT kuana_save") {
            Ok(()) => true,
            Err(err) if err.code() == Some(&tokio_postgres::error::SqlState::NO_ACTIVE_SQL_TRANSACTION) => {
                self.batch("BEGIN").map_err(|e| message(&e))?;
                false
            }
            Err(err) => return Err(message(&err)),
        };
        for statement in &statements {
            let sql = statement.sql(text);
            if let Err(err) = self.run(&sql) {
                let _ = self.batch(if nested { "ROLLBACK TO SAVEPOINT kuana_save" } else { "ROLLBACK" });
                if err.as_db_error().is_none() {
                    return Err(message(&err));
                }
                let lead = text[statement.range.clone()].len() - text[statement.range.clone()].trim_start().len();
                let offset = statement.range.start + lead + position(&err).map_or(0, |at| char_to_byte(&sql, at));
                return Ok(vec![FileProblem { offset, message: message(&err), warning: false }]);
            }
        }
        self.batch(if nested { "RELEASE SAVEPOINT kuana_save" } else { "COMMIT" }).map_err(|e| message(&e))?;
        Ok(Vec::new())
    }

    fn execute(&mut self, sql: &str) -> Executed {
        let started = Instant::now();
        let outcome = match self.run(sql) {
            Ok((Some(mut grid), _)) => {
                grid.guess_numeric();
                Outcome::Rows(grid)
            }
            Ok((None, rows)) if is_dml(sql) => Outcome::Changed { rows, pending: false },
            Ok((None, _)) => Outcome::Done,
            Err(err) => Outcome::Failed { message: message(&err), offset: position(&err) },
        };
        Executed { outcome, output: self.take_notices(), elapsed: started.elapsed() }
    }

    fn commit(&mut self) -> Result<(), String> {
        self.batch("COMMIT").map_err(|e| message(&e))
    }

    fn rollback(&mut self) -> Result<(), String> {
        self.batch("ROLLBACK").map_err(|e| message(&e))
    }
}

impl Postgres {
    /// A table's DDL as PostgreSQL has none built in: its columns (types,
    /// defaults, identity, NOT NULL), constraints, then its other indexes.
    fn table_ddl(&mut self, schema: &str, name: &str) -> Result<String, String> {
        let target = Self::regclass(schema, name);
        let columns = self.query(
            "select a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull, pg_get_expr(d.adbin, d.adrelid), a.attidentity::text
             from pg_attribute a left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
             where a.attrelid = $1::text::regclass and a.attnum > 0 and not a.attisdropped order by a.attnum",
            &[&target],
        )?;
        let mut lines: Vec<String> = columns
            .iter()
            .map(|r| {
                let mut line = format!("    {} {}", quote(&r.get::<_, String>(0)), r.get::<_, String>(1));
                match r.get::<_, String>(4).as_str() {
                    "a" => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
                    "d" => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                    _ => {
                        if let Some(default) = r.get::<_, Option<String>>(3) {
                            line.push_str(&format!(" DEFAULT {default}"));
                        }
                    }
                }
                if r.get::<_, bool>(2) {
                    line.push_str(" NOT NULL");
                }
                line
            })
            .collect();
        let constraints = self.query(
            "select conname::text, pg_get_constraintdef(oid, true) from pg_constraint where conrelid = $1::text::regclass and contype <> 'n'
             order by case contype when 'p' then 0 when 'u' then 1 when 'f' then 2 else 3 end, conname",
            &[&target],
        )?;
        lines.extend(constraints.iter().map(|r| format!("    CONSTRAINT {} {}", quote(&r.get::<_, String>(0)), r.get::<_, String>(1))));
        let mut ddl = format!("CREATE TABLE {target} (\n{}\n);\n", lines.join(",\n"));
        let indexes = self.query(
            "select pg_get_indexdef(i.indexrelid) from pg_index i where i.indrelid = $1::text::regclass
             and not exists (select 1 from pg_constraint k where k.conindid = i.indexrelid) order by 1",
            &[&target],
        )?;
        for index in indexes {
            ddl.push_str(&format!("\n{};\n", index.get::<_, String>(0)));
        }
        Ok(ddl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real server: `KUANA_PG_TEST=user:password@host:port/database
    /// cargo test postgres_end_to_end -- --ignored` (e.g. a postgres container).
    #[test]
    #[ignore]
    fn postgres_end_to_end() {
        let spec = std::env::var("KUANA_PG_TEST").expect("KUANA_PG_TEST=user:password@host:port/database");
        let (login, place) = spec.split_once('@').unwrap();
        let (user, password) = login.split_once(':').unwrap();
        let (address, database) = place.split_once('/').unwrap();
        let (host, port) = address.split_once(':').unwrap();
        let config = ConnectionConfig {
            kind: super::super::DbKind::Postgres,
            host: host.into(),
            port: port.parse().unwrap(),
            database: database.into(),
            user: user.into(),
            ssl: SslMode::Prefer,
            ..Default::default()
        };
        let Connected { mut backend, schema, label, version } = connect(&config, password).unwrap();
        let db = backend.as_mut();
        assert_eq!((schema.as_str(), label.as_str()), ("public", database));
        assert!(version.starts_with("PostgreSQL "), "{version}");

        db.execute("drop schema if exists kuana_t cascade");
        assert!(matches!(db.execute("create schema kuana_t").outcome, Outcome::Done));
        let create = "create table kuana_t.items (id integer generated always as identity primary key, name text not null default 'x', price numeric(8,2), born date, tags text[])";
        assert!(matches!(db.execute(create).outcome, Outcome::Done), "{:?}", db.execute("select 1").outcome);
        assert!(matches!(db.execute("insert into kuana_t.items (name, price, born, tags) values ('Ada', 12.5, '1815-12-10', '{a,b}'), ('Bob', null, null, null)").outcome, Outcome::Changed { rows: 2, pending: false }));
        let Outcome::Rows(grid) = db.execute("select id, name, price, born, tags from kuana_t.items order by id").outcome else { panic!() };
        assert_eq!(grid.columns, ["id", "name", "price", "born", "tags"]);
        assert_eq!(grid.rows[0], [Some("1".into()), Some("Ada".into()), Some("12.50".into()), Some("1815-12-10".into()), Some("{a,b}".into())]);
        assert_eq!(grid.rows[1][2], None);
        assert_eq!(grid.numeric, [true, false, true, false, false]);

        // A long result stops at MAX_ROWS and the connection carries on.
        let Outcome::Rows(big) = db.execute("select g from generate_series(1, 500000) g").outcome else { panic!() };
        assert!(big.truncated && big.rows.len() == MAX_ROWS);
        assert!(matches!(db.execute("select 1").outcome, Outcome::Rows(_)));

        // An error's position, and notices.
        let Outcome::Failed { message, offset } = db.execute("select nosuch from kuana_t.items").outcome else { panic!() };
        assert!(message.contains("nosuch"), "{message}");
        assert_eq!(offset, Some(7));
        let done = db.execute("do $$ begin raise notice 'hello %', 1; end $$");
        assert!(matches!(done.outcome, Outcome::Done));
        assert_eq!(done.output, ["hello 1"]);

        // The catalog.
        assert!(db.schemas().unwrap().contains(&"kuana_t".to_string()));
        db.execute("create view kuana_t.cheap as select name from kuana_t.items where price < 20");
        db.execute("create function kuana_t.twice(x int) returns int language sql as $$ select x * 2 $$");
        db.execute("create function kuana_t.twice(x text) returns text language sql as $$ select x || x $$");
        db.execute("create procedure kuana_t.touch() language plpgsql as $$ begin update kuana_t.items set name = name; end $$");
        let objects = db.objects("kuana_t").unwrap();
        let has = |name: &str, kind| objects.iter().filter(|o| o.name == name && o.kind == kind).count();
        assert_eq!(has("items", ObjectKind::Table), 1);
        assert_eq!(has("cheap", ObjectKind::View), 1);
        assert_eq!(has("twice", ObjectKind::Function), 1, "{objects:?}");
        assert_eq!(has("touch", ObjectKind::Procedure), 1);
        assert_eq!(has("items_id_seq", ObjectKind::Sequence), 1);
        let cols = db.columns("kuana_t", "items").unwrap();
        assert_eq!(cols[0], ColumnInfo { name: "id".into(), data_type: "integer".into(), nullable: false, primary: true });
        assert_eq!(cols[2].data_type, "numeric(8,2)");

        // Sources, and saving them back.
        let ddl = db.source("kuana_t", "items", ObjectKind::Table, false).unwrap();
        assert!(ddl.contains("GENERATED ALWAYS AS IDENTITY") && ddl.contains("PRIMARY KEY"), "{ddl}");
        let view = db.source("kuana_t", "cheap", ObjectKind::View, false).unwrap();
        assert!(view.starts_with("CREATE OR REPLACE VIEW \"kuana_t\".\"cheap\" AS"), "{view}");
        let file = ObjectFile { connection: "c".into(), owner: "kuana_t".into(), name: "cheap".into(), kind: ObjectKind::View, body: false };
        assert!(db.save_object(&file, &view.replace("< 20", "< 30")).unwrap().is_empty());
        assert!(db.source("kuana_t", "cheap", ObjectKind::View, false).unwrap().contains("< 30"));
        let functions = db.source("kuana_t", "twice", ObjectKind::Function, false).unwrap();
        assert_eq!(script::split(&functions, Dialect::Postgres).len(), 2, "{functions}");
        let file = ObjectFile { connection: "c".into(), owner: "kuana_t".into(), name: "twice".into(), kind: ObjectKind::Function, body: false };
        let broken = functions.replacen("x * 2", "x * nosuch", 1);
        let problems = db.save_object(&file, &broken).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        // Nothing changed: the transaction was rolled back.
        assert!(matches!(db.execute("select kuana_t.twice(2)").outcome, Outcome::Rows(ref g) if g.rows[0][0].as_deref() == Some("4")));
        assert!(db.source("kuana_t", "items_id_seq", ObjectKind::Sequence, false).unwrap().starts_with("CREATE SEQUENCE"));

        db.execute("drop schema kuana_t cascade");
    }
}

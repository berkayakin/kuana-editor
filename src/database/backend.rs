//! What every database the Database view connects to provides (`Backend`),
//! the results they share (`Executed`, `Grid`…), and the live connection
//! (`Session`): the drivers block, so each connection is a thread of its own
//! that owns its `Backend` and runs jobs in order; the UI awaits their results.

use std::{sync::mpsc, time::Duration};

use futures::channel::oneshot;

use super::{ColumnInfo, ConnectionConfig, DbKind, DbObject, ObjectFile, ObjectKind, Subprogram};

/// How many rows a query result keeps.
pub const MAX_ROWS: usize = 1000;
/// How long connecting may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// A connection to one database, owned by its session's thread.
pub trait Backend: Send {
    /// The schemas (Oracle's users, MySQL's databases) it can see.
    fn schemas(&mut self) -> Result<Vec<String>, String>;
    /// The objects in `schema` that the Database view lists, by name.
    fn objects(&mut self, schema: &str) -> Result<Vec<DbObject>, String>;
    /// A table's or view's columns.
    fn columns(&mut self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>, String>;
    /// A package's procedures and functions (only Oracle has packages).
    fn subprograms(&mut self, _schema: &str, _package: &str) -> Result<Vec<Subprogram>, String> {
        Ok(Vec::new())
    }
    /// An object's source as the script that creates it (a body: Oracle's
    /// package and type bodies). Objects whose kind `DbKind::saves` come
    /// back as a script that replaces them when run.
    fn source(&mut self, schema: &str, name: &str, kind: ObjectKind, body: bool) -> Result<String, String>;
    /// Runs an object's edited source file (an explicit Save): the problems
    /// it has, by byte offset in `text`, or an error that isn't about the text.
    fn save_object(&mut self, object: &ObjectFile, text: &str) -> Result<Vec<FileProblem>, String>;
    /// Compiles an object again as it is in the database (Oracle's ALTER …
    /// COMPILE): its compile errors.
    fn recompile(&mut self, _schema: &str, _name: &str, _kind: ObjectKind, _body: bool) -> Result<Vec<CompileError>, String> {
        Err("Only Oracle compiles objects".into())
    }
    /// Runs one statement (as `script::Statement::sql` gives it).
    fn execute(&mut self, sql: &str) -> Executed;
    fn commit(&mut self) -> Result<(), String>;
    fn rollback(&mut self) -> Result<(), String>;
}

/// A connection, just made: the backend and what the UI shows about it.
pub struct Connected {
    pub backend: Box<dyn Backend>,
    /// The schema it starts in (empty: none, as MySQL without a database).
    pub schema: String,
    /// What the Database view shows beside the connection's name.
    pub label: String,
    /// The server, as the user knows it ("PostgreSQL 17.2").
    pub version: String,
}

/// A problem in an object's file, after saving it ran it.
#[derive(Clone, Debug, PartialEq)]
pub struct FileProblem {
    /// Byte offset in the file.
    pub offset: usize,
    pub message: String,
    pub warning: bool,
}

/// A compile error (Oracle's ALL_ERRORS): 1-based line and column in the unit's source.
#[derive(Clone, Debug)]
pub struct CompileError {
    pub line: usize,
    pub column: usize,
    pub message: String,
    pub warning: bool,
}

/// What one statement did.
#[derive(Clone, Debug)]
pub enum Outcome {
    Rows(Grid),
    /// DML: rows changed; `pending` until a commit (Oracle; the others commit
    /// each statement unless a transaction was begun).
    Changed { rows: u64, pending: bool },
    /// DDL, PL/SQL, anything else.
    Done,
    /// A PL/SQL unit created with compile errors.
    Compiled(Vec<CompileError>),
    /// `offset`: characters into the statement where the server points.
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

impl Grid {
    /// For drivers that don't say which columns are numbers: those whose
    /// values all read as numbers (and that have some).
    pub fn guess_numeric(&mut self) {
        self.numeric = (0..self.columns.len())
            .map(|c| {
                let mut values = self.rows.iter().filter_map(|r| r.get(c).and_then(|v| v.as_deref())).peekable();
                values.peek().is_some() && values.all(|v| v.parse::<f64>().is_ok())
            })
            .collect();
    }
}

#[derive(Clone, Debug)]
pub struct Executed {
    pub outcome: Outcome,
    /// What the server said meanwhile: DBMS_OUTPUT, notices, warnings.
    pub output: Vec<String>,
    pub elapsed: Duration,
}

/// Whether a statement changes rows (its count is "rows changed").
pub fn is_dml(sql: &str) -> bool {
    let first = sql.trim_start().split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("").to_ascii_lowercase();
    matches!(first.as_str(), "insert" | "update" | "delete" | "merge" | "replace" | "upsert")
}

/// Characters into `text` → a byte offset (clamped to its end).
pub fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i)
}

type Job = Box<dyn FnOnce(&mut dyn Backend) + Send>;

/// A live connection: its thread runs one job at a time.
pub struct Session {
    jobs: mpsc::Sender<Job>,
    pub kind: DbKind,
    /// The schema it starts in (empty: none).
    pub schema: String,
    pub label: String,
    pub version: String,
}

impl Session {
    /// Connects on a new thread; resolves once connected (or not).
    pub fn connect(config: &ConnectionConfig, password: String) -> oneshot::Receiver<Result<Session, String>> {
        let (done, result) = oneshot::channel();
        let config = config.clone();
        let spawned = std::thread::Builder::new().name(format!("db-{}", config.name)).spawn(move || {
            let connected = match config.kind {
                DbKind::Oracle => super::oracle::connect(&config, &password),
                DbKind::Postgres => super::postgres::connect(&config, &password),
                DbKind::Mysql => super::mysql::connect(&config, &password),
                DbKind::Sqlite => super::sqlite::connect(&config),
            };
            let Connected { mut backend, schema, label, version } = match connected {
                Ok(connected) => connected,
                Err(err) => {
                    let _ = done.send(Err(err));
                    return;
                }
            };
            let (jobs, queue) = mpsc::channel::<Job>();
            if done.send(Ok(Session { jobs, kind: config.kind, schema, label, version })).is_err() {
                return;
            }
            // Until every handle is dropped (disconnect, quit); the backend
            // closes its connection when dropped.
            for job in queue {
                job(backend.as_mut());
            }
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
        F: FnOnce(&mut dyn Backend) -> Result<T, String> + Send + 'static,
    {
        let (done, result) = oneshot::channel();
        let sent = self.jobs.send(Box::new(move |backend| {
            let _ = done.send(job(backend));
        }));
        async move {
            if sent.is_err() {
                return Err("The connection was closed".to_string());
            }
            result.await.unwrap_or_else(|_| Err("The connection was closed".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_dml() {
        let mut grid = Grid {
            columns: vec!["a".into(), "b".into(), "c".into()],
            rows: vec![vec![Some("1".into()), Some("x".into()), None], vec![Some("2.5".into()), Some("3".into()), None]],
            ..Default::default()
        };
        grid.guess_numeric();
        assert_eq!(grid.numeric, [true, false, false]);
        assert!(is_dml("  UPDATE t set a = 1") && is_dml("insert into t") && !is_dml("select 1") && !is_dml("updated"));
        assert_eq!(char_to_byte("aé b", 3), 4);
    }
}

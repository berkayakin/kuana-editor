//! SQLite through rusqlite, with SQLite compiled in: a database is a file
//! (made if it isn't there yet), with no server, user or password.

use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, types::ValueRef};

use super::{
    ColumnInfo, ConnectionConfig, DbObject, ObjectFile, ObjectKind,
    backend::{Backend, Connected, Executed, FileProblem, Grid, MAX_ROWS, Outcome, char_to_byte, is_dml},
    script::{self, Dialect},
};

/// Connects (blocking, on the session's thread).
pub fn connect(config: &ConnectionConfig) -> Result<Connected, String> {
    let path = config.sqlite_path();
    if config.database.trim().is_empty() {
        return Err("Choose the database file".into());
    }
    if path.parent().is_some_and(|dir| !dir.as_os_str().is_empty() && !dir.is_dir()) {
        return Err(format!("{} isn't a folder", path.parent().unwrap_or(&path).display()));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&path, flags).map_err(|e| e.to_string())?;
    // Another program writing it waits a little instead of failing at once.
    let _ = conn.busy_timeout(Duration::from_secs(5));
    // Reading something tells a file that isn't a database from one that is.
    let version: String = conn.query_row("select sqlite_version() from sqlite_master limit 1", [], |r| r.get(0)).or_else(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => conn.query_row("select sqlite_version()", [], |r| r.get(0)),
        other => Err(other),
    })
    .map_err(|e| e.to_string())?;
    let label = path.file_name().map_or_else(|| config.database.clone(), |n| n.to_string_lossy().into_owned());
    Ok(Connected { backend: Box::new(Sqlite { conn }), schema: "main".into(), label, version: format!("SQLite {version}") })
}

struct Sqlite {
    conn: Connection,
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// An error, and where in the statement it is (characters), when SQLite says.
fn failure(sql: &str, err: &rusqlite::Error) -> (String, Option<usize>) {
    match err {
        rusqlite::Error::SqlInputError { msg, offset, .. } => {
            let at = usize::try_from(*offset).ok().filter(|&o| o <= sql.len() && sql.is_char_boundary(o));
            (msg.clone(), at.map(|o| sql[..o].chars().count()))
        }
        rusqlite::Error::SqliteFailure(_, Some(msg)) => (msg.clone(), None),
        other => (other.to_string(), None),
    }
}

/// A value as the grid shows it.
fn text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => {
            let mut hex: String = b.iter().take(256).map(|b| format!("{b:02X}")).collect();
            if b.len() > 256 {
                hex.push('…');
            }
            Some(format!("x'{hex}'"))
        }
    }
}

impl Sqlite {
    fn run(&mut self, sql: &str) -> rusqlite::Result<Outcome> {
        let mut statement = self.conn.prepare(sql)?;
        let count = statement.column_count();
        if count == 0 {
            let rows = statement.raw_execute()?;
            return Ok(if is_dml(sql) { Outcome::Changed { rows: rows as u64, pending: false } } else { Outcome::Done });
        }
        let mut grid = Grid {
            columns: statement.column_names().map(str::to_string).collect(),
            types: statement.columns().map(|c| c.decl_type().unwrap_or("").to_lowercase()).collect(),
            ..Default::default()
        };
        // Numbers by what the column says it holds, else by its values.
        let declared: Vec<Option<bool>> = grid
            .types
            .iter()
            .map(|t| (!t.is_empty()).then(|| ["int", "real", "num", "dec", "float", "doub"].iter().any(|n| t.contains(n))))
            .collect();
        let mut seen = vec![None::<bool>; count];
        let mut rows = statement.raw_query();
        while let Some(row) = rows.next()? {
            if grid.rows.len() == MAX_ROWS {
                grid.truncated = true;
                break;
            }
            let mut cells = Vec::with_capacity(count);
            for (i, seen) in seen.iter_mut().enumerate() {
                let value = row.get_ref(i)?;
                let number = matches!(value, ValueRef::Integer(_) | ValueRef::Real(_));
                if !matches!(value, ValueRef::Null) {
                    *seen = Some(seen.unwrap_or(true) && number);
                }
                cells.push(text(value));
            }
            grid.rows.push(cells);
        }
        grid.numeric = declared.iter().zip(&seen).map(|(d, s)| d.unwrap_or(false) || s.unwrap_or(false)).collect();
        Ok(Outcome::Rows(grid))
    }
}

impl Backend for Sqlite {
    fn schemas(&mut self) -> Result<Vec<String>, String> {
        let mut statement = self.conn.prepare("select name from pragma_database_list where name <> 'temp' order by seq").map_err(|e| e.to_string())?;
        let names = statement.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
        names.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    fn objects(&mut self, schema: &str) -> Result<Vec<DbObject>, String> {
        let sql = format!("select name, type from {}.sqlite_master where type in ('table', 'view', 'trigger') and name not like 'sqlite_%' order by name", quote(schema));
        let mut statement = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (name, kind) = row.map_err(|e| e.to_string())?;
            let kind = match kind.as_str() {
                "view" => ObjectKind::View,
                "trigger" => ObjectKind::Trigger,
                _ => ObjectKind::Table,
            };
            out.push(DbObject { name, kind, valid: true, has_body: false, body_valid: true });
        }
        Ok(out)
    }

    fn columns(&mut self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>, String> {
        let sql = "select name, type, \"notnull\", pk from pragma_table_info(?1, ?2) order by cid";
        let mut statement = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([table, schema], |r| {
                Ok(ColumnInfo { name: r.get(0)?, data_type: r.get(1)?, nullable: r.get::<_, i64>(2)? == 0, primary: r.get::<_, i64>(3)? > 0 })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    fn source(&mut self, schema: &str, name: &str, _kind: ObjectKind, _body: bool) -> Result<String, String> {
        let master = format!("{}.sqlite_master", quote(schema));
        let sql: Option<String> = self
            .conn
            .query_row(&format!("select sql from {master} where name = ?1"), [name], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let mut text = format!("{};\n", sql.unwrap_or_default().trim_end().trim_end_matches(';'));
        // A table's own indexes after it.
        let mut statement = self
            .conn
            .prepare(&format!("select sql from {master} where type = 'index' and tbl_name = ?1 and sql is not null order by name"))
            .map_err(|e| e.to_string())?;
        let indexes = statement.query_map([name], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
        for index in indexes.flatten() {
            text.push_str(&format!("\n{};\n", index.trim_end_matches(';')));
        }
        Ok(text)
    }

    /// Drops and creates the view or trigger in one transaction (SQLite's DDL
    /// is transactional): the new one, or the old one and the error.
    fn save_object(&mut self, object: &ObjectFile, text: &str) -> Result<Vec<FileProblem>, String> {
        let what = if object.kind == ObjectKind::Trigger { "TRIGGER" } else { "VIEW" };
        let in_transaction = !self.conn.is_autocommit();
        let (begin, undo, done) =
            if in_transaction { ("SAVEPOINT kuana_save", "ROLLBACK TO kuana_save", "RELEASE kuana_save") } else { ("BEGIN", "ROLLBACK", "COMMIT") };
        self.conn.execute_batch(begin).map_err(|e| e.to_string())?;
        let drop = format!("DROP {what} IF EXISTS {}.{}", quote(&object.owner), quote(&object.name));
        if let Err(err) = self.conn.execute_batch(&drop) {
            let _ = self.conn.execute_batch(undo);
            return Err(err.to_string());
        }
        for statement in script::split(text, Dialect::Sqlite).into_iter().filter(|s| s.kind != script::Kind::Client) {
            let sql = statement.sql(text);
            if let Err(err) = self.conn.execute_batch(&sql) {
                let _ = self.conn.execute_batch(undo);
                let (message, at) = failure(&sql, &err);
                let lead = text[statement.range.clone()].len() - text[statement.range.clone()].trim_start().len();
                let offset = statement.range.start + lead + at.map_or(0, |a| char_to_byte(&sql, a));
                return Ok(vec![FileProblem { offset, message, warning: false }]);
            }
        }
        self.conn.execute_batch(done).map_err(|e| e.to_string())?;
        Ok(Vec::new())
    }

    fn execute(&mut self, sql: &str) -> Executed {
        let started = Instant::now();
        let outcome = self.run(sql).unwrap_or_else(|err| {
            let (message, offset) = failure(sql, &err);
            Outcome::Failed { message, offset }
        });
        Executed { outcome, output: Vec::new(), elapsed: started.elapsed() }
    }

    fn commit(&mut self) -> Result<(), String> {
        self.conn.execute_batch("COMMIT").map_err(|e| e.to_string())
    }

    fn rollback(&mut self) -> Result<(), String> {
        self.conn.execute_batch("ROLLBACK").map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No server needed: a database file in a temporary folder.
    #[test]
    fn sqlite_end_to_end() {
        let dir = std::env::temp_dir().join(format!("kuana-sqlite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("shop.db");
        let config = ConnectionConfig { kind: super::super::DbKind::Sqlite, database: file.display().to_string(), ..Default::default() };
        let Connected { mut backend, schema, label, version } = connect(&config).unwrap();
        let db = backend.as_mut();
        assert_eq!((schema.as_str(), label.as_str()), ("main", "shop.db"));
        assert!(version.starts_with("SQLite 3."), "{version}");

        assert!(matches!(db.execute("create table items (id integer primary key, name text not null, price real, data blob)").outcome, Outcome::Done));
        db.execute("create index items_name on items (name)");
        let inserted = db.execute("insert into items (name, price, data) values ('Ada', 12.5, x'CAFE'), ('Bob', null, null)");
        assert!(matches!(inserted.outcome, Outcome::Changed { rows: 2, pending: false }), "{:?}", inserted.outcome);
        let Outcome::Rows(grid) = db.execute("select id, name, price, data from items order by id").outcome else { panic!() };
        assert_eq!(grid.columns, ["id", "name", "price", "data"]);
        assert_eq!(grid.rows[0], [Some("1".into()), Some("Ada".into()), Some("12.5".into()), Some("x'CAFE'".into())]);
        assert_eq!(grid.rows[1][2], None);
        assert_eq!(grid.numeric, [true, false, true, false]);
        let Outcome::Rows(big) = db.execute("with recursive n(i) as (select 1 union all select i + 1 from n limit 5000) select i from n").outcome else { panic!() };
        assert!(big.truncated && big.rows.len() == MAX_ROWS);

        let Outcome::Failed { message, offset } = db.execute("select nosuch from items").outcome else { panic!() };
        assert!(message.contains("nosuch"), "{message}");
        assert_eq!(offset, Some(7));

        db.execute("create view cheap as select name from items where price < 20");
        db.execute("create trigger stamp after insert on items begin update items set name = upper(new.name) where id = new.id; end");
        let objects = db.objects("main").unwrap();
        let kinds: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
        assert_eq!(kinds, [("cheap", ObjectKind::View), ("items", ObjectKind::Table), ("stamp", ObjectKind::Trigger)]);
        assert_eq!(db.columns("main", "items").unwrap()[0], ColumnInfo { name: "id".into(), data_type: "INTEGER".into(), nullable: true, primary: true });
        assert!(db.source("main", "items", ObjectKind::Table, false).unwrap().contains("CREATE INDEX items_name"));

        // Saving a trigger's file: its BEGIN … END body is one statement.
        let trigger = db.source("main", "stamp", ObjectKind::Trigger, false).unwrap();
        let object = ObjectFile { connection: "c".into(), owner: "main".into(), name: "stamp".into(), kind: ObjectKind::Trigger, body: false };
        assert!(db.save_object(&object, &trigger.replace("upper", "lower")).unwrap().is_empty());
        db.execute("insert into items (name) values ('Cy')");
        let Outcome::Rows(names) = db.execute("select name from items where id = 3").outcome else { panic!() };
        assert_eq!(names.rows[0][0].as_deref(), Some("cy"));
        // A broken one leaves the old one there.
        let problems = db.save_object(&object, &trigger.replace("update items", "updat items")).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(db.source("main", "stamp", ObjectKind::Trigger, false).unwrap().contains("lower"));
        let view = db.source("main", "cheap", ObjectKind::View, false).unwrap();
        let object = ObjectFile { connection: "c".into(), owner: "main".into(), name: "cheap".into(), kind: ObjectKind::View, body: false };
        assert!(db.save_object(&object, &view.replace("< 20", "< 30")).unwrap().is_empty());
        assert!(db.source("main", "cheap", ObjectKind::View, false).unwrap().contains("< 30"));

        drop(backend);
        std::fs::remove_dir_all(&dir).ok();
    }
}

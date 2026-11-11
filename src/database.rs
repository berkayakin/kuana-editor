//! The Database view's data: saved connections (`<config>/database/connections.json`,
//! passwords in the system keychain), the kinds of objects it lists, and the
//! local files an object's source opens in.
//!
//! Opening a package, procedure, view… writes its source to
//! `<config>/database/objects/<connection>/<OWNER>/<NAME>.<ext>` and opens
//! that file like any other, so editing, undo, Local History and the PL/SQL
//! extension's highlighting just work. Saving such a file (an explicit Save)
//! runs it against its connection: `CREATE OR REPLACE …`, then its compile
//! errors become the file's problems (`workspace/database.rs`).

pub mod dialog;
pub mod model;
pub mod oracle;
pub mod results;
pub mod script;
pub mod view;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How the user connects (Oracle's administrative roles).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Default,
    Sysdba,
    Sysoper,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    /// Stable: names the keychain entry and the objects' folder.
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    /// A service name, or a SID when `use_sid`.
    pub service: String,
    pub use_sid: bool,
    /// A connect string used instead of host/port/service: EZConnect, a TNS
    /// descriptor or a tnsnames.ora alias.
    pub connect_string: String,
    pub user: String,
    pub role: Role,
    /// The password is kept in the system's keychain.
    pub save_password: bool,
}

impl ConnectionConfig {
    /// Where it connects, as the view shows it.
    pub fn address(&self) -> String {
        if !self.connect_string.trim().is_empty() {
            let text = self.connect_string.trim();
            return if text.len() > 48 { format!("{}…", &text[..text.floor_char_boundary(47)]) } else { text.to_string() };
        }
        let port = if self.port == 0 { 1521 } else { self.port };
        format!("{}:{port}/{}", self.host.trim(), self.service.trim())
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    connections: Vec<ConnectionConfig>,
}

pub fn folder() -> Option<PathBuf> {
    crate::theme::config_dir().map(|d| d.join("database"))
}

/// The saved connections (none if the file is missing or doesn't read).
pub fn load_connections() -> Vec<ConnectionConfig> {
    let Some(path) = folder().map(|d| d.join("connections.json")) else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    match crate::state::read_lenient::<Saved>(&text) {
        Ok(saved) => saved.connections,
        Err(err) => {
            crate::state::set_aside(&path, "database connections", err);
            Vec::new()
        }
    }
}

pub fn save_connections(connections: &[ConnectionConfig]) -> std::io::Result<()> {
    let Some(dir) = folder() else { return Ok(()) };
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_vec_pretty(&Saved { connections: connections.to_vec() }).map_err(std::io::Error::other)?;
    crate::state::write_atomic(&dir.join("connections.json"), &json)
}

/// A new connection's id: unique enough, and safe as a folder name.
pub fn new_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("c{nanos:x}")
}

// ---- Passwords --------------------------------------------------------------------

const KEYCHAIN_SERVICE: &str = "Kuana Database";

pub fn saved_password(id: &str) -> Option<String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, id).ok()?.get_password().ok()
}

pub fn save_password(id: &str, password: &str) -> Result<(), String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, id).and_then(|e| e.set_password(password)).map_err(|e| e.to_string())
}

pub fn forget_password(id: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, id) {
        let _ = entry.delete_credential();
    }
}

// ---- Objects ----------------------------------------------------------------------

/// The kinds of objects the view groups a schema's objects by, in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Package,
    Procedure,
    Function,
    Trigger,
    Type,
    Sequence,
    Synonym,
}

impl ObjectKind {
    pub const ALL: [ObjectKind; 10] = [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Package,
        ObjectKind::Procedure,
        ObjectKind::Function,
        ObjectKind::Trigger,
        ObjectKind::Type,
        ObjectKind::Sequence,
        ObjectKind::Synonym,
    ];

    /// ALL_OBJECTS.OBJECT_TYPE.
    pub fn object_type(self) -> &'static str {
        match self {
            ObjectKind::Table => "TABLE",
            ObjectKind::View => "VIEW",
            ObjectKind::MaterializedView => "MATERIALIZED VIEW",
            ObjectKind::Package => "PACKAGE",
            ObjectKind::Procedure => "PROCEDURE",
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Trigger => "TRIGGER",
            ObjectKind::Type => "TYPE",
            ObjectKind::Sequence => "SEQUENCE",
            ObjectKind::Synonym => "SYNONYM",
        }
    }

    pub fn from_object_type(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.object_type() == text)
    }

    /// DBMS_METADATA's name for it.
    pub fn metadata_type(self) -> &'static str {
        match self {
            ObjectKind::MaterializedView => "MATERIALIZED_VIEW",
            other => other.object_type(),
        }
    }

    /// The group's label.
    pub fn plural(self) -> &'static str {
        match self {
            ObjectKind::Table => "Tables",
            ObjectKind::View => "Views",
            ObjectKind::MaterializedView => "Materialized Views",
            ObjectKind::Package => "Packages",
            ObjectKind::Procedure => "Procedures",
            ObjectKind::Function => "Functions",
            ObjectKind::Trigger => "Triggers",
            ObjectKind::Type => "Types",
            ObjectKind::Sequence => "Sequences",
            ObjectKind::Synonym => "Synonyms",
        }
    }

    /// ALL_SOURCE.TYPE for PL/SQL units (their source is code that compiles);
    /// None for objects whose source is DDL.
    pub fn source_type(self, body: bool) -> Option<&'static str> {
        Some(match (self, body) {
            (ObjectKind::Package, false) => "PACKAGE",
            (ObjectKind::Package, true) => "PACKAGE BODY",
            (ObjectKind::Type, false) => "TYPE",
            (ObjectKind::Type, true) => "TYPE BODY",
            (ObjectKind::Procedure, _) => "PROCEDURE",
            (ObjectKind::Function, _) => "FUNCTION",
            (ObjectKind::Trigger, _) => "TRIGGER",
            _ => return None,
        })
    }

    /// Whether saving its source runs it (PL/SQL units and views, whose DDL
    /// is CREATE OR REPLACE); tables and the rest open read as DDL only.
    pub fn compiles(self) -> bool {
        self.source_type(false).is_some() || self == ObjectKind::View
    }

    /// The file suffix its source opens with (Oracle's usual ones).
    pub fn suffix(self, body: bool) -> &'static str {
        match (self, body) {
            (ObjectKind::Package, false) => "pks",
            (ObjectKind::Package, true) => "pkb",
            (ObjectKind::Type, false) => "tps",
            (ObjectKind::Type, true) => "tpb",
            (ObjectKind::Procedure, _) => "prc",
            (ObjectKind::Function, _) => "fnc",
            (ObjectKind::Trigger, _) => "trg",
            (ObjectKind::View, _) => "vw",
            _ => "sql",
        }
    }

    pub fn icon(self) -> crate::assets::Icon {
        use crate::assets::Icon;
        match self {
            ObjectKind::Table => Icon::Table,
            ObjectKind::View | ObjectKind::MaterializedView => Icon::Eye,
            ObjectKind::Package => Icon::Package,
            ObjectKind::Procedure | ObjectKind::Function => Icon::Function,
            ObjectKind::Trigger => Icon::Bolt,
            ObjectKind::Type => Icon::Braces,
            ObjectKind::Sequence => Icon::Hash,
            ObjectKind::Synonym => Icon::Link,
        }
    }

    /// Whether the object has columns to list.
    pub fn has_columns(self) -> bool {
        matches!(self, ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DbObject {
    pub name: String,
    pub kind: ObjectKind,
    pub valid: bool,
    /// A package or type with a body.
    pub has_body: bool,
    pub body_valid: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Subprogram {
    pub name: String,
    pub function: bool,
}

/// An object whose source is open in a file (`object_path`).
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectFile {
    pub connection: String,
    pub owner: String,
    pub name: String,
    pub kind: ObjectKind,
    pub body: bool,
}

impl ObjectFile {
    /// ALL_SOURCE / ALL_ERRORS type.
    pub fn source_type(&self) -> &'static str {
        self.kind.source_type(self.body).unwrap_or(self.kind.object_type())
    }
}

/// Where an object's source opens. Names are kept as Oracle stores them
/// (uppercase, usually); characters a file name can't hold are escaped.
pub fn object_path(object: &ObjectFile) -> Option<PathBuf> {
    let kind = object.kind.object_type().to_lowercase().replace(' ', "_");
    let file = format!("{}.{kind}.{}", escape(&object.name), object.kind.suffix(object.body));
    Some(folder()?.join("objects").join(&object.connection).join(escape(&object.owner)).join(file))
}

/// The object an `object_path` file holds.
pub fn object_of(path: &Path) -> Option<ObjectFile> {
    let objects = folder()?.join("objects");
    let rel = path.strip_prefix(&objects).ok()?;
    let mut parts = rel.iter().map(|p| p.to_string_lossy().into_owned());
    let connection = parts.next()?;
    let owner = unescape(&parts.next()?);
    let file = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let mut pieces = file.rsplitn(3, '.');
    let suffix = pieces.next()?;
    let kind = pieces.next()?.replace('_', " ").to_uppercase();
    let name = unescape(pieces.next()?);
    let kind = ObjectKind::from_object_type(&kind)?;
    let body = matches!(suffix, "pkb" | "tpb");
    Some(ObjectFile { connection, owner, name, kind, body })
}

/// A new SQL script's file for a connection (`scripts/<connection>/`).
pub fn new_script_path(connection: &str) -> Option<PathBuf> {
    let dir = folder()?.join("scripts").join(connection);
    let _ = std::fs::create_dir_all(&dir);
    (1..10_000).map(|n| dir.join(format!("script{n}.sql"))).find(|p| !p.exists())
}

/// The connection a script in `scripts/<connection>/` belongs to.
pub fn script_connection(path: &Path) -> Option<String> {
    let rel = path.strip_prefix(folder()?.join("scripts")).ok()?;
    Some(rel.iter().next()?.to_string_lossy().into_owned())
}

/// Characters other than letters, digits, `_`, `$`, `#` and `-` as `%XX`.
fn escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || matches!(c, '_' | '$' | '#' | '-') {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = text.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a file is SQL that ⌘↩ / Ctrl+Enter runs against a connection.
pub fn is_sql_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "sql" | "pls" | "plb" | "pks" | "pkb" | "pkg" | "pck" | "spc" | "bdy" | "prc" | "fnc" | "fct" | "trg" | "tps" | "tpb" | "typ" | "vw" | "ddl"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_paths_round_trip() {
        let Some(_) = folder() else { return };
        for (name, kind, body) in [("ORDER_API", ObjectKind::Package, true), ("My Table/x", ObjectKind::Table, false), ("V$X", ObjectKind::MaterializedView, false)] {
            let object = ObjectFile { connection: "c1".into(), owner: "HR".into(), name: name.into(), kind, body };
            let path = object_path(&object).unwrap();
            assert_eq!(object_of(&path), Some(object), "{}", path.display());
        }
    }

    #[test]
    fn sql_files() {
        assert!(is_sql_path(Path::new("a/b.PKB")));
        assert!(is_sql_path(Path::new("x.sql")));
        assert!(!is_sql_path(Path::new("x.rs")));
    }
}

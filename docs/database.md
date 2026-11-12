# Databases

The **Database** view (the cylinder on the activity bar, under Files and Commit) connects to Oracle
Database. You can browse a schema's tables, views, packages, procedures, functions, triggers, types,
sequences and synonyms. You can edit and compile PL/SQL, and run SQL from any `.sql` file. Kuana
connects with Oracle's own pure-Rust driver, so you don't need Oracle Client or Instant Client.
It works with Oracle Database 12c and newer, on premises or in the cloud.

For highlighting, outline and folding of SQL and PL/SQL, install the **Oracle SQL and PL/SQL**
extension from **Settings → Extensions → Available**.

## Connections

Click **+** in the view (or run **New Database Connection…** from Find Action) and fill in:

- **Host**, **Port** (1521 by default) and the **Service name**, or switch to **SID**.
- Or click **Use a connect string…** and enter one of these instead: EZConnect
  (`host:port/service`), a full TNS descriptor (`(DESCRIPTION=…)`), or an alias from
  `tnsnames.ora` (found through `TNS_ADMIN`).
- **User**, **Password** and **Role** (Default, SYSDBA or SYSOPER).
- **Save password** keeps the password in the system keychain: Keychain on macOS, Credential
  Manager on Windows. On Linux it uses the kernel keyring, which forgets it on restart. With
  the box unticked, Kuana asks for the password each time you connect.

**Test Connection** connects once and shows the database version. **Connect** saves the connection and
connects. Connections are kept in `~/.config/kuana/database/connections.json`, without passwords.
Right-click a connection to **Connect** or **Disconnect**, **Refresh**, open a **New SQL Script**,
**Edit Connection…** or **Delete Connection…**.

## Browsing

Once connected, the connection lists your own schema's objects by kind, each group with its count.
Invalid objects are shown in red. **Other Schemas** lists every schema you can see. The filter field
narrows every loaded list by name. Lists load when you first open them; **Refresh** reads them again.

- A **package**, **procedure**, **function**, **trigger**, **type** or **view** opens its source in a
  tab when you click it. Open a package to see its **Body** and its procedures and functions. Click
  one of those to jump to it in the body.
- A **table** opens to its columns, with their types; primary key columns have a key icon.
  Double-click a table for its DDL. Right-click it and choose **Select Rows** to see its first 1000 rows.
- Right-click any object for **Open Specification** / **Open Body**, **Compile** (`ALTER … COMPILE`)
  and **Copy Name**.

The keyboard works too: ↑/↓ to move, ←/→ to close and open, Enter to open, Escape to go back to the
editor.

## Editing and compiling

An object's source opens as a `CREATE OR REPLACE …` script in a file of its own, under
`~/.config/kuana/database/objects/`. Highlighting, undo, Local History, splits and Find all work
on it like any other file. An object from another schema has its owner in the name, so
compiling it replaces that schema's object.

**Save** (⌘S / Ctrl+S) compiles the file into its database. The result is shown in a notification.
Compile errors become the file's problems: underlined where Oracle points, and listed in Problems.
Auto-save only writes the local file; only an explicit Save compiles. Opening the object again
reads it from the database again, unless its tab has unsaved edits.

## Running SQL

In any `.sql` file (and `.pks`, `.pkb`, `.prc`, `.fnc`, `.trg` and the like), **⌘↩ / Ctrl+Enter**
runs the statement at the caret. With text selected, it runs the selection. **Run SQL Script** runs
the whole file. Both are also at the top of the editor's right-click menu and in Find Action.

Once you have a connection, each statement in a script gets a green **▶** in the gutter, on its
first line; click it to run that statement. Object files don't get one, since saving them compiles.

Statements end with `;`. PL/SQL blocks and `CREATE … PACKAGE` and
similar units end with a line holding only `/`, as in SQL*Plus and SQL Developer. SQL*Plus commands
such as `SET SERVEROUTPUT ON` and `PROMPT` are skipped, and `EXEC proc(…)` runs as a block.

The file's connection is shown in the status bar; click it to choose another. An object's file and a
**New SQL Script** keep their own connection. Any other file uses the one selected in the Database
view, else the only one connected, else your only connection. If that connection needs its
password, Kuana asks for it, then runs what you asked for.

Results show in the **Database** panel at the bottom:

- Each query gets a **Result** tab: a grid with up to 1000 rows that scrolls both ways. NULLs show as
  `(null)`. Click a cell and press ⌘C / Ctrl+C to copy it; the copy button copies every row as
  tab-separated text.
- **Output** lists every statement with what it did, how long it took, and any `DBMS_OUTPUT` lines.
  DBMS_OUTPUT is always on. Click a row to select its statement in the editor.
- An error is underlined in the file where Oracle points.
- Changes from INSERT, UPDATE, DELETE and MERGE aren't committed until you click **Commit**, or until
  **Rollback** undoes them. A script's own `COMMIT` works too.

## Not there yet

Other databases (PostgreSQL, MySQL, SQLite), editing table data in the grid, explain plans,
the PL/SQL debugger, and completion from the schema.

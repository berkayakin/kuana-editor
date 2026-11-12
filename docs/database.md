# Databases

The **Database** view (the cylinder on the activity bar, under Files and Commit) connects to
**PostgreSQL**, **MySQL**, **MariaDB**, **SQLite** and **Oracle Database**. You can browse tables,
views, functions, procedures, triggers and the rest, edit and save routines and views, and run SQL
from any `.sql` file. Kuana has the drivers built in, so you don't need a database client installed.

| Database | Versions | Browses | Saving an object runs it for |
| --- | --- | --- | --- |
| PostgreSQL | 11 and newer, and compatible servers | tables, views, materialized views, functions, procedures, triggers, sequences | views, functions, procedures |
| MySQL / MariaDB | MySQL 5.7 and newer, MariaDB | tables, views, procedures, functions, triggers | views, procedures, functions, triggers |
| SQLite | any database file | tables, views, triggers | views, triggers |
| Oracle | 12c and newer, no Oracle Client needed | tables, views, packages, procedures, functions, triggers, types, sequences, synonyms | PL/SQL units and views (compiled) |

For highlighting, install the **SQL** extension from **Settings → Extensions → Available**, or for
Oracle's SQL and PL/SQL (with outline and folding of packages and procedures), the **Oracle SQL and
PL/SQL** extension.

## Connections

Click **+** in the view (or run **New Database Connection…** from Find Action), pick the kind of
database at the top, and fill in:

- **PostgreSQL, MySQL / MariaDB:** **Host**, **Port** (5432 and 3306 by default), **Database** (for
  MySQL it may be left empty: the view then lists every database), **User** and **Password**, and
  **SSL**:
  - **Off:** not encrypted.
  - **Prefer** (the default): encrypted when the server allows it.
  - **Require:** always encrypted.
  - **Verify:** always encrypted, with the server's certificate and host name checked against the
    system's certificates.

  As with `psql`, Prefer and Require don't check the certificate, so a server with a self-signed
  certificate works with them.
- **SQLite:** the **Database file**, typed or picked with **Browse…**. A file that isn't there yet is
  created. There's no user or password.
- **Oracle:** **Host**, **Port** (1521 by default) and the **Service name**, or switch to **SID**. Or
  click **Use a connect string…** and enter one of these instead: EZConnect (`host:port/service`), a
  full TNS descriptor (`(DESCRIPTION=…)`), or an alias from `tnsnames.ora` (found through
  `TNS_ADMIN`). Then **User**, **Password** and **Role** (Default, SYSDBA or SYSOPER).

**Save password** keeps the password in the system keychain: Keychain on macOS, Credential Manager
on Windows. On Linux it uses the kernel keyring, which forgets it on restart. With the box unticked,
Kuana asks for the password in a small prompt each time you connect (it has its own Save password). PostgreSQL and MySQL connections may have an
empty password, for servers that trust the user.

**Test Connection** connects once and shows the server and its version. **Connect** saves the
connection and connects. Connections are kept in `~/.config/kuana/database/connections.json`,
without passwords. Right-click a connection to **Connect** or **Disconnect**, **Refresh**, open a
**New SQL Script**, **Edit Connection…** or **Delete Connection…**.

## Browsing

Once connected, the connection lists the objects of the schema it starts in (PostgreSQL's current
schema, usually `public`; MySQL's database; SQLite's `main`; your Oracle user's schema), grouped by
kind, each group with its count. Invalid Oracle objects are shown in red. **Other Schemas** (MySQL:
**Other Databases**) lists the rest. The filter field narrows every loaded list by name. Lists load
when you first open them; **Refresh** reads them again.

- A **view**, **function**, **procedure** or **trigger**, and Oracle's **packages** and **types**,
  opens its source in a tab when you click it. Open an Oracle package to see its **Body** and its
  procedures and functions; click one of those to jump to it in the body.
- A **table** opens to its columns, with their types; primary key columns have a key icon.
  Double-click a table for its DDL. Right-click it and choose **Select Rows** to see its first 1000 rows.
- Right-click any object for **Open Source** or **Show DDL**, **Copy Name**, and with Oracle
  **Open Specification** / **Open Body** and **Compile** (`ALTER … COMPILE`).

The keyboard works too: ↑/↓ to move, ←/→ to close and open, Enter to open, Escape to go back to the
editor.

## Editing objects

An object's source opens as a script in a file of its own, under
`~/.config/kuana/database/objects/`. Highlighting, undo, Local History, splits and Find all work
on it like any other file. Its name has its schema in it, so saving it replaces that schema's object.

**Save** (⌘S / Ctrl+S) runs the file against its database, and the result is shown in a
notification:

- **PostgreSQL:** the file's `CREATE OR REPLACE …` statements run in one transaction, so either all
  of them work or nothing changes. A function with overloads opens with all of them.
- **MySQL / MariaDB:** a view is replaced in place. A procedure, function or trigger is dropped and
  created again, since MySQL can't replace one; if the new version fails, the old one is put back.
- **SQLite:** a view or trigger is dropped and created in one transaction.
- **Oracle:** the unit is compiled. Its compile errors come back with their lines.

Errors become the file's problems: underlined where the database points, and listed in Problems.
Auto-save only writes the local file; only an explicit Save runs it. Opening the object again reads
it from the database again, unless its tab has unsaved edits. Tables and the other objects open
their DDL to read; saving those doesn't change the database.

## Running SQL

In any `.sql` file (and Oracle's `.pks`, `.pkb`, `.prc`, `.fnc`, `.trg` and the like),
**⌘↩ / Ctrl+Enter** runs the statement at the caret. With text selected, it runs the selection.
**Run SQL Script** runs the whole file. Both are also at the top of the editor's right-click menu
and in Find Action.

Once you have a connection, each statement in a script gets a green **▶** in the gutter, on its
first line; click it to run that statement. Object files don't get one, since saving them runs them.

Scripts split into statements as each database's own tools split them:

- Statements end with `;` outside strings, quoted names and comments.
- **PostgreSQL:** `$$ … $$` and `$tag$ … $tag$` bodies, `E'…'` strings and nested `/* */` comments
  stay whole; psql's `\…` commands are skipped.
- **MySQL / MariaDB:** `#` comments, backticks and backslash escapes are understood. A procedure,
  function or trigger with a `BEGIN … END` body is one statement, with or without `DELIMITER`.
- **SQLite:** a trigger's `BEGIN … END` body is one statement; sqlite3's `.…` commands are skipped.
- **Oracle:** PL/SQL blocks and `CREATE … PACKAGE` and similar units end with a line holding only
  `/`, as in SQL*Plus and SQL Developer. SQL*Plus commands such as `SET SERVEROUTPUT ON` and `PROMPT`
  are skipped, and `EXEC proc(…)` runs as a block.

The file's connection is shown in the status bar; click it to choose another. An object's file and a
**New SQL Script** keep their own connection. Any other file uses the one selected in the Database
view, else the only one connected, else your only connection. If that connection needs its
password, Kuana asks for it, then runs what you asked for.

Results show in the **Database** panel at the bottom:

- Each query gets a **Result** tab: a grid with up to 1000 rows that scrolls both ways. NULLs show as
  `(null)`. A longer result is cut there, and the server stops sending the rest. Click a cell and
  press ⌘C / Ctrl+C to copy it; the copy button copies every row as tab-separated text.
- **Output** lists every statement with what it did and how long it took, and what the server said
  meanwhile: Oracle's `DBMS_OUTPUT` (always on), PostgreSQL's notices (`RAISE NOTICE`), MySQL's
  warnings. Click a row to select its statement in the editor.
- An error is underlined in the file where the database points.
- With Oracle, changes from INSERT, UPDATE, DELETE and MERGE aren't committed until you click
  **Commit**, or until **Rollback** undoes them. PostgreSQL, MySQL and SQLite commit each statement,
  unless your script begins a transaction (`BEGIN`) and ends it itself.

## Not there yet

SQL Server, editing table data in the grid, explain plans, debuggers, and completion from the schema.

// `sqlite-link` perf fixture — a Rust binary that links libsqlite3
// statically via rusqlite's `bundled` feature, so the dep graph
// contains both pure-Rust crates and a non-trivial C compilation
// stage (cc-rs / make / clang).
//
// The point is not what the program does — it just runs an
// in-memory query. The point is that compiling this fixture
// exercises a different fingerprint surface than `medium` does, and
// any caching bug specific to build-script outputs / sysroot
// linking will show up here first.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
struct Row {
    id: i64,
    name: String,
}

fn main() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute(
        "CREATE TABLE rows (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        [],
    )?;
    for name in ["soldr", "zccache", "cargo"] {
        conn.execute("INSERT INTO rows (name) VALUES (?1)", params![name])?;
    }
    let mut stmt = conn.prepare("SELECT id, name FROM rows ORDER BY id")?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    println!("{}", serde_json::to_string(&rows)?);
    Ok(())
}

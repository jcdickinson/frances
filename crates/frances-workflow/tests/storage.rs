//! Integration tests for `frances:v1/storage` — happy path, drift
//! detection, and param-binding validation. Wires the workflow runtime
//! against `StubDeps`, which backs `workflow_db` with an in-memory
//! turso connection lazily initialised on first call.

use std::borrow::Cow;
use std::io::Write;

use frances_storage::Migration;
use frances_workflow::{
    Invocation, Runtime, SectionKind, SectionTranscript, test_deps::StubDeps,
    test_drive::drive_one_cycle,
};
use uuid::Uuid;

const ENTITY: Uuid = Uuid::from_u128(0x4d5d_3a5b_9d6c_4e5f_8b1a_4c2d_3e4f_5a6b);

const SCHEMA_V1: &str = "CREATE TABLE notes (id INTEGER PRIMARY KEY, text TEXT NOT NULL);";
const SCHEMA_V1_EDITED: &str =
    "CREATE TABLE notes (id INTEGER PRIMARY KEY, text TEXT NOT NULL, color TEXT);";

fn write_source(body: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(".js")
        .tempfile()
        .expect("tempfile");
    f.write_all(body.as_bytes()).expect("write");
    f
}

fn migration(name: &'static str, sql: &str) -> Migration {
    Migration {
        name: Cow::Borrowed(name),
        sql: Cow::Owned(sql.to_owned()),
    }
}

fn text_of(frame: &SectionTranscript) -> String {
    match frame {
        SectionTranscript::Set { section: spec, .. } => match &spec.kind {
            SectionKind::Markdown { .. } | SectionKind::Error => {
                spec.seed.clone().unwrap_or_default()
            }
            SectionKind::ToolUse { name, detail } => match detail {
                Some(d) => format!("→ {name}  {d}"),
                None => format!("→ {name}"),
            },
            SectionKind::Json { tag, value } => format!("[{tag}] {value}"),
            SectionKind::ShellOutput { state, cmd } => format!(
                "[shell:{state:?}] $ {cmd}
{}",
                spec.seed.clone().unwrap_or_default()
            ),
            SectionKind::Reasoning { state } => format!(
                "[reasoning:{state:?}]\n{}",
                spec.seed.clone().unwrap_or_default()
            ),
            SectionKind::Diff { lines } => format!("[diff:{} lines]", lines.len()),
            SectionKind::EntityRef { entity_id } => format!("[entity:{entity_id}]"),
        },
        SectionTranscript::Append { delta, .. } => delta.clone(),
        SectionTranscript::Close { id } => format!("[close:{}]", id.0),
    }
}

#[tokio::test]
async fn exec_query_and_query_stream_round_trip() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { db } from "frances:v1/storage";
        import { transcript, MarkdownSection, JsonSection } from "frances:v1/sections";

        const inserted = await db.exec(
            `INSERT INTO notes (text) VALUES (?)`,
            ["alpha"],
        );
        transcript.push(new MarkdownSection({
            content: `inserted:${inserted.rowsAffected}:${inserted.lastInsertRowid}`,
        }));

        await db.exec(`INSERT INTO notes (text) VALUES (?)`, ["beta"]);

        const rows = await db.query(`SELECT id, text FROM notes ORDER BY id`);
        transcript.push(new JsonSection({ tag: "query", value: rows }));

        const collected = [];
        const stream = db.queryStream(`SELECT text FROM notes ORDER BY id`);
        const reader = stream.getReader();
        while (true) {
            const { done, value } = await reader.read();
            if (done) break;
            collected.push(value.text);
        }
        transcript.push(new JsonSection({ tag: "stream", value: collected }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            entity: ENTITY,
            instance_id: Uuid::nil(),
            migrations: vec![migration("0001_init.sql", SCHEMA_V1)],
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(text_of(&frames[0]), "inserted:1:1");
    assert_eq!(
        text_of(&frames[1]),
        r#"[query] [{"id":1,"text":"alpha"},{"id":2,"text":"beta"}]"#
    );
    assert_eq!(text_of(&frames[2]), r#"[stream] ["alpha","beta"]"#);
}

#[tokio::test]
async fn transaction_commits_on_success() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { db } from "frances:v1/storage";
        import { transcript, JsonSection } from "frances:v1/sections";

        await db.transaction(async (tx) => {
            await tx.exec(`INSERT INTO notes (text) VALUES (?)`, ["one"]);
            await tx.exec(`INSERT INTO notes (text) VALUES (?)`, ["two"]);
        });

        const rows = await db.query(`SELECT text FROM notes ORDER BY id`);
        transcript.push(new JsonSection({ tag: "after", value: rows.map(r => r.text) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            entity: Uuid::from_u128(0xa1),
            instance_id: Uuid::nil(),
            migrations: vec![migration("0001_init.sql", SCHEMA_V1)],
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(text_of(&frames[0]), r#"[after] ["one","two"]"#);
}

#[tokio::test]
async fn transaction_rolls_back_on_throw() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { db } from "frances:v1/storage";
        import { transcript, JsonSection } from "frances:v1/sections";

        let caught;
        try {
            await db.transaction(async (tx) => {
                await tx.exec(`INSERT INTO notes (text) VALUES (?)`, ["doomed"]);
                throw new Error("rolling back");
            });
        } catch (e) {
            caught = e.message;
        }

        const rows = await db.query(`SELECT text FROM notes`);
        transcript.push(new JsonSection({ tag: "caught", value: caught }));
        transcript.push(new JsonSection({ tag: "rows", value: rows }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            entity: Uuid::from_u128(0xa2),
            instance_id: Uuid::nil(),
            migrations: vec![migration("0001_init.sql", SCHEMA_V1)],
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(text_of(&frames[0]), r#"[caught] "rolling back""#);
    assert_eq!(text_of(&frames[1]), r#"[rows] []"#);
}

#[tokio::test]
async fn explicit_commit_then_throw_keeps_committed_rows() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { db } from "frances:v1/storage";
        import { transcript, JsonSection } from "frances:v1/sections";

        try {
            await db.transaction(async (tx) => {
                await tx.exec(`INSERT INTO notes (text) VALUES (?)`, ["kept"]);
                await tx.commit();
                throw new Error("after commit");
            });
        } catch (_e) {}

        const rows = await db.query(`SELECT text FROM notes`);
        transcript.push(new JsonSection({ tag: "rows", value: rows.map(r => r.text) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            entity: Uuid::from_u128(0xa3),
            instance_id: Uuid::nil(),
            migrations: vec![migration("0001_init.sql", SCHEMA_V1)],
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(text_of(&frames[0]), r#"[rows] ["kept"]"#);
}

#[tokio::test]
async fn unsupported_param_type_throws_typeerror() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { db } from "frances:v1/storage";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        let caught;
        try {
            await db.exec(`INSERT INTO notes (text) VALUES (?)`, [() => 1]);
        } catch (e) {
            caught = e.message;
        }
        transcript.push(new MarkdownSection({ content: caught ?? "no error" }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            entity: Uuid::from_u128(0xa4),
            instance_id: Uuid::nil(),
            migrations: vec![migration("0001_init.sql", SCHEMA_V1)],
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let msg = text_of(&frames[0]);
    assert!(msg.contains("unsupported parameter type"), "got: {msg}");
}

#[tokio::test]
async fn drift_in_migration_sql_fails_start() {
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(r#"// no-op body"#);

    // First start applies `SCHEMA_V1` cleanly.
    let _handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            entity: ENTITY,
            instance_id: Uuid::nil(),
            migrations: vec![migration("0001_init.sql", SCHEMA_V1)],
        })
        .await
        .unwrap();

    // Drop the cached handle but keep the turso connection (and its
    // `_migrations` rows). A re-start with edited SQL for the same name
    // should be rejected by the migrator.
    deps.forget_workflow_db(ENTITY);

    let result = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            entity: ENTITY,
            instance_id: Uuid::nil(),
            migrations: vec![migration("0001_init.sql", SCHEMA_V1_EDITED)],
        })
        .await;
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("expected migration drift error"),
    };
    let msg = err.to_string();
    assert!(msg.contains("checksum"), "got: {msg}");
}

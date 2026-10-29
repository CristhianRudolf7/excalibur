use excalibur_store::{
    DatabaseKind, initialize_sqlite,
    schema::{self, VectorEncoding},
};
use rusqlite::Connection;

fn main_db() -> Connection {
    initialize_sqlite().unwrap();
    let mut connection = Connection::open_in_memory().unwrap();
    schema::migrate(&mut connection, DatabaseKind::Main).unwrap();
    connection
}

#[test]
fn normalized_natural_language_index_preserves_legacy_rows_and_trigger_contracts() {
    initialize_sqlite().unwrap();
    let mut db = Connection::open_in_memory().unwrap();
    schema::migrate_to(&mut db, DatabaseKind::Main, 2).unwrap();
    db.execute("INSERT INTO memories(id,statement,scope_id) VALUES('mem_old','Canción e información sobre ộ','project:a')",[]).unwrap();
    schema::migrate(&mut db, DatabaseKind::Main).unwrap();
    for query in ["cancion", "informacion", "o"] {
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH ?1",
                [query],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            1
        );
    }
    let sql: String = db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='memories_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("unicode61 remove_diacritics 2"));
    db.execute(
        "UPDATE memories SET statement='árbol actualizado' WHERE id='mem_old'",
        [],
    )
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'arbol'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        1
    );
    schema::migrate_to(&mut db, DatabaseKind::Main, 2).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT statement FROM memories_fts WHERE memories_fts MATCH 'arbol'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "árbol actualizado"
    );
    db.execute("DELETE FROM memories WHERE id='mem_old'", [])
        .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'arbol'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn codegraph_trigram_indexes_paths_code_and_symbol_substrings() {
    initialize_sqlite().unwrap();
    let mut db = Connection::open_in_memory().unwrap();
    schema::migrate_to(&mut db, DatabaseKind::CodeGraph, 2).unwrap();
    db.execute("INSERT INTO files(id,path,hash) VALUES('file1','src/authentication/session_store.rs','synthetic')",[]).unwrap();
    db.execute("INSERT INTO nodes(id,file_id,kind,name) VALUES('node1','file1','function','refresh_session_token')",[]).unwrap();
    schema::migrate(&mut db, DatabaseKind::CodeGraph).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM files_fts WHERE files_fts MATCH 'session'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM nodes_fts WHERE nodes_fts MATCH 'session_tok'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        1
    );
    db.execute(
        "UPDATE files SET content='fn renew_access_token() {}' WHERE id='file1'",
        [],
    )
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM files_fts WHERE files_fts MATCH 'access_tok'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        1
    );
    db.execute("DELETE FROM nodes WHERE id='node1'", [])
        .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM nodes_fts WHERE nodes_fts MATCH 'session_tok'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
    schema::rebuild_lexical_indexes(&db, DatabaseKind::CodeGraph).unwrap();
    schema::migrate_to(&mut db, DatabaseKind::CodeGraph, 2).unwrap();
    assert_eq!(
        db.query_row("SELECT path FROM files WHERE id='file1'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "src/authentication/session_store.rs"
    );
}

#[test]
fn scoped_vectors_require_explicit_partitions_and_keep_same_object_in_distinct_scopes() {
    let db = main_db();
    assert_eq!(
        schema::create_vector_space(&db, "synthetic", 3).unwrap(),
        "scoped_vectors_synthetic"
    );
    schema::put_vector(&db, "synthetic", "project:a", "shared-object", "[0,0,1]").unwrap();
    schema::put_vector(&db, "synthetic", "project:b", "shared-object", "[1,0,0]").unwrap();
    assert!(schema::query_vectors(&db, "synthetic", &[], "[1,0,0]", 5).is_err());
    assert!(schema::query_vectors(&db, "synthetic", &[""], "[1,0,0]", 5).is_err());
    let a = schema::query_vectors(&db, "synthetic", &["project:a"], "[1,0,0]", 5).unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].scope_id, "project:a");
    assert!(a[0].distance > 1.0);
    let both =
        schema::query_vectors(&db, "synthetic", &["project:a", "project:b"], "[1,0,0]", 5).unwrap();
    assert_eq!(both.len(), 2);
    assert_eq!(both[0].scope_id, "project:b");
    assert_eq!(both[0].distance, 0.0);
    assert!(schema::put_vector(&db, "synthetic", "project:a", "shared-object", "[1,2]").is_err());
    assert_eq!(
        schema::query_vectors(&db, "synthetic", &["project:a"], "[0,0,1]", 1).unwrap()[0].distance,
        0.0,
        "invalid replacement rolled back"
    );
    assert_eq!(
        schema::delete_vector(&db, "synthetic", "project:a", "shared-object").unwrap(),
        1
    );
    assert_eq!(
        schema::query_vectors(&db, "synthetic", &["project:b"], "[1,0,0]", 1)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn migration_preserves_legacy_vectors_and_derives_or_quarantines_scope() {
    initialize_sqlite().unwrap();
    let mut db = Connection::open_in_memory().unwrap();
    schema::migrate_to(&mut db, DatabaseKind::Main, 2).unwrap();
    db.execute("INSERT INTO memories(id,statement,scope_id) VALUES('mem_known','known source','project:known')",[]).unwrap();
    db.execute_batch("CREATE VIRTUAL TABLE vectors_legacy USING vec0(object_id TEXT PRIMARY KEY,embedding float[3]); INSERT INTO vectors_legacy(object_id,embedding) VALUES('mem_known','[1,0,0]'),('unknown-object','[0,1,0]');").unwrap();
    schema::migrate(&mut db, DatabaseKind::Main).unwrap();
    let known = schema::query_vectors(&db, "legacy", &["project:known"], "[1,0,0]", 5).unwrap();
    assert_eq!(known.len(), 1);
    assert_eq!(known[0].object_id, "mem_known");
    assert!(
        schema::query_vectors(&db, "legacy", &["general", "project:other"], "[0,1,0]", 5)
            .unwrap()
            .is_empty()
    );
    let quarantined =
        schema::query_vectors(&db, "legacy", &["legacy:unscoped"], "[0,1,0]", 5).unwrap();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0].object_id, "unknown-object");
    schema::migrate_to(&mut db, DatabaseKind::Main, 2).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM vectors_legacy", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        2,
        "older readers retain legacy rows"
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        schema::CURRENT_VERSION
    );
    schema::migrate(&mut db, DatabaseKind::Main).unwrap();
    assert_eq!(
        schema::query_vectors(
            &db,
            "legacy",
            &["project:known", "legacy:unscoped"],
            "[1,0,0]",
            5
        )
        .unwrap()
        .len(),
        2,
        "reopening does not duplicate migrated rows"
    );
}

#[test]
fn stable_vector_version_quantization_and_long_metadata_delete_regression() {
    let db = main_db();
    assert_eq!(
        db.query_row("SELECT vec_version()", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "v0.1.9"
    );
    schema::create_quantized_vector_space(&db, "i8", 3, VectorEncoding::Int8).unwrap();
    let long_scope = "project:synthetic-scope-with-more-than-twelve-characters";
    let long_object = "document:synthetic-object-with-more-than-twelve-characters";
    schema::put_vector(&db, "i8", long_scope, long_object, "[1,2,3]").unwrap();
    assert_eq!(
        schema::query_vectors(&db, "i8", &[long_scope], "[1,2,3]", 1).unwrap()[0].distance,
        0.0
    );
    assert_eq!(
        schema::delete_vector(&db, "i8", long_scope, long_object).unwrap(),
        1
    );
    schema::create_quantized_vector_space(&db, "binary", 8, VectorEncoding::Bit).unwrap();
    schema::put_vector(
        &db,
        "binary",
        long_scope,
        "binary-object",
        "[1,-1,1,-1,1,-1,1,-1]",
    )
    .unwrap();
    assert_eq!(
        schema::query_vectors(&db, "binary", &[long_scope], "[1,-1,1,-1,1,-1,1,-1]", 1).unwrap()[0]
            .distance,
        0.0
    );
    assert!(
        schema::create_quantized_vector_space(&db, "bad_bits", 7, VectorEncoding::Bit).is_err()
    );
    assert!(schema::create_quantized_vector_space(&db, "i8", 4, VectorEncoding::Int8).is_err());
}

#[test]
fn unsupported_legacy_vector_upgrade_rolls_back_all_index_changes() {
    initialize_sqlite().unwrap();
    let mut db = Connection::open_in_memory().unwrap();
    schema::migrate_to(&mut db, DatabaseKind::Main, 2).unwrap();
    db.execute(
        "INSERT INTO memories(id,statement) VALUES('mem_preserved','synthetic preserved')",
        [],
    )
    .unwrap();
    db.execute_batch("CREATE VIRTUAL TABLE vectors_external USING vec0(object_id TEXT PRIMARY KEY,embedding int8[3]); INSERT INTO vectors_external(object_id,embedding) VALUES('external',vec_int8('[1,2,3]'));").unwrap();
    assert!(schema::migrate(&mut db, DatabaseKind::Main).is_err());
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM vectors_external", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'preserved'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        1
    );
    assert!(
        db.prepare("SELECT * FROM vector_indexes").is_err(),
        "new migration metadata rolled back"
    );
    let sql: String = db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='memories_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !sql.contains("remove_diacritics 2"),
        "FTS replacement rolled back too"
    );
}

use excalibur_code_intelligence::*;
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use std::path::Path;
async fn fixture() -> (tempfile::TempDir, Database, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("codegraph.db"),
        DatabaseKind::CodeGraph,
        StorageConfig::default(),
    )
    .unwrap();
    let graph = CodeGraph::open(db.clone(), "project").await.unwrap();
    (dir, db, graph)
}
#[tokio::test]
async fn real_tree_sitter_graph_is_incremental_by_hash_and_known_test_impact_survives_restart() {
    let (_dir, db, graph) = fixture().await;
    let math = "pub fn add(a: i32, b: i32) -> i32 { a + b }\n";
    let first = graph
        .index_path_content(Path::new("src/math.rs"), math)
        .await
        .unwrap();
    assert_eq!(first.mode, ParseMode::Cold);
    assert!(first.syntax_errors.is_empty());
    let tests = "use crate::math::add;\n#[test]\nfn test_add(){ let actual=add(1,2); assert_eq!(actual,3); }\n";
    graph
        .index_path_content(Path::new("tests/test_math.rs"), tests)
        .await
        .unwrap();
    assert_eq!(
        graph
            .index_path_content(Path::new("src/math.rs"), math)
            .await
            .unwrap()
            .mode,
        ParseMode::Unchanged
    );
    let callers = graph
        .query(GraphQuery::Callers {
            symbol: "add".into(),
            limit: 100,
        })
        .await
        .unwrap();
    assert!(callers.nodes.iter().any(|n| n.name == "test_add"));
    assert!(
        callers
            .edges
            .iter()
            .any(|e| e.kind == "tests" && e.resolution == "syntactic_name")
    );
    let dependencies = graph
        .query(GraphQuery::Dependencies {
            path: "tests/test_math.rs".into(),
            limit: 100,
        })
        .await
        .unwrap();
    assert!(dependencies.edges.iter().any(|e| {
        e.kind == "import"
            && dependencies
                .nodes
                .iter()
                .any(|n| n.id == e.destination_id && n.path.as_deref() == Some("src/math.rs"))
    }));
    let impacted = graph.impacted_tests(&["src/math.rs".into()]).await.unwrap();
    assert!(impacted.iter().any(|n| n.name == "test_add"));
    let modified = "pub fn add(a: i32, b: i32) -> i32 { let _unicode=\"日本語 🦀\"; a + b + 0 }\n";
    let changed = graph
        .index_path_content(Path::new("src/math.rs"), modified)
        .await
        .unwrap();
    assert_eq!(changed.mode, ParseMode::Incremental);
    assert!(changed.syntax_errors.is_empty());
    assert_ne!(first.hash, changed.hash);
    let stored = db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT content FROM files WHERE path='src/math.rs'",
                [],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        stored.is_empty(),
        "full source must not be duplicated into plaintext graph storage"
    );
    assert!(CodeGraph::open(db.clone(), "other-project").await.is_err());
    drop(graph);
    let graph = CodeGraph::open(db.clone(), "project").await.unwrap();
    assert_eq!(
        graph
            .index_path_content(Path::new("src/math.rs"), modified)
            .await
            .unwrap()
            .mode,
        ParseMode::Unchanged
    );
    assert_eq!(
        graph.indexed_paths().await.unwrap(),
        vec!["src/math.rs", "tests/test_math.rs"]
    );
    assert!(graph.remove_file(Path::new("src/math.rs")).await.unwrap());
    let page = graph
        .query(GraphQuery::Symbols {
            query: "add".into(),
            limit: 100,
        })
        .await
        .unwrap();
    assert!(
        !page
            .nodes
            .iter()
            .any(|n| n.kind == "function" && n.name == "add")
    );
    let integrity = db
        .read(ReadLane::Interactive, |c| {
            let mut q = c.prepare("PRAGMA foreign_key_check")?;
            Ok(q.query([])?.next()?.is_none())
        })
        .await
        .unwrap();
    assert!(integrity);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn multilingual_symbols_documents_imports_requirements_and_syntax_errors_are_real() {
    let (_dir, db, graph) = fixture().await;
    graph
        .index_path_content(Path::new("pkg/math.py"), "def add(a,b):\n    return a+b\n")
        .await
        .unwrap();
    graph
        .index_path_content(
            Path::new("tests/test_math.py"),
            "from pkg.math import add\ndef test_python():\n    assert add(1,2)==3\n",
        )
        .await
        .unwrap();
    graph.index_path_content(Path::new("src/math.ts"),"export function sum(a:number,b:number):number { return a+b; }\nexport interface Result { value:number; }\n").await.unwrap();
    graph
        .index_path_content(
            Path::new("src/math.test.ts"),
            "import {sum} from './math';\ntest('sum works',()=> { return sum(1,2); });\n",
        )
        .await
        .unwrap();
    graph
        .index_path_content(
            Path::new("docs/API.md"),
            "# API\n## Addition\nSee [implementation](../pkg/math.py) and `sum`.\n",
        )
        .await
        .unwrap();
    graph
        .index_path_content(Path::new("config.json"), "{\"enabled\":true}")
        .await
        .unwrap();
    for name in ["add", "sum", "Result", "Addition"] {
        let page = graph
            .query(GraphQuery::Symbols {
                query: name.into(),
                limit: 100,
            })
            .await
            .unwrap();
        assert!(page.nodes.iter().any(|n| n.name == name), "missing {name}");
    }
    let js_tests = graph.impacted_tests(&["src/math.ts".into()]).await.unwrap();
    assert!(js_tests.iter().any(|n| n.name == "sum works"));
    let py_tests = graph.impacted_tests(&["pkg/math.py".into()]).await.unwrap();
    assert!(py_tests.iter().any(|n| n.name == "test_python"));
    let cited=db.read(ReadLane::Interactive,|c|Ok(c.query_row("SELECT count(*) FROM edges e JOIN nodes n ON n.id=e.source_id JOIN files f ON f.id=n.file_id WHERE f.path='docs/API.md' AND e.kind='cites'",[],|r|r.get::<_,u64>(0))?)).await.unwrap();
    assert!(cited >= 2);
    let target = graph
        .query(GraphQuery::Symbols {
            query: "sum".into(),
            limit: 100,
        })
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|n| n.kind == "function" && n.name == "sum")
        .unwrap();
    graph
        .link_requirement(
            "R4",
            std::slice::from_ref(&target.id),
            "confirmed:run:edit:7",
        )
        .await
        .unwrap();
    let mapped = graph
        .query(GraphQuery::Implementations {
            requirement_id: "R4".into(),
            limit: 100,
        })
        .await
        .unwrap();
    assert!(mapped.nodes.iter().any(|n| n.id == target.id));
    assert_eq!(mapped.edges[0].resolution, "host:confirmed:run:edit:7");
    let bad = graph
        .index_path_content(Path::new("broken.rs"), "fn broken( { let value = ; }")
        .await
        .unwrap();
    assert!(!bad.syntax_errors.is_empty());
    let again = graph
        .index_path_content(Path::new("broken.rs"), "fn broken( { let value = ; }")
        .await
        .unwrap();
    assert_eq!(again.syntax_errors, bad.syntax_errors);
    assert_eq!(again.mode, ParseMode::Unchanged);
    assert!(
        graph
            .index_path_content(Path::new("../escape.rs"), "fn f(){}")
            .await
            .is_err()
    );
    assert!(matches!(
        graph
            .index_path_content(Path::new("unknown.xyz"), "data")
            .await,
        Err(Error::UnsupportedLanguage)
    ));
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn ambiguity_remains_unresolved_and_lsp_enrichment_rejects_stale_hashes() {
    let (_dir, db, graph) = fixture().await;
    let a = graph
        .index_path_content(Path::new("a.py"), "def duplicate():\n    return 1\n")
        .await
        .unwrap();
    let b = graph
        .index_path_content(Path::new("b.py"), "def duplicate():\n    return 2\n")
        .await
        .unwrap();
    let caller = graph
        .index_path_content(
            Path::new("caller.py"),
            "def call_it():\n    return duplicate()\n",
        )
        .await
        .unwrap();
    let page = graph
        .query(GraphQuery::Callers {
            symbol: "duplicate".into(),
            limit: 100,
        })
        .await
        .unwrap();
    assert!(page.edges.iter().all(|e| e.resolution == "unresolved"));
    let symbols = graph
        .query(GraphQuery::Symbols {
            query: String::new(),
            limit: 100,
        })
        .await
        .unwrap();
    let source = symbols.nodes.iter().find(|n| n.name == "call_it").unwrap();
    let target = symbols
        .nodes
        .iter()
        .find(|n| n.name == "duplicate" && n.path.as_deref() == Some("a.py"))
        .unwrap();
    graph
        .enrich_edge(&source.id, &target.id, "call", &caller.hash, &a.hash)
        .await
        .unwrap();
    assert!(
        graph
            .enrich_edge(&source.id, &target.id, "call", &caller.hash, &b.hash)
            .await
            .is_err()
    );
    let page = graph
        .query(GraphQuery::Callers {
            symbol: target.id.clone(),
            limit: 100,
        })
        .await
        .unwrap();
    assert!(page.edges.iter().any(|e| e.resolution == "lsp"));
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn anonymous_missing_punctuation_is_reported_even_when_the_root_has_no_error_node() {
    let (_dir, db, graph) = fixture().await;
    let before = graph
        .index_path_content(Path::new("file.rs"), "fn old() {}\n// OLD_ERROR\n")
        .await
        .unwrap();
    assert!(before.syntax_errors.is_empty());
    let after = graph
        .index_path_content(
            Path::new("file.rs"),
            "fn old( {}\n// NEW_ERROR\n// OLD_ERROR\n",
        )
        .await
        .unwrap();
    assert!(
        after
            .syntax_errors
            .iter()
            .any(|issue| issue.kind == ")" && issue.message == "missing )"),
        "{:?}",
        after.syntax_errors
    );
    assert!(
        after
            .syntax_errors
            .iter()
            .any(|issue| issue.start_byte == issue.end_byte)
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn concurrent_indexers_serialize_writes_and_javascript_tsx_are_real_grammars() {
    let (_dir, db, graph) = fixture().await;
    let mut jobs = Vec::new();
    for index in 0..20 {
        let graph = graph.clone();
        jobs.push(tokio::spawn(async move {
            graph
                .index_path_content(
                    Path::new(&format!("src/file_{index}.rs")),
                    &format!("pub fn unique_{index}() -> i32 {{ {index} }}\n"),
                )
                .await
        }));
    }
    for job in jobs {
        job.await.unwrap().unwrap();
    }
    assert_eq!(graph.indexed_paths().await.unwrap().len(), 20);
    let js = graph
        .index_path_content(
            Path::new("src/component.js"),
            "export const run = () => 1;\nexport class Controller { run(){ return 2; } }\n",
        )
        .await
        .unwrap();
    assert!(js.syntax_errors.is_empty());
    let tsx = graph
        .index_path_content(
            Path::new("src/component.tsx"),
            "export function View(){ return <div>synthetic</div>; }\n",
        )
        .await
        .unwrap();
    assert!(tsx.syntax_errors.is_empty());
    assert!(
        graph
            .query(GraphQuery::Symbols {
                query: "View".into(),
                limit: 10
            })
            .await
            .unwrap()
            .nodes
            .iter()
            .any(|n| n.name == "View")
    );
    let redactor = excalibur_vault::Redactor::with_secrets([excalibur_vault::SecretValue::new(
        b"synthetic-private-heading".to_vec(),
    )
    .unwrap()]);
    let private = graph.clone().with_redactor(redactor);
    let report = private
        .index_path_content(Path::new("README.md"), "# synthetic-private-heading\n")
        .await
        .unwrap();
    assert!(report.redacted_nodes > 0);
    assert!(
        !serde_json::to_string(
            &private
                .query(GraphQuery::Symbols {
                    query: "synthetic".into(),
                    limit: 100
                })
                .await
                .unwrap()
        )
        .unwrap()
        .contains("synthetic-private-heading")
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn existing_codegraph_file_identity_is_adopted_and_new_project_ids_are_disjoint() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("legacy.db"),
        DatabaseKind::CodeGraph,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(excalibur_store::Priority::Normal, |c| {
        c.execute(
            "INSERT INTO files(id,path,hash,language) VALUES('legacy-file','lib.rs','old','rust')",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let graph = CodeGraph::open(db.clone(), "project").await.unwrap();
    let indexed = graph
        .index_path_content(Path::new("lib.rs"), "fn existing() {}\n")
        .await
        .unwrap();
    assert_eq!(indexed.file_id, "legacy-file");
    let otherdb = Database::open(
        dir.path().join("other.db"),
        DatabaseKind::CodeGraph,
        StorageConfig::default(),
    )
    .unwrap();
    let other = CodeGraph::open(otherdb.clone(), "other-project")
        .await
        .unwrap();
    let first = graph
        .index_path_content(Path::new("new.rs"), "fn shared_name() {}\n")
        .await
        .unwrap();
    let second = other
        .index_path_content(Path::new("new.rs"), "fn shared_name() {}\n")
        .await
        .unwrap();
    assert_ne!(first.file_id, second.file_id);
    let one = graph
        .query(GraphQuery::Symbols {
            query: "shared_name".into(),
            limit: 10,
        })
        .await
        .unwrap();
    let two = other
        .query(GraphQuery::Symbols {
            query: "shared_name".into(),
            limit: 10,
        })
        .await
        .unwrap();
    assert_ne!(one.nodes[0].id, two.nodes[0].id);
    otherdb.shutdown().await.unwrap();
    db.shutdown().await.unwrap();
}

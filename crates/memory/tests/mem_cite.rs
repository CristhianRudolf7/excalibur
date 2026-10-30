//! Suite `mem-cite`: exact page and paragraph, equivalent locations without pagination,
//! removal of citations not retrieved in the task tree, and web sources limited to pages read.
mod common;
use common::*;
use excalibur_memory::{testing::*, *};
use std::sync::{Arc, atomic::Ordering};

fn para(text: &str) -> ExtractedBlock {
    ExtractedBlock {
        kind: BlockKind::Paragraph,
        text: text.into(),
    }
}
fn pdf_pages() -> Vec<ExtractedPage> {
    vec![
        ExtractedPage {
            number: 1,
            blocks: vec![
                ExtractedBlock {
                    kind: BlockKind::Heading(1),
                    text: "Introduction".into(),
                },
                para(
                    "The reactor uses molten salt as coolant and operates at atmospheric pressure.",
                ),
                para("Its design dates from the 1960s."),
            ],
        },
        ExtractedPage {
            number: 2,
            blocks: vec![
                para(
                    "Thorium fuel cycles reduce long-lived transuranic waste compared with uranium.",
                ),
                para(
                    "Neutron economy limits the breeding ratio to about 1.07 in the reference core.",
                ),
            ],
        },
    ]
}

async fn library(services: LibraryServices) -> Fixture {
    let mut f = fixture().await;
    f.store = f.store.clone().with_library(services);
    f
}

#[tokio::test]
async fn paginated_citations_resolve_page_and_paragraph_and_invalid_ones_are_removed() {
    let f = library(LibraryServices {
        vision: Some(Arc::new(FixedVision(pdf_pages()))),
        ..Default::default()
    })
    .await;
    let g = grant(&["research"]);
    let doc = f
        .store
        .library_ingest(
            &g,
            IngestRequest {
                scope: "research".into(),
                title: "MSR Review".into(),
                media_type: "application/pdf".into(),
                origin: Some("upload".into()),
                bytes: b"%PDF-1.7 synthetic".to_vec(),
                document_id: None,
                library: false,
            },
            NOW,
        )
        .await
        .unwrap();
    assert!(doc.paginated);
    assert_eq!(doc.chunks, 4);
    // A child run of the master retrieves the evidence.
    let tree = StaticRunTree::default()
        .with_child("master", "child")
        .with_child("other", "sibling");
    let (chunks, _) = f
        .store
        .library_search(
            &g.clone().with_run("child"),
            "thorium transuranic waste",
            1,
            NOW,
        )
        .await
        .unwrap();
    let thorium = chunks.first().unwrap();
    assert_eq!(
        thorium.location.as_deref(),
        Some("MSR Review, p. 2, para. 1")
    );
    let cite = thorium.cite.clone().unwrap();
    assert_eq!(cite, format!("[[doc:{}#c3]]", doc.document_id));
    let answer = format!(
        "Thorium reduces transuranic waste {cite}. The breeding ratio is 1.07 [[doc:{}#c4]]. Invented claim [[doc:doc_fake#c1]].",
        doc.document_id
    );
    let checked = f
        .store
        .validate_citations(&g, "master", &tree, &answer, "en")
        .await
        .unwrap();
    assert_eq!(
        checked.text,
        "Thorium reduces transuranic waste [MSR Review, p. 2, para. 1]. The breeding ratio is 1.07 [unsourced]. Invented claim [unsourced]."
    );
    assert_eq!(checked.citations.len(), 1);
    assert_eq!(checked.removed.len(), 2);
    assert_eq!(checked.unsourced_sentences, 2);
    let es = f
        .store
        .validate_citations(
            &g,
            "master",
            &tree,
            &format!("Reduce residuos {cite}."),
            "es",
        )
        .await
        .unwrap();
    assert_eq!(es.text, "Reduce residuos [MSR Review, p. 2, párr. 1].");
    // A sibling tree did not retrieve the chunk.
    let other = f
        .store
        .validate_citations(&g, "other", &tree, &format!("Claim {cite}."), "en")
        .await
        .unwrap();
    assert_eq!(other.text, "Claim [unsourced].");
    // Another principal cannot validate this retrieval.
    let stranger = MemoryGrant::new("stranger", ["research"]);
    assert!(
        f.store
            .validate_citations(&stranger, "master", &tree, &format!("Claim {cite}."), "en")
            .await
            .unwrap()
            .citations
            .is_empty()
    );
}

#[tokio::test]
async fn unpaginated_documents_cite_section_paragraph_and_lines_without_pages() {
    let context = Arc::new(TitleContextWriter {
        calls: Default::default(),
    });
    let f = library(LibraryServices {
        context: Some(context.clone()),
        ..Default::default()
    })
    .await;
    let g = grant(&["research"]);
    let text = "# Setup\n\nInstall the toolchain first.\n\n## Database\n\nRun the migrations before seeding.\nThis takes a minute.\n\n| key | value |\n| port | 5432 |\n";
    let doc = f
        .store
        .library_ingest(
            &g,
            IngestRequest {
                scope: "research".into(),
                title: "Guide".into(),
                media_type: "text/markdown".into(),
                origin: None,
                bytes: text.as_bytes().to_vec(),
                document_id: None,
                library: true,
            },
            NOW,
        )
        .await
        .unwrap();
    assert!(!doc.paginated);
    assert_eq!(doc.chunks, 3);
    assert!(
        context.calls.load(Ordering::SeqCst) >= 1,
        "library documents get context sentences"
    );
    let (chunks, _) = f
        .store
        .library_search(&g.clone().with_run("r1"), "migrations seeding", 3, NOW)
        .await
        .unwrap();
    let location = chunks[0].location.clone().unwrap();
    assert_eq!(
        location,
        "Guide, § Setup > Database, para. 1, lines 7–8, no pagination"
    );
    assert!(!location.contains("p. "));
    let checked = f
        .store
        .validate_citations(
            &g,
            "r1",
            &StaticRunTree::default(),
            &format!("Migrate first {}.", chunks[0].cite.clone().unwrap()),
            "en",
        )
        .await
        .unwrap();
    assert_eq!(checked.text, format!("Migrate first [{location}]."));

    let plain = library(LibraryServices {
        context: Some(context.clone()),
        ..Default::default()
    })
    .await;
    let before = context.calls.load(Ordering::SeqCst);
    plain
        .store
        .library_ingest(
            &g,
            IngestRequest {
                scope: "research".into(),
                title: "Note".into(),
                media_type: "text/plain".into(),
                origin: None,
                bytes: b"Short.".to_vec(),
                document_id: None,
                library: false,
            },
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(
        context.calls.load(Ordering::SeqCst),
        before,
        "no context sentence outside the library"
    );
}

#[tokio::test]
async fn web_sources_are_only_pages_read_and_cited_in_the_tree() {
    let f = fixture().await;
    let g = grant(&["general"]);
    for r in ["master", "child", "unrelated"] {
        run(&f.db, r).await;
    }
    let child = g.clone().with_run("child");
    let cited = f
        .store
        .record_source_read(
            &child,
            "general",
            Some("s1"),
            "https://example.org/a",
            "Page A",
            b"aaa",
            NOW,
        )
        .await
        .unwrap();
    let _read_not_cited = f
        .store
        .record_source_read(
            &child,
            "general",
            Some("s1"),
            "https://example.org/b",
            "Page B",
            b"bbb",
            NOW,
        )
        .await
        .unwrap();
    let foreign = f
        .store
        .record_source_read(
            &g.clone().with_run("unrelated"),
            "general",
            None,
            "https://example.org/c",
            "Page C",
            b"ccc",
            NOW,
        )
        .await
        .unwrap();
    let tree = StaticRunTree::default().with_child("master", "child");
    let answer = format!(
        "A says yes [[web:{cited}]]. C says no [[web:{foreign}]]. Again A [[web:{cited}]]."
    );
    let checked = f
        .store
        .validate_citations(&g, "master", &tree, &answer, "en")
        .await
        .unwrap();
    assert_eq!(
        checked.text,
        "A says yes [1]. C says no [unsourced]. Again A [1]."
    );
    assert_eq!(checked.sources.len(), 1);
    assert_eq!(checked.sources[0].url, "https://example.org/a");
    assert!(
        f.store
            .record_source_read(&g, "general", None, "https://x", "x", b"x", NOW)
            .await
            .is_err(),
        "a read needs its run"
    );
}

#[tokio::test]
async fn versions_transcripts_and_guard() {
    let f = library(LibraryServices {
        transcriber: Some(Arc::new(FixedTranscriber(vec![
            TimedSegment {
                start_ms: 0,
                end_ms: 4000,
                text: "Welcome to the quarterly review.".into(),
            },
            TimedSegment {
                start_ms: 65_000,
                end_ms: 70_000,
                text: "Revenue grew eleven percent.".into(),
            },
        ]))),
        ..Default::default()
    })
    .await;
    let g = grant(&["general"]);
    let req = |bytes: &[u8], id: Option<String>| IngestRequest {
        scope: "general".into(),
        title: "Spec".into(),
        media_type: "text/plain".into(),
        origin: None,
        bytes: bytes.to_vec(),
        document_id: id,
        library: false,
    };
    let v1 = f
        .store
        .library_ingest(&g, req(b"The limit is 10 requests per minute.", None), NOW)
        .await
        .unwrap();
    let same = f
        .store
        .library_ingest(
            &g,
            req(
                b"The limit is 10 requests per minute.",
                Some(v1.document_id.clone()),
            ),
            NOW,
        )
        .await
        .unwrap();
    assert!(same.unchanged && same.version == 1);
    let v2 = f
        .store
        .library_ingest(
            &g,
            req(
                b"The limit is 50 requests per minute.",
                Some(v1.document_id.clone()),
            ),
            NOW + 1,
        )
        .await
        .unwrap();
    assert_eq!(v2.version, 2);
    assert_eq!(
        f.store
            .document_versions(&g, &v1.document_id)
            .await
            .unwrap()
            .len(),
        2
    );
    let (chunks, _) = f
        .store
        .library_search(&g, "limit requests per minute", 5, NOW)
        .await
        .unwrap();
    assert_eq!(chunks.len(), 1);
    assert!(
        chunks[0].text.contains("50"),
        "only the current version is searched"
    );

    let audio = f
        .store
        .library_ingest(
            &g,
            IngestRequest {
                media_type: "audio/ogg".into(),
                title: "Call".into(),
                ..req(b"OggS", None)
            },
            NOW,
        )
        .await
        .unwrap();
    let (chunks, _) = f
        .store
        .library_search(&g, "revenue grew", 2, NOW)
        .await
        .unwrap();
    assert!(
        chunks
            .iter()
            .any(|c| c.location.as_deref() == Some("Call, 00:00:00–00:01:10")),
        "{chunks:?} {audio:?}"
    );

    let fake_pdf = f
        .store
        .library_ingest(
            &g,
            IngestRequest {
                media_type: "application/pdf".into(),
                ..req(b"not a pdf", None)
            },
            NOW,
        )
        .await;
    assert!(matches!(fake_pdf, Err(Error::Rejected(_))));
    let video = f
        .store
        .library_ingest(
            &g,
            IngestRequest {
                media_type: "video/mp4".into(),
                ..req(b"....", None)
            },
            NOW,
        )
        .await;
    assert!(matches!(video, Err(Error::Unsupported(_))));
    let no_vision = f
        .store
        .library_ingest(
            &g,
            IngestRequest {
                media_type: "image/png".into(),
                ..req(b"\x89PNG....", None)
            },
            NOW,
        )
        .await;
    assert!(matches!(no_vision, Err(Error::Unavailable(_))));
}

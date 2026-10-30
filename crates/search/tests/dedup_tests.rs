//! Exact and near-duplicate verification (section 15.11).

use excalibur_search::dedup::{content_hash, hamming_distance, is_near_duplicate, simhash64};
use excalibur_search::tokenize;

fn tokens(text: &str) -> Vec<String> {
    tokenize(text)
}

#[test]
fn exact_duplicates_share_content_hash() {
    let first = content_hash("# Title\n\nBody text here\n");
    let second = content_hash("# Title\n\nBody text here   \n\n\n");
    assert_eq!(first, second);
}

#[test]
fn different_pages_have_different_hashes() {
    assert_ne!(
        content_hash("alpha beta gamma"),
        content_hash("alpha beta delta page about rust")
    );
}

#[test]
fn near_identical_pages_are_near_duplicates() {
    let base = tokens(&"the quick brown fox jumps over the lazy dog near the river bank".repeat(4));
    let mut altered = base.clone();
    altered.push("extra".to_owned());
    let left = simhash64(&base);
    let right = simhash64(&altered);
    assert!(
        is_near_duplicate(left, right),
        "distance={}",
        hamming_distance(left, right)
    );
}

#[test]
fn unrelated_pages_are_not_near_duplicates() {
    let left = simhash64(&tokens(
        &"rust ownership borrow checker lifetimes traits generics macros".repeat(4),
    ));
    let right = simhash64(&tokens(
        &"paella valenciana receta arroz azafran conejo garrofo fuego lena".repeat(4),
    ));
    assert!(
        !is_near_duplicate(left, right),
        "distance={}",
        hamming_distance(left, right)
    );
}

#[test]
fn hamming_distance_counts_bits() {
    assert_eq!(hamming_distance(0, 0), 0);
    assert_eq!(hamming_distance(0, 0xffff_ffff_ffff_ffff), 64);
    assert_eq!(hamming_distance(0b1010, 0b0101), 4);
}

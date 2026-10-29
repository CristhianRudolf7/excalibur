use excalibur_context::*;
#[test]
fn local_bpe_is_real_encoding_scoped_and_treats_special_tokens_as_literal_data() {
    let cl = TokenCounter::for_model("gpt-4");
    assert_eq!(cl.encoding, Some(TokenEncoding::Cl100kBase));
    assert_eq!(cl.count_text("hello world"), 2);
    let o = TokenCounter::for_model("gpt-4o");
    assert_eq!(o.encoding, Some(TokenEncoding::O200kBase));
    assert_eq!(o.count_text("hello world"), 2);
    assert_eq!(
        TokenCounter::for_model("text-davinci-003").encoding,
        Some(TokenEncoding::P50kBase)
    );
    assert_eq!(cl.count_text("お誕生日おめでとう"), 9);
    assert_eq!(o.count_text("お誕生日おめでとう"), 8);
    let unknown = TokenCounter::for_model("unverified-model-name");
    assert!(unknown.encoding.is_none());
    let text = "Literal <|endoftext|> is untrusted data. 日本語 España 🦀";
    assert!(cl.count_text(text) > 2);
    assert_eq!(
        cl.count_text(text),
        tiktoken_rs::cl100k_base_singleton()
            .encode_ordinary(text)
            .len()
    );
    assert_eq!(
        cl,
        serde_json::from_slice::<TokenCounter>(&serde_json::to_vec(&cl).unwrap()).unwrap()
    );
}
#[test]
fn calibration_uses_only_actual_usage_is_per_model_recoverable_and_converges_within_margin() {
    let input = ContextInput::new("Synthetic core", "Synthetic literal request", "fixed");
    let request = compile(&input).unwrap().request;
    let mut counter = TokenCounter::for_model("synthetic-calibration-model");
    let untouched = TokenCounter::for_model("another-model");
    let base = counter.count_request(&request);
    assert!(!counter.observe(&request, (base * 2) as u64, true));
    assert!(!counter.observe(&request, 0, false));
    assert_eq!(counter.samples, 0);
    for index in 0..20 {
        let actual = (base * 2 + index % 3) as u64;
        assert!(counter.observe(&request, actual, false));
        let estimated = counter.count_request(&request);
        assert!(estimated.abs_diff(actual as usize) * 100 <= actual as usize * 10);
    }
    assert_eq!(counter.samples, 20);
    assert_eq!(untouched.samples, 0);
    let restored: TokenCounter =
        serde_json::from_slice(&serde_json::to_vec(&counter).unwrap()).unwrap();
    assert_eq!(
        restored.count_request(&request),
        counter.count_request(&request)
    );
    assert_eq!(TokenCounter::with_margin(101), 112);
}
#[test]
fn threshold_formula_caps_only_after_fraction_and_output_reserve() {
    let policy = SessionPolicy {
        model_window: 1_048_576,
        output_reserve: 32_768,
        ..Default::default()
    };
    assert_eq!(policy.compaction_threshold(), 500_000);
    let above_ceiling = SessionPolicy {
        compaction_cap: 2_000_000,
        ..policy.clone()
    };
    assert_eq!(above_ceiling.compaction_threshold(), 500_000);
    let policy = SessionPolicy {
        model_window: 128_000,
        output_reserve: 8_000,
        ..Default::default()
    };
    assert_eq!(policy.compaction_threshold(), 100_800);
}

#[test]
fn managed_layer_limits_use_local_bpe_without_a_character_estimate_gate() {
    let mut input = ContextInput::new(
        format!("{}Rule.", " ".repeat(1024)),
        "Literal task",
        "1700000000",
    );
    input.token_budgets[0] = 32;
    assert!(matches!(
        compile(&input),
        Err(ContextError::Budget { layer: 0, .. })
    ));
    let policy = SessionPolicy {
        counter: TokenCounter::for_model("gpt-4o"),
        ..Default::default()
    };
    let mut session = ContextSession::new(input.clone(), policy).unwrap();
    let prepared = session
        .prepare(
            input.volatile,
            0,
            &StructuredState::default(),
            SummaryOutcome::NotRequested,
        )
        .unwrap();
    assert!(TokenCounter::with_margin(prepared.compiled.estimated_tokens[0]) <= 32);
    assert_eq!(
        prepared.compiled.request.messages[0].content,
        format!("[L0]\n{}", input.layers[0])
    );
}

//! Planner classification on a table of English and Turkish queries,
//! glossary expansion and the classifier hook.

use knowell_query::{
    Component, Glossary, GlossaryEntry, HttpMethod, Intent, IntentClassifier, IntentDecision,
    PlanOptions, QueryPlan, SourceError, TermKind, TermRelation, TermStatus, plan, plan_with,
};

use crate::common::name;

use Intent::{Behavior, Endpoint, ErrorTrace, ExactSymbol, Impact, PathOrFile, Why};
use TermKind::{ErrorCode, ErrorType, Identifier, Path, QualifiedName, Route};

type Row = (&'static str, Intent, &'static [(TermKind, &'static str)]);

const ENGLISH: &[Row] = &[
    (
        "PaymentService.cancelSubscription",
        ExactSymbol,
        &[(QualifiedName, "PaymentService.cancelSubscription")],
    ),
    (
        "where is cancel_subscription defined?",
        ExactSymbol,
        &[(Identifier, "cancel_subscription")],
    ),
    ("Foo::bar()", ExactSymbol, &[(QualifiedName, "Foo::bar")]),
    ("a.b.c", ExactSymbol, &[(QualifiedName, "a.b.c")]),
    (
        "billing.subscriptions.cancel",
        ExactSymbol,
        &[(QualifiedName, "billing.subscriptions.cancel")],
    ),
    (
        "MAX_RETRY_COUNT",
        ExactSymbol,
        &[(Identifier, "MAX_RETRY_COUNT")],
    ),
    ("checkout", ExactSymbol, &[]),
    ("`retry` logic", ExactSymbol, &[(Identifier, "retry")]),
    (
        "src/billing/subscription.service.ts",
        PathOrFile,
        &[(Path, "src/billing/subscription.service.ts")],
    ),
    ("what is in Cargo.toml", PathOrFile, &[(Path, "Cargo.toml")]),
    (
        "POST /v1/subscriptions/{id}/cancel",
        Endpoint,
        &[(Route, "/v1/subscriptions/{id}/cancel")],
    ),
    (
        "https://api.example.test/v1/users?id=3",
        Endpoint,
        &[(Route, "/v1/users")],
    ),
    (
        "TypeError: Cannot read properties of undefined (reading 'id')",
        ErrorTrace,
        &[(ErrorType, "TypeError")],
    ),
    (
        "Error: boom\n    at charge (src/payment.ts:42:7)\n    at main (src/index.ts:3:1)",
        ErrorTrace,
        &[(Path, "src/payment.ts"), (Path, "src/index.ts")],
    ),
    (
        "thread 'main' panicked at src/main.rs:2:5:",
        ErrorTrace,
        &[(Path, "src/main.rs")],
    ),
    (
        "error[E0308]: mismatched types",
        ErrorTrace,
        &[(ErrorCode, "E0308")],
    ),
    (
        "ECONNREFUSED when calling the ledger",
        ErrorTrace,
        &[(ErrorCode, "ECONNREFUSED")],
    ),
    (
        "NullPointerException in InvoiceMapper",
        ErrorTrace,
        &[
            (ErrorType, "NullPointerException"),
            (Identifier, "InvoiceMapper"),
        ],
    ),
    (
        "Where do we prevent the same payment being processed twice?",
        Behavior,
        &[],
    ),
    ("how is the invoice total calculated", Behavior, &[]),
    (
        "who calls cancelSubscription",
        Impact,
        &[(Identifier, "cancelSubscription")],
    ),
    (
        "what breaks if I remove the amount field from the refund API",
        Impact,
        &[],
    ),
    (
        "where is PaymentGateway used",
        Impact,
        &[(Identifier, "PaymentGateway")],
    ),
    ("why do we retry webhooks three times", Why, &[]),
    ("Node.js e.g. the retry policy", Behavior, &[]),
];

const TURKISH: &[Row] = &[
    (
        "Ödeme iki kez işlenmesini nerede engelliyoruz?",
        Behavior,
        &[],
    ),
    ("faturada KDV nasıl hesaplanıyor", Behavior, &[]),
    ("ödeme", Behavior, &[]),
    ("Abonelik iptali neden bu şekilde tasarlandı?", Why, &[]),
    ("iade akışının tarihçesi", Why, &[]),
    (
        "PaymentService'i kim kullanıyor?",
        Impact,
        &[(Identifier, "PaymentService")],
    ),
    ("bu endpoint'i değiştirirsem ne bozulur", Impact, &[]),
    (
        "src/odeme/servis.ts dosyası",
        PathOrFile,
        &[(Path, "src/odeme/servis.ts")],
    ),
    ("ödeme servisinde hata alıyorum", ErrorTrace, &[]),
    ("bu hataya neden olan kod", ErrorTrace, &[]),
    (
        "GET /api/v1/faturalar/:id ne döndürüyor",
        Endpoint,
        &[(Route, "/api/v1/faturalar/:id")],
    ),
    (
        "refund_amount alanı nerede tanımlı",
        ExactSymbol,
        &[(Identifier, "refund_amount")],
    ),
];

fn check(rows: &[Row]) {
    let glossary = Glossary::default();
    for (query, intent, terms) in rows {
        let p = plan(query, &glossary);
        assert_eq!(
            p.intent, *intent,
            "query {query:?}: signals {:#?}",
            p.signals
        );
        assert_eq!(p.decided_by, IntentDecision::Rules);
        for (kind, text) in *terms {
            assert!(
                p.exact_terms
                    .iter()
                    .any(|t| t.kind == *kind && t.text == *text),
                "query {query:?}: missing {kind:?} {text:?} in {:?}",
                p.exact_terms
            );
        }
        // The chosen intent is always explained by a signal, except the
        // documented default.
        let explained = p.signals.iter().any(|s| s.intent == p.intent);
        assert!(explained || p.intent == Behavior, "query {query:?}");
        assert!(!p.secondary.contains(&p.intent), "query {query:?}");
    }
}

#[test]
fn classifies_english_queries() {
    check(ENGLISH);
}

#[test]
fn classifies_turkish_queries() {
    check(TURKISH);
}

#[test]
fn extracts_method_and_trace_frames() {
    let g = Glossary::default();
    let p = plan("POST /v1/subscriptions/{id}/cancel", &g);
    assert_eq!(p.http_method, Some(HttpMethod::Post));

    let trace = "Traceback (most recent call last):\n  File \"app/billing.py\", line 12, in charge\n    total = order.amount / 0\nZeroDivisionError: division by zero";
    let p = plan(trace, &g);
    assert_eq!(p.intent, ErrorTrace);
    let frame = &p.trace_frames[0];
    assert_eq!(frame.path, "app/billing.py");
    assert_eq!(frame.line, Some(12));
    assert_eq!(frame.symbol.as_deref(), Some("charge"));
    assert!(p.terms_of(ErrorType).any(|t| t == "ZeroDivisionError"));

    let js = "Error: boom\n    at charge (src/payment.ts:42:7)\n    at Object.<anonymous> (src/index.ts:3:1)";
    let p = plan(js, &g);
    assert_eq!(p.trace_frames.len(), 2);
    assert_eq!(p.trace_frames[0].symbol.as_deref(), Some("charge"));
    assert_eq!(p.trace_frames[0].column, Some(7));

    let java = "Exception in thread \"main\" java.lang.IllegalStateException: closed\n\tat com.example.billing.Ledger.post(Ledger.java:88)";
    let p = plan(java, &g);
    assert_eq!(p.intent, ErrorTrace);
    assert_eq!(p.trace_frames[0].path, "Ledger.java");
    assert_eq!(p.trace_frames[0].line, Some(88));
    assert!(
        p.terms_of(ErrorType)
            .any(|t| t == "java.lang.IllegalStateException")
    );
}

#[test]
fn words_drop_stopwords_in_both_languages() {
    let g = Glossary::default();
    let p = plan("where do we validate the refund amount", &g);
    assert_eq!(p.words, ["validate", "refund", "amount"]);
    let p = plan("iade tutarı nerede ve nasıl doğrulanıyor", &g);
    assert_eq!(p.words, ["iade", "tutarı", "doğrulanıyor"]);
}

fn glossary() -> Glossary {
    Glossary::new(vec![
        GlossaryEntry::approved("ödeme", "payment", TermRelation::Translation),
        GlossaryEntry::approved("abonelik", "subscription", TermRelation::Translation),
        GlossaryEntry::approved("kdv", "vat", TermRelation::Abbreviation),
        GlossaryEntry::suggested("tahsilat", "charge", TermRelation::Translation),
        GlossaryEntry::approved("üye", "member", TermRelation::CodeName).in_domain(name("crm")),
    ])
    .unwrap()
}

#[test]
fn approved_glossary_terms_expand_suggested_do_not() {
    let p = plan("Ödemenin tahsilat adımı nerede", &glossary());
    let applied: Vec<&str> = p.expansions.iter().map(|e| e.expansion.as_str()).collect();
    assert_eq!(applied, ["payment"]);
    assert!(p.expansions[0].inflected);
    let suggested: Vec<&str> = p.suggestions.iter().map(|e| e.expansion.as_str()).collect();
    assert_eq!(suggested, ["charge"]);
    assert_eq!(p.suggestions[0].status, TermStatus::Suggested);
    assert!(p.lexical_terms().contains(&"payment".to_owned()));
    assert!(!p.lexical_terms().contains(&"charge".to_owned()));
    assert_eq!(
        p.semantic_text(),
        "Ödemenin tahsilat adımı nerede (payment)"
    );

    let options = PlanOptions {
        include_suggested: true,
        domain: None,
    };
    let p = plan_with(
        "Ödemenin tahsilat adımı nerede",
        &glossary(),
        &options,
        None,
    );
    let applied: Vec<&str> = p.expansions.iter().map(|e| e.expansion.as_str()).collect();
    assert_eq!(applied, ["payment", "charge"]);
    assert!(p.suggestions.is_empty());
}

#[test]
fn domain_entries_apply_only_in_their_domain() {
    let p = plan("üye kaydı", &glossary());
    assert!(p.expansions.is_empty());
    let options = PlanOptions {
        include_suggested: false,
        domain: Some(name("crm")),
    };
    let p = plan_with("üye kaydı", &glossary(), &options, None);
    assert_eq!(p.expansions[0].expansion, "member");
    assert_eq!(p.domain, Some(name("crm")));
}

#[test]
fn expansions_skip_terms_already_in_the_query() {
    let p = plan("ödeme payment flow", &glossary());
    assert!(p.expansions.is_empty());
}

struct Fixed(Result<Option<Intent>, SourceError>);

impl IntentClassifier for Fixed {
    fn id(&self) -> String {
        "fixed@1".into()
    }

    fn classify(&self, _query: &str, _rules: &QueryPlan) -> Result<Option<Intent>, SourceError> {
        self.0.clone()
    }
}

#[test]
fn classifier_hook_can_override_abstain_or_fail() {
    let g = Glossary::default();
    let options = PlanOptions::default();

    let p = plan_with("checkout", &g, &options, Some(&Fixed(Ok(Some(Behavior)))));
    assert_eq!(p.intent, Behavior);
    assert_eq!(
        p.decided_by,
        IntentDecision::Classifier {
            classifier: "fixed@1".into(),
            rule_intent: ExactSymbol,
        }
    );
    assert_eq!(p.secondary.first(), Some(&ExactSymbol));

    let p = plan_with("checkout", &g, &options, Some(&Fixed(Ok(None))));
    assert_eq!(p.intent, ExactSymbol);
    assert_eq!(p.decided_by, IntentDecision::Rules);

    let failing = Fixed(Err(SourceError::Unavailable("timeout after 300 ms".into())));
    let p = plan_with("checkout", &g, &options, Some(&failing));
    assert_eq!(p.intent, ExactSymbol);
    assert_eq!(p.degraded[0].component, Component::Classifier);
    assert_eq!(
        p.degraded[0].to_string(),
        "classifier: timeout after 300 ms"
    );
}

#[test]
fn planning_is_deterministic_and_serialisable() {
    let query = "PaymentService'i kim kullanıyor? src/a.ts POST /x";
    let a = plan(query, &glossary());
    let b = plan(query, &glossary());
    assert_eq!(a, b);
    let json = serde_json::to_string(&a).unwrap();
    let back: QueryPlan = serde_json::from_str(&json).unwrap();
    assert_eq!(back, a);
}

#[test]
fn malformed_and_hostile_queries_do_not_panic() {
    let g = glossary();
    let inputs = [
        String::new(),
        "   \n\t ".to_owned(),
        "`".to_owned(),
        "\"unterminated".to_owned(),
        "at (".to_owned(),
        "File \"".to_owned(),
        "error[E".to_owned(),
        "#1".to_owned(),
        "GET".to_owned(),
        "::".to_owned(),
        "a..b".to_owned(),
        "x:99999999999999999999".to_owned(),
        "ö'ğ'ü".to_owned(),
        "\u{0}\u{1}\u{202E}".to_owned(),
        "(".repeat(10_000),
        ")".repeat(10_000),
        "\u{201C}".repeat(16_000),
        "\"".repeat(9_999),
        "a/".repeat(5_000),
        "ödeme ".repeat(5_000),
    ];
    for input in inputs {
        let p = plan(&input, &g);
        assert!(p.exact_terms.len() <= 64);
        assert!(p.words.len() <= 128);
    }
}

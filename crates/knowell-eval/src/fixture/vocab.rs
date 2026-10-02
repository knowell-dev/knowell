//! Vocabulary for generated noise files.
//!
//! Noise must look like ordinary product code around the core system,
//! including distractor words ("subscription", "cancel", "retry", "dedupe",
//! Turkish comments) so that lexical matching is not trivially perfect. It
//! must never implement behaviour that an `absent` query asks about (PDF
//! documents, SMS, two-factor login, loyalty points, crypto payments, gift
//! cards, invoices); a test enforces that.

/// A product area with its entities and a few realistic comment sentences.
pub(super) struct Topic {
    pub(super) module: &'static str,
    pub(super) entities: &'static [&'static str],
    pub(super) notes: &'static [&'static str],
}

pub(super) const TOPICS: &[Topic] = &[
    Topic {
        module: "catalog",
        entities: &["product", "category", "variant", "attribute", "brand"],
        notes: &[
            "Variants inherit attributes from their product unless they override them.",
            "Category trees are cached for ten minutes; editors see changes after the next refresh.",
            "Archived products stay addressable by SKU so that old orders still render.",
        ],
    },
    Topic {
        module: "inventory",
        entities: &["stock_level", "reservation", "location", "adjustment"],
        notes: &[
            "Reservations expire after thirty minutes when checkout is abandoned.",
            "Negative stock is allowed for backorderable items only.",
            "Adjustments are append-only; the current level is the sum of all rows.",
        ],
    },
    Topic {
        module: "shipping",
        entities: &["shipment", "carrier", "label", "rate", "parcel"],
        notes: &[
            "Carrier calls are retried with exponential backoff because the sandbox often times out.",
            "Rates are quoted in minor units and include VAT.",
            "Replenishment boxes ship on the member's renewal day.",
        ],
    },
    Topic {
        module: "coupons",
        entities: &["coupon", "promotion", "redemption", "campaign"],
        notes: &[
            "A coupon code can be redeemed once per customer; repeated redemptions are rejected.",
            "Promotions never stack with member prices.",
            "Campaign windows are evaluated in the shop's time zone.",
        ],
    },
    Topic {
        module: "reviews",
        entities: &["review", "rating", "moderation_case", "photo"],
        notes: &[
            "Ratings are averaged per variant and rounded to one decimal.",
            "Reviews that mention order numbers are held for moderation.",
            "Photos are scanned before they become visible.",
        ],
    },
    Topic {
        module: "analytics",
        entities: &["metric", "funnel", "cohort", "dashboard", "tracking_event"],
        notes: &[
            "Counts subscription cancellations per cohort for the churn dashboard.",
            "Cancel reasons are bucketed weekly; free-text feedback is never exported.",
            "Tracking events are deduplicated by client event id before aggregation.",
            "Payment capture success rate is computed from the daily ledger snapshot.",
        ],
    },
    Topic {
        module: "search",
        entities: &["search_index", "synonym", "facet", "suggestion"],
        notes: &[
            "Synonyms are maintained in English and Turkish.",
            "Facets are computed from the in-stock subset only.",
            "The index is rebuilt nightly and patched incrementally during the day.",
        ],
    },
    Topic {
        module: "recommendations",
        entities: &["recommendation", "similarity", "bundle_hint"],
        notes: &[
            "Recommendations exclude items the customer returned.",
            "Members see replenishment suggestions based on their last three boxes.",
        ],
    },
    Topic {
        module: "wishlist",
        entities: &["wishlist", "wishlist_item", "share_link"],
        notes: &[
            "Share links expire after ninety days.",
            "Price drop alerts are sent at most once a week per item.",
        ],
    },
    Topic {
        module: "returns",
        entities: &["return_request", "return_label", "inspection"],
        notes: &[
            "Return windows are thirty days from delivery.",
            "Refund amounts for returns are calculated by the ledger, not here.",
        ],
    },
    Topic {
        module: "warehouse",
        entities: &["pick_list", "bin", "wave", "packing_slip"],
        notes: &[
            "Waves are released every fifteen minutes during opening hours.",
            "Packing slips are printed in the language of the shipping country.",
        ],
    },
    Topic {
        module: "pricing",
        entities: &["price_list", "member_price", "price_rule", "currency_rate"],
        notes: &[
            "Member prices apply while the membership is active or inside the paid period after cancelling.",
            "Currency rates are refreshed every morning from the treasury feed.",
            "Prices are stored in minor units; never use floating point.",
        ],
    },
    Topic {
        module: "tax",
        entities: &["tax_rate", "tax_region", "exemption"],
        notes: &[
            "Tax rates are looked up by shipping region, not billing region.",
            "Exemptions require a validated business id.",
        ],
    },
    Topic {
        module: "banners",
        entities: &["banner", "placement", "slot"],
        notes: &[
            "Banners are scheduled in the shop's time zone.",
            "Only one hero banner may be active per placement.",
        ],
    },
    Topic {
        module: "content",
        entities: &["page", "article", "faq_entry", "snippet"],
        notes: &[
            "Content is authored in English and translated to Turkish by the localisation team.",
            "Draft pages are visible only with a preview token.",
        ],
    },
    Topic {
        module: "reports",
        entities: &["sales_report", "export_job", "kpi"],
        notes: &[
            "Exports are written as CSV and kept for thirty days.",
            "Report queries run against the read replica.",
        ],
    },
    Topic {
        module: "addresses",
        entities: &["address", "postal_code", "geo_point"],
        notes: &[
            "Postal codes are validated per country before saving.",
            "Addresses are never hard-deleted while an order references them.",
        ],
    },
    Topic {
        module: "carts",
        entities: &["cart", "cart_line", "saved_cart"],
        notes: &[
            "Abandoned carts get one reminder e-mail after twenty-four hours.",
            "Cart lines keep the price seen by the customer for one hour.",
        ],
    },
    Topic {
        module: "suppliers",
        entities: &["supplier", "purchase_order", "delivery_slot"],
        notes: &[
            "Purchase orders are sent to suppliers every weekday at noon.",
            "Delivery slots are confirmed through the supplier portal.",
        ],
    },
    Topic {
        module: "bundles",
        entities: &["bundle", "bundle_component"],
        notes: &["A bundle is in stock only when every component is in stock."],
    },
    Topic {
        module: "media",
        entities: &["image", "rendition", "upload"],
        notes: &[
            "Renditions are generated lazily on first request.",
            "Uploads larger than ten megabytes are rejected.",
        ],
    },
    Topic {
        module: "seo",
        entities: &["sitemap", "redirect", "meta_tag"],
        notes: &[
            "Sitemaps are split into files of at most fifty thousand URLs.",
            "Redirect chains longer than two hops are flattened nightly.",
        ],
    },
    Topic {
        module: "feature_flags",
        entities: &["flag", "rollout", "segment"],
        notes: &[
            "Rollouts are sticky per customer id.",
            "Flags older than ninety days are reported for cleanup.",
        ],
    },
    Topic {
        module: "audit",
        entities: &["audit_entry", "retention_policy"],
        notes: &["Audit entries are immutable and kept for seven years."],
    },
    Topic {
        module: "support",
        entities: &["ticket", "macro", "satisfaction_survey"],
        notes: &[
            "Agents can see the membership status but cannot cancel on a member's behalf.",
            "Tickets are auto-closed after seven days without a reply.",
        ],
    },
];

/// Generic English remarks used as secondary comments.
pub(super) const GENERIC_NOTES: &[&str] = &[
    "Keep this in sync with the admin screens.",
    "Batch size is tuned for the nightly window.",
    "Values are cached for five minutes.",
    "Callers must handle an empty result.",
    "Runs inside the request transaction.",
    "This path is hot; avoid extra queries here.",
    "Protected by a feature flag; see the rollout plan.",
    "Timestamps are stored in UTC.",
    "Safe to run again: the operation is repeatable.",
    "Do not log customer e-mail addresses here.",
    "Sorted newest first to match the UI.",
    "Old rows without a value are treated as zero.",
];

/// Turkish remarks: part of the team writes comments in Turkish.
pub(super) const TURKISH_NOTES: &[&str] = &[
    "Kampanya bitince bu kontrol kaldırılacak.",
    "Stok sıfırın altına düşerse depo ekibine uyarı gider.",
    "Bu alan eski mobil sürümler için tutuluyor.",
    "Fiyatlar kuruş cinsinden saklanır.",
    "Abonelik iptal oranı haftalık raporda gösterilir.",
    "Yorumlarda puan ortalaması varyant bazında hesaplanır.",
    "Kargo firması zaman aşımına uğrarsa istek tekrar denenir.",
    "Bu sorgu gece çalışan raporlar için optimize edildi.",
    "Toplu işlem boyutu gece penceresine göre ayarlandı.",
];

/// String-valued fields (snake_case).
pub(super) const TEXT_FIELDS: &[&str] = &[
    "name",
    "status",
    "sku",
    "region",
    "locale",
    "notes",
    "external_ref",
    "channel",
    "currency",
    "owner_id",
    "title",
    "source",
];

/// Integer-valued fields (snake_case).
pub(super) const NUMBER_FIELDS: &[&str] = &[
    "quantity",
    "price_minor",
    "priority",
    "score",
    "position",
    "version",
    "weight_grams",
    "batch_size",
    "retry_count",
    "sort_order",
];

/// Verbs with their past tense (for event names).
pub(super) const VERBS: &[(&str, &str)] = &[
    ("list", "listed"),
    ("get", "fetched"),
    ("create", "created"),
    ("update", "updated"),
    ("archive", "archived"),
    ("sync", "synced"),
    ("validate", "validated"),
    ("compute", "computed"),
    ("refresh", "refreshed"),
    ("import", "imported"),
    ("publish", "published"),
    ("schedule", "scheduled"),
    ("merge", "merged"),
    ("reconcile", "reconciled"),
    ("aggregate", "aggregated"),
    ("normalize", "normalized"),
    ("resolve", "resolved"),
    ("export", "exported"),
    ("cancel", "cancelled"),
    ("retry", "retried"),
    ("prune", "pruned"),
    ("rebuild", "rebuilt"),
];

/// Fictional first names for meeting notes.
pub(super) const PEOPLE: &[&str] = &[
    "Ada", "Bora", "Cem", "Defne", "Elif", "Jonas", "Lena", "Mara", "Noah", "Omar", "Selin",
    "Theo", "Yusuf", "Zeynep",
];

/// Discussion points for meeting notes; `{{...}}` placeholders are filled
/// from the file's variables.
pub(super) const DISCUSSIONS: &[&str] = &[
    "Whether {{entities_words}} should be cached per region.",
    "The {{field_words}} column is still empty for rows imported last year.",
    "Support asked for a filter by {{field_words}} in the admin list.",
    "Nightly {{verb}} job takes too long on Mondays.",
    "Moving {{entities_words}} behind the {{module_words}} API instead of reading the table directly.",
    "Error rate of the {{verb}} step after the last release.",
    "Bu iş için ayrı bir kuyruk gerekip gerekmediği konuşuldu.",
];

/// Decisions for meeting notes.
pub(super) const DECISIONS: &[&str] = &[
    "Keep the current behaviour and add a dashboard panel.",
    "Add an index on {{field_words}} before the next campaign.",
    "Split the {{verb}} job into batches of {{n}}.",
    "Deprecate the old {{entities_words}} endpoint by the end of the quarter.",
    "No change; revisit after the peak season.",
];

/// Meeting note topics.
pub(super) const MEETING_TOPICS: &[&str] = &[
    "sync",
    "retro",
    "planning",
    "incident-review",
    "design-review",
    "kickoff",
];

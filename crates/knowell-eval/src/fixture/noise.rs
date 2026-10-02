//! Procedurally generated noise files.
//!
//! Every project gets plausible modules in its own language (catalog,
//! inventory, shipping, …) built from [`super::vocab`]. Noise exists to make
//! retrieval realistic: it adds volume, near-miss identifiers and distractor
//! words, but never carries the semantics of a core file, so it is never
//! listed as relevant by a query.
//!
//! Generation is a pure function of the random stream: paths are planned
//! first (cheap), checked against the paths already used, and only then is
//! the content rendered.

use std::collections::BTreeSet;

use super::vocab::{
    DECISIONS, DISCUSSIONS, GENERIC_NOTES, MEETING_TOPICS, NUMBER_FIELDS, PEOPLE, TEXT_FIELDS,
    TOPICS, TURKISH_NOTES, Topic, VERBS,
};
use crate::rng::SplitMix64;

/// The kind of noise a project receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoiseStyle {
    /// React components and hooks (TypeScript).
    React,
    /// NestJS-style injectable services (TypeScript).
    Nest,
    /// Flutter repositories and controllers (Dart).
    Dart,
    /// Go packages.
    Go,
    /// Python modules and pytest files.
    Python,
    /// Rust job modules.
    Rust,
    /// Protobuf files and JSON event schemas.
    Contracts,
    /// SQL migrations.
    Sql,
    /// Kubernetes manifests.
    Kubernetes,
    /// Meeting notes and runbooks.
    Markdown,
}

/// Number of planning attempts before a colliding path gets a `-vN` suffix.
const PLAN_ATTEMPTS: usize = 12;

/// First sequence number of generated SQL migrations (core uses 0001–0099).
const FIRST_NOISE_MIGRATION: usize = 100;

/// Generates `count` noise files as `(path, content)`, avoiding every path in
/// `used` and adding the new paths to it.
pub(crate) fn generate(
    style: NoiseStyle,
    rng: &mut SplitMix64,
    count: usize,
    used: &mut BTreeSet<String>,
) -> Vec<(String, String)> {
    let mut files = Vec::with_capacity(count);
    for index in 0..count {
        let mut candidate = plan(style, rng, index);
        let mut attempts = 1;
        while used.contains(&candidate.path()) && attempts < PLAN_ATTEMPTS {
            candidate = plan(style, rng, index);
            attempts += 1;
        }
        if used.contains(&candidate.path()) {
            // Small vocabularies run out of names at large scales; real
            // repositories have such versioned siblings too.
            let base = candidate.stem.clone();
            let mut n = 2usize;
            loop {
                candidate.stem = format!("{base}-v{n}");
                if !used.contains(&candidate.path()) {
                    break;
                }
                n += 1;
            }
        }
        let path = candidate.path();
        used.insert(path.clone());
        let content = render_plan(rng, &mut candidate);
        files.push((path, content));
    }
    files
}

/// Named template variables of one file.
#[derive(Debug, Clone, Default)]
struct Vars(Vec<(&'static str, String)>);

impl Vars {
    fn set(&mut self, key: &'static str, value: impl Into<String>) {
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Copy)]
enum Template {
    ReactComponent,
    ReactHook,
    Nest,
    Dart,
    Go,
    Python,
    PythonTest,
    Rust,
    Proto,
    EventSchema,
    SqlCreate,
    SqlAdd,
    SqlIndex,
    SqlBackfill,
    K8sDeployment,
    K8sCronJob,
    K8sConfigMap,
    K8sAutoscaler,
    MarkdownNote,
    MarkdownRunbook,
}

#[derive(Debug, Clone)]
struct Plan {
    dir: String,
    stem: String,
    ext: &'static str,
    template: Template,
    vars: Vars,
}

impl Plan {
    fn path(&self) -> String {
        format!("{}/{}.{}", self.dir, self.stem, self.ext)
    }
}

// ---------------------------------------------------------------------------
// Planning: choose topic, entity, names and the path.
// ---------------------------------------------------------------------------

const REACT_AREAS: &[&str] = &[
    "account",
    "admin",
    "storefront",
    "internal",
    "experiments",
    "legacy",
    "partners",
    "mobile-web",
];
const REACT_COMPONENTS: &[&str] = &[
    "List", "Card", "Panel", "Table", "Form", "Summary", "Badge", "Editor", "Picker", "Drawer",
];
const REACT_HOOKS: &[&str] = &["Query", "Filters", "Selection", "Sorting", "Draft"];
const NEST_AREAS: &[&str] = &[
    "core",
    "admin",
    "jobs",
    "partners",
    "legacy",
    "v2",
    "internal",
    "reporting",
];
const NEST_KINDS: &[&str] = &[
    "service",
    "repository",
    "mapper",
    "policy",
    "scheduler",
    "validator",
];
const DART_AREAS: &[&str] = &[
    "data",
    "domain",
    "presentation",
    "widgets",
    "legacy",
    "experiments",
];
const DART_KINDS: &[&str] = &[
    "repository",
    "controller",
    "view_model",
    "cache",
    "formatter",
    "service",
];
const GO_AREAS: &[&str] = &["core", "admin", "jobs", "legacy", "sync", "v2"];
const GO_KINDS: &[(&str, &str)] = &[
    ("", "Service"),
    ("_repo", "Repo"),
    ("_handler", "Handler"),
    ("_worker", "Worker"),
    ("_cache", "Cache"),
    ("_sync", "Syncer"),
];
const PYTHON_AREAS: &[&str] = &[
    "services",
    "repositories",
    "jobs",
    "schemas",
    "legacy",
    "views",
];
const PYTHON_KINDS: &[&str] = &[
    "service",
    "repository",
    "tasks",
    "rules",
    "builder",
    "policy",
];
const RUST_AREAS: &[&str] = &["nightly", "hourly", "adhoc", "backfill"];
const RUST_KINDS: &[&str] = &["job", "digest", "render", "schedule", "cleanup", "batch"];
const PROTO_KINDS: &[(&str, &str)] = &[
    ("service", ""),
    ("admin", "Admin"),
    ("internal", "Internal"),
];
const K8S_KINDS: &[&str] = &["deployment", "cronjob", "configmap", "hpa"];
const K8S_ENVS: &[&str] = &["dev", "staging", "prod"];

fn plan(style: NoiseStyle, rng: &mut SplitMix64, index: usize) -> Plan {
    let (topic, entity) = pick_topic(rng);
    let mut vars = base_vars(rng, topic, entity);
    let (dir, stem, ext, template) = match style {
        NoiseStyle::React => {
            let area = rng.pick(REACT_AREAS);
            let dir = format!("src/features/{}/{area}", kebab(topic.module));
            if rng.chance(70) {
                let variant = rng.pick(REACT_COMPONENTS);
                vars.set("Variant", variant);
                vars.set("variant_kebab", variant.to_ascii_lowercase());
                (
                    dir,
                    format!("{}{variant}", pascal(entity)),
                    "tsx",
                    Template::ReactComponent,
                )
            } else {
                let variant = rng.pick(REACT_HOOKS);
                vars.set("Variant", variant);
                (
                    dir,
                    format!("use{}{variant}", pascal(entity)),
                    "ts",
                    Template::ReactHook,
                )
            }
        }
        NoiseStyle::Nest => {
            let area = rng.pick(NEST_AREAS);
            let kind = rng.pick(NEST_KINDS);
            vars.set("Kind", pascal(kind));
            (
                format!("src/modules/{}/{area}", kebab(topic.module)),
                format!("{}.{kind}", kebab(entity)),
                "ts",
                Template::Nest,
            )
        }
        NoiseStyle::Dart => {
            let area = rng.pick(DART_AREAS);
            let kind = rng.pick(DART_KINDS);
            vars.set("Kind", pascal(kind));
            (
                format!("lib/features/{}/{area}", topic.module),
                format!("{entity}_{kind}"),
                "dart",
                Template::Dart,
            )
        }
        NoiseStyle::Go => {
            let area = rng.pick(GO_AREAS);
            let (file_suffix, kind) = rng.pick_ref(GO_KINDS).copied().unwrap_or(("", "Service"));
            vars.set("Kind", kind);
            vars.set("package", area);
            (
                format!("internal/{}/{area}", flat(topic.module)),
                format!("{entity}{file_suffix}"),
                "go",
                Template::Go,
            )
        }
        NoiseStyle::Python => {
            let area = rng.pick(PYTHON_AREAS);
            let kind = rng.pick(PYTHON_KINDS);
            vars.set("Kind", pascal(kind));
            vars.set("kind_snake", kind);
            vars.set("kind_words", kind);
            vars.set("area", area);
            if rng.chance(25) {
                (
                    format!("tests/{}", topic.module),
                    format!("test_{entity}_{kind}"),
                    "py",
                    Template::PythonTest,
                )
            } else {
                (
                    format!("ledger/{}/{area}", topic.module),
                    format!("{entity}_{kind}"),
                    "py",
                    Template::Python,
                )
            }
        }
        NoiseStyle::Rust => {
            let area = rng.pick(RUST_AREAS);
            let kind = rng.pick(RUST_KINDS);
            vars.set("Kind", pascal(kind));
            (
                format!("src/jobs/{}/{area}", topic.module),
                format!("{entity}_{kind}"),
                "rs",
                Template::Rust,
            )
        }
        NoiseStyle::Contracts => {
            let version = rng.range(1, 3).to_string();
            vars.set("version", version.as_str());
            if rng.chance(60) {
                let (kind, kind_pascal) = rng
                    .pick_ref(PROTO_KINDS)
                    .copied()
                    .unwrap_or(("service", ""));
                vars.set("Kind", kind_pascal);
                (
                    format!("proto/{}/v{version}", topic.module),
                    format!("{entity}_{kind}"),
                    "proto",
                    Template::Proto,
                )
            } else {
                let (_, past) = rng
                    .pick_ref(VERBS)
                    .copied()
                    .unwrap_or(("update", "updated"));
                vars.set("past", past);
                (
                    "events".to_owned(),
                    format!("{}.{entity}.{past}.v{version}", topic.module),
                    "json",
                    Template::EventSchema,
                )
            }
        }
        NoiseStyle::Sql => {
            let seq = FIRST_NOISE_MIGRATION + index;
            let table = plural(&qualified(topic.module, entity));
            let field = vars.get("field_snake").unwrap_or("name").to_owned();
            let field2 = vars.get("field2_snake").unwrap_or("position").to_owned();
            vars.set("table", table.as_str());
            let (stem, template) = match rng.below(4) {
                0 => (format!("{seq:04}_create_{table}"), Template::SqlCreate),
                1 => (format!("{seq:04}_add_{field}_to_{table}"), Template::SqlAdd),
                2 => (
                    format!("{seq:04}_index_{table}_{field2}"),
                    Template::SqlIndex,
                ),
                _ => (
                    format!("{seq:04}_backfill_{table}_{field}"),
                    Template::SqlBackfill,
                ),
            };
            ("migrations".to_owned(), stem, "sql", template)
        }
        NoiseStyle::Kubernetes => {
            let dir = if rng.chance(50) {
                format!("k8s/{}", kebab(topic.module))
            } else {
                format!(
                    "k8s/overlays/{}/{}",
                    rng.pick(K8S_ENVS),
                    kebab(topic.module)
                )
            };
            let kind = rng.pick(K8S_KINDS);
            let template = match kind {
                "deployment" => Template::K8sDeployment,
                "cronjob" => Template::K8sCronJob,
                "configmap" => Template::K8sConfigMap,
                _ => Template::K8sAutoscaler,
            };
            vars.set("kind", kind);
            (dir, format!("{}-{kind}", kebab(entity)), "yaml", template)
        }
        NoiseStyle::Markdown => {
            if rng.chance(75) {
                let topic_name = rng.pick(MEETING_TOPICS);
                let date = random_date(rng);
                vars.set("topic_words", topic_name.replace('-', " "));
                vars.set("date", date.as_str());
                (
                    format!("notes/{}", kebab(topic.module)),
                    format!("{date}-{}-{topic_name}", kebab(entity)),
                    "md",
                    Template::MarkdownNote,
                )
            } else {
                let (verb, _) = rng.pick_ref(VERBS).copied().unwrap_or(("sync", "synced"));
                (
                    format!("runbooks/{}", kebab(topic.module)),
                    format!("{}-{verb}-failures", kebab(entity)),
                    "md",
                    Template::MarkdownRunbook,
                )
            }
        }
    };
    Plan {
        dir,
        stem,
        ext,
        template,
        vars,
    }
}

fn pick_topic(rng: &mut SplitMix64) -> (&'static Topic, &'static str) {
    let index = rng.below(TOPICS.len());
    let Some(topic) = TOPICS.get(index) else {
        return (&EMPTY_TOPIC, "item");
    };
    let entity = rng.pick(topic.entities);
    (topic, if entity.is_empty() { "item" } else { entity })
}

/// Only reachable if `TOPICS` were empty; keeps generation total.
static EMPTY_TOPIC: Topic = Topic {
    module: "misc",
    entities: &["item"],
    notes: &["General purpose helpers."],
};

fn base_vars(rng: &mut SplitMix64, topic: &Topic, entity: &'static str) -> Vars {
    let mut vars = Vars::default();
    let entities = plural(entity);
    vars.set("Entity", pascal(entity));
    vars.set("entity", camel(entity));
    vars.set("entity_snake", entity);
    vars.set("entity_kebab", kebab(entity));
    vars.set("entity_words", words(entity));
    vars.set("ENTITY", entity.to_ascii_uppercase());
    vars.set("Entities", pascal(&entities));
    vars.set("entities_snake", entities.as_str());
    vars.set("entities_kebab", kebab(&entities));
    vars.set("entities_words", words(&entities));
    vars.set("module_snake", topic.module);
    vars.set("module_kebab", kebab(topic.module));
    vars.set("module_camel", camel(topic.module));
    vars.set("module_flat", flat(topic.module));
    vars.set("module_words", words(topic.module));
    vars.set("Module_words", capitalize(&words(topic.module)));
    vars.set("MODULE", topic.module.to_ascii_uppercase());
    let qualified_name = qualified(topic.module, entity);
    vars.set("qualified_kebab", kebab(&qualified_name));
    vars.set("QUALIFIED", qualified_name.to_ascii_uppercase());

    let field = rng.pick(TEXT_FIELDS);
    let field2 = rng.pick(NUMBER_FIELDS);
    vars.set("field", camel(field));
    vars.set("field_snake", field);
    vars.set("field_kebab", kebab(field));
    vars.set("field_words", words(field));
    vars.set("Field", pascal(field));
    vars.set("field2", camel(field2));
    vars.set("field2_snake", field2);
    vars.set("field2_words", words(field2));
    vars.set("Field2", pascal(field2));

    let note = if rng.chance(10) {
        rng.pick(TURKISH_NOTES)
    } else {
        rng.pick(topic.notes)
    };
    vars.set("note", note);
    vars.set("note2", rng.pick(GENERIC_NOTES));
    vars.set("n", (rng.range(1, 50) * 10).to_string());
    set_verb(
        &mut vars,
        rng.pick_ref(VERBS).copied().unwrap_or(("sync", "synced")),
    );
    vars
}

fn set_verb(vars: &mut Vars, (verb, past): (&str, &str)) {
    vars.set("verb", verb);
    vars.set("Verb", pascal(verb));
    vars.set("verb_kebab", verb);
    vars.set("verb_past", past);
}

fn random_date(rng: &mut SplitMix64) -> String {
    format!(
        "{}-{:02}-{:02}",
        rng.range(2024, 2026),
        rng.range(1, 12),
        rng.range(1, 28)
    )
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

fn render_plan(rng: &mut SplitMix64, plan: &mut Plan) -> String {
    let vars = &mut plan.vars;
    match plan.template {
        Template::ReactComponent => {
            let helpers = methods(
                rng,
                vars,
                &[TS_HELPER_SORT, TS_HELPER_POST],
                &[TS_HELPER_LABEL],
                1,
                3,
            );
            vars.set("helpers", helpers);
            render(REACT_COMPONENT, vars)
        }
        Template::ReactHook => render(REACT_HOOK, vars),
        Template::Nest => {
            let body = methods(
                rng,
                vars,
                &[NEST_METHOD_GET, NEST_METHOD_LIST],
                &[NEST_METHOD_SET],
                2,
                4,
            );
            vars.set("methods", body);
            render(NEST_CLASS, vars)
        }
        Template::Dart => {
            let body = methods(
                rng,
                vars,
                &[DART_METHOD_FETCH, DART_METHOD_POST],
                &[DART_METHOD_LOOKUP],
                2,
                3,
            );
            vars.set("methods", body);
            render(DART_CLASS, vars)
        }
        Template::Go => {
            let body = methods(
                rng,
                vars,
                &[GO_METHOD_ONE, GO_METHOD_MANY],
                &[GO_METHOD_SET],
                2,
                3,
            );
            vars.set("methods", body);
            let field = vars.get("Field").unwrap_or("Name").to_owned();
            let field2 = vars.get("Field2").unwrap_or("Version").to_owned();
            vars.set(
                "go_fields",
                go_struct_fields(&[
                    ("ID", "string"),
                    (&field, "string"),
                    (&field2, "int64"),
                    ("UpdatedAt", "time.Time"),
                ]),
            );
            render(GO_FILE, vars)
        }
        Template::Python => {
            let body = methods(
                rng,
                vars,
                &[PY_METHOD_GET, PY_METHOD_SELECT],
                &[PY_METHOD_SET],
                2,
                4,
            );
            vars.set("methods", body);
            render(PY_MODULE, vars)
        }
        Template::PythonTest => render(PY_TEST, vars),
        Template::Rust => {
            let body = methods(
                rng,
                vars,
                &[RS_METHOD_IDS, RS_METHOD_SUMMARY],
                &[RS_METHOD_SET],
                2,
                3,
            );
            vars.set("methods", body);
            render(RUST_FILE, vars)
        }
        Template::Proto => {
            let count = rng.range(1, 3);
            let mut rpcs = String::new();
            let mut messages = String::new();
            for verb in distinct_verbs(rng, count) {
                set_verb(vars, verb);
                rpcs.push_str(&render(PROTO_RPC, vars));
                messages.push_str(&render(PROTO_MESSAGES, vars));
            }
            vars.set("rpcs", rpcs);
            vars.set("messages", messages);
            render(PROTO_FILE, vars)
        }
        Template::EventSchema => render(EVENT_SCHEMA, vars),
        Template::SqlCreate => render(SQL_CREATE, vars),
        Template::SqlAdd => render(SQL_ADD, vars),
        Template::SqlIndex => render(SQL_INDEX, vars),
        Template::SqlBackfill => render(SQL_BACKFILL, vars),
        Template::K8sDeployment
        | Template::K8sCronJob
        | Template::K8sConfigMap
        | Template::K8sAutoscaler => {
            vars.set("replicas", rng.range(1, 4).to_string());
            vars.set("cpu", (rng.range(1, 8) * 25).to_string());
            vars.set("mem", (rng.range(1, 8) * 64).to_string());
            vars.set("minute", rng.below(60).to_string());
            vars.set("hour", rng.below(24).to_string());
            vars.set(
                "image_tag",
                format!("{}.{}.{}", rng.range(1, 4), rng.below(20), rng.below(10)),
            );
            let template = match plan.template {
                Template::K8sDeployment => K8S_DEPLOYMENT,
                Template::K8sCronJob => K8S_CRONJOB,
                Template::K8sConfigMap => K8S_CONFIGMAP,
                _ => K8S_AUTOSCALER,
            };
            render(template, vars)
        }
        Template::MarkdownNote => {
            let mut people: Vec<&str> = Vec::new();
            for _ in 0..rng.range(2, 4) {
                let person = rng.pick(PEOPLE);
                if !people.contains(&person) {
                    people.push(person);
                }
            }
            vars.set("people", people.join(", "));
            vars.set("person", people.first().copied().unwrap_or("Ada"));
            vars.set("date2", random_date(rng));
            let first = render(rng.pick(DISCUSSIONS), vars);
            let second = render(rng.pick(DISCUSSIONS), vars);
            let decision = render(rng.pick(DECISIONS), vars);
            vars.set("discussion1", first);
            vars.set("discussion2", second);
            vars.set("decision", decision);
            render(MARKDOWN_NOTE, vars)
        }
        Template::MarkdownRunbook => {
            vars.set("check", render(rng.pick(DISCUSSIONS), vars));
            render(MARKDOWN_RUNBOOK, vars)
        }
    }
}

/// Renders between `min` and `max` members. `repeatable` templates are
/// named after a verb (each member gets a distinct verb); `once` templates
/// have a fixed name and are used at most once per file.
fn methods(
    rng: &mut SplitMix64,
    vars: &mut Vars,
    repeatable: &[&str],
    once: &[&'static str],
    min: usize,
    max: usize,
) -> String {
    let count = rng.range(min, max);
    let mut unused_once: Vec<&str> = once.to_vec();
    let mut out = String::new();
    for verb in distinct_verbs(rng, count) {
        set_verb(vars, verb);
        vars.set("note2", rng.pick(GENERIC_NOTES));
        let choice = rng.below(repeatable.len() + unused_once.len());
        let template = match repeatable.get(choice) {
            Some(template) => *template,
            None => match choice.checked_sub(repeatable.len()) {
                Some(index) if index < unused_once.len() => unused_once.remove(index),
                _ => "",
            },
        };
        out.push_str(&render(template, vars));
    }
    out
}

/// Go struct fields aligned like `gofmt` does.
fn go_struct_fields(fields: &[(&str, &str)]) -> String {
    let width = fields.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    fields
        .iter()
        .map(|(name, ty)| {
            format!(
                "	{name:<width$} {ty}
"
            )
        })
        .collect()
}

fn distinct_verbs(rng: &mut SplitMix64, count: usize) -> Vec<(&'static str, &'static str)> {
    let mut chosen: Vec<(&'static str, &'static str)> = Vec::with_capacity(count);
    let mut guard = 0;
    while chosen.len() < count && guard < count * 8 {
        guard += 1;
        if let Some(verb) = rng.pick_ref(VERBS).copied()
            && !chosen.contains(&verb)
        {
            chosen.push(verb);
        }
    }
    chosen
}

/// Replaces `{{name}}` placeholders with variables in one pass. Unknown
/// names are left untouched (tests assert no placeholder survives).
fn render(template: &str, vars: &Vars) -> String {
    let mut out = String::with_capacity(template.len() + 256);
    let mut rest = template;
    while let Some((before, after)) = rest.split_once("{{") {
        out.push_str(before);
        match after.split_once("}}") {
            Some((name, tail)) if is_var_name(name) => match vars.get(name) {
                Some(value) => {
                    out.push_str(value);
                    rest = tail;
                }
                None => {
                    out.push_str("{{");
                    rest = after;
                }
            },
            _ => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_var_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// Identifier helpers (input is snake_case ASCII).
// ---------------------------------------------------------------------------

pub(crate) fn pascal(snake: &str) -> String {
    snake.split('_').map(capitalize).collect()
}

pub(crate) fn camel(snake: &str) -> String {
    let mut parts = snake.split('_');
    let mut out = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        out.push_str(&capitalize(part));
    }
    out
}

pub(crate) fn kebab(snake: &str) -> String {
    snake.replace('_', "-")
}

fn words(snake: &str) -> String {
    snake.replace('_', " ")
}

fn flat(snake: &str) -> String {
    snake.replace('_', "")
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// `module_entity`, without repeating the module when the entity already
/// starts with it (`audit` + `audit_entry` -> `audit_entry`).
fn qualified(module: &str, entity: &str) -> String {
    let first = entity.split('_').next().unwrap_or(entity);
    if module.starts_with(first) {
        entity.to_owned()
    } else {
        format!("{module}_{entity}")
    }
}

/// English plural of the last word of a snake_case noun.
pub(crate) fn plural(snake: &str) -> String {
    let (head, last) = match snake.rsplit_once('_') {
        Some((head, last)) => (format!("{head}_"), last),
        None => (String::new(), snake),
    };
    let last_plural = if let Some(stem) = last.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u'])
    {
        format!("{stem}ies")
    } else if last.ends_with('s')
        || last.ends_with('x')
        || last.ends_with("ch")
        || last.ends_with("sh")
    {
        format!("{last}es")
    } else {
        format!("{last}s")
    };
    head + &last_plural
}

// ---------------------------------------------------------------------------
// Templates.
// ---------------------------------------------------------------------------

const REACT_COMPONENT: &str = r##"import { useMemo, useState } from "react";
import { useT } from "../../../i18n";

// {{note}}
export interface {{Entity}}Row {
  id: string;
  {{field}}: string;
  {{field2}}: number;
  updatedAt: string;
}

export interface {{Entity}}{{Variant}}Props {
  rows: {{Entity}}Row[];
  onSelect?: (row: {{Entity}}Row) => void;
}

export function {{Entity}}{{Variant}}({ rows, onSelect }: {{Entity}}{{Variant}}Props) {
  const t = useT();
  const [query, setQuery] = useState("");
  const visible = useMemo(
    () => rows.filter((row) => row.{{field}}.toLowerCase().includes(query.toLowerCase())),
    [rows, query],
  );
  if (rows.length === 0) {
    return <p className="empty">{t("{{module_camel}}.{{entity}}.empty")}</p>;
  }
  return (
    <section className="{{entity_kebab}}-{{variant_kebab}}">
      <h2>{t("{{module_camel}}.{{entity}}.title")}</h2>
      <input value={query} onChange={(event) => setQuery(event.target.value)} />
      <ul>
        {visible.map((row) => (
          <li key={row.id} onClick={() => onSelect?.(row)}>
            {row.{{field}}} ({row.{{field2}}})
          </li>
        ))}
      </ul>
    </section>
  );
}
{{helpers}}"##;

const TS_HELPER_SORT: &str = r##"
/** {{note2}} */
export function {{verb}}{{Entities}}(rows: {{Entity}}Row[]): {{Entity}}Row[] {
  return [...rows].sort((a, b) => a.{{field2}} - b.{{field2}});
}
"##;

const TS_HELPER_LABEL: &str = r##"
export function format{{Entity}}Label(row: {{Entity}}Row, locale: string): string {
  const updated = new Date(row.updatedAt).toLocaleDateString(locale);
  return `${row.{{field}}} - ${updated}`;
}
"##;

const TS_HELPER_POST: &str = r##"
export async function {{verb}}{{Entity}}(id: string): Promise<{{Entity}}Row | null> {
  const response = await fetch(`/api/{{module_kebab}}/{{entities_kebab}}/${encodeURIComponent(id)}/{{verb_kebab}}`, {
    method: "POST",
  });
  if (!response.ok) {
    return null;
  }
  return (await response.json()) as {{Entity}}Row;
}
"##;

const REACT_HOOK: &str = r##"import { useEffect, useState } from "react";

// {{note}}
export interface {{Entity}}{{Variant}}State {
  items: { id: string; {{field}}: string; {{field2}}: number }[];
  loading: boolean;
  error: string | null;
}

export function use{{Entity}}{{Variant}}(scope: string): {{Entity}}{{Variant}}State {
  const [state, setState] = useState<{{Entity}}{{Variant}}State>({ items: [], loading: true, error: null });
  useEffect(() => {
    let cancelled = false;
    fetch(`/api/{{module_kebab}}/{{entities_kebab}}?scope=${encodeURIComponent(scope)}`)
      .then((response) => response.json())
      .then((items) => {
        if (!cancelled) setState({ items, loading: false, error: null });
      })
      .catch((error: Error) => {
        if (!cancelled) setState({ items: [], loading: false, error: error.message });
      });
    return () => {
      cancelled = true;
    };
  }, [scope]);
  return state;
}
"##;

const NEST_CLASS: &str = r##"import { Injectable, Logger, NotFoundException } from "@nestjs/common";

// {{note}}
export interface {{Entity}}Record {
  id: string;
  {{field}}: string;
  {{field2}}: number;
  updatedAt: Date;
}

@Injectable()
export class {{Entity}}{{Kind}} {
  private readonly logger = new Logger({{Entity}}{{Kind}}.name);
  private readonly items = new Map<string, {{Entity}}Record>();
{{methods}}}
"##;

const NEST_METHOD_GET: &str = r##"
  async {{verb}}{{Entity}}(id: string): Promise<{{Entity}}Record> {
    const item = this.items.get(id);
    if (!item) {
      throw new NotFoundException(`{{entity_words}} ${id} not found`);
    }
    item.updatedAt = new Date();
    this.logger.debug(`{{verb}} {{entity_words}} ${id}`);
    return item;
  }
"##;

const NEST_METHOD_LIST: &str = r##"
  {{verb}}{{Entities}}(limit = {{n}}): {{Entity}}Record[] {
    return [...this.items.values()]
      .filter((item) => item.{{field2}} >= 0)
      .sort((a, b) => b.updatedAt.getTime() - a.updatedAt.getTime())
      .slice(0, limit);
  }
"##;

const NEST_METHOD_SET: &str = r##"
  // {{note2}}
  set{{Entity}}{{Field}}(id: string, value: string): boolean {
    const item = this.items.get(id);
    if (!item || item.{{field}} === value) {
      return false;
    }
    item.{{field}} = value;
    item.updatedAt = new Date();
    return true;
  }
"##;

const DART_CLASS: &str = r##"import 'package:dio/dio.dart';

// {{note}}
class {{Entity}} {
  const {{Entity}}({required this.id, required this.{{field}}, this.{{field2}} = 0});

  final String id;
  final String {{field}};
  final int {{field2}};

  factory {{Entity}}.fromJson(Map<String, dynamic> json) => {{Entity}}(
        id: json['id'] as String,
        {{field}}: json['{{field_snake}}'] as String? ?? '',
        {{field2}}: json['{{field2_snake}}'] as int? ?? 0,
      );
}

class {{Entity}}{{Kind}} {
  {{Entity}}{{Kind}}(this._dio);

  final Dio _dio;
  final Map<String, {{Entity}}> _cache = {};
{{methods}}}
"##;

const DART_METHOD_FETCH: &str = r##"
  Future<List<{{Entity}}>> {{verb}}{{Entities}}() async {
    final response = await _dio.get<List<dynamic>>('/v1/{{module_kebab}}/{{entities_kebab}}');
    final items = (response.data ?? const [])
        .map((item) => {{Entity}}.fromJson(item as Map<String, dynamic>))
        .toList();
    for (final item in items) {
      _cache[item.id] = item;
    }
    return items;
  }
"##;

const DART_METHOD_LOOKUP: &str = r##"
  // {{note2}}
  {{Entity}}? cached{{Entity}}(String id) {
    final cached = _cache[id];
    if (cached == null || cached.{{field2}} < 0) {
      return null;
    }
    return cached;
  }
"##;

const DART_METHOD_POST: &str = r##"
  Future<void> {{verb}}{{Entity}}{{Field}}(String id, String value) async {
    await _dio.post<void>('/v1/{{module_kebab}}/{{entities_kebab}}/$id/{{verb_kebab}}', data: {'{{field_snake}}': value});
    _cache.remove(id);
  }
"##;

const GO_FILE: &str = r##"package {{package}}

import (
	"context"
	"errors"
	"fmt"
	"time"
)

// {{note}}
type {{Entity}} struct {
{{go_fields}}}

var Err{{Entity}}NotFound = errors.New("{{entity_words}} not found")

// {{Entity}}Store persists {{entities_words}}.
type {{Entity}}Store interface {
	Get(ctx context.Context, id string) ({{Entity}}, error)
	Save(ctx context.Context, item {{Entity}}) error
}

type {{Entity}}{{Kind}} struct {
	store {{Entity}}Store
	now   func() time.Time
}

func New{{Entity}}{{Kind}}(store {{Entity}}Store) *{{Entity}}{{Kind}} {
	return &{{Entity}}{{Kind}}{store: store, now: time.Now}
}
{{methods}}"##;

const GO_METHOD_ONE: &str = r##"
func (s *{{Entity}}{{Kind}}) {{Verb}}{{Entity}}(ctx context.Context, id string) error {
	item, err := s.store.Get(ctx, id)
	if err != nil {
		return fmt.Errorf("{{verb}} {{entity_words}} %s: %w", id, err)
	}
	item.UpdatedAt = s.now()
	return s.store.Save(ctx, item)
}
"##;

const GO_METHOD_MANY: &str = r##"
// {{Verb}}{{Entities}} processes ids in order. {{note2}}
func (s *{{Entity}}{{Kind}}) {{Verb}}{{Entities}}(ctx context.Context, ids []string, deadline time.Duration) (int, error) {
	ctx, cancel := context.WithTimeout(ctx, deadline)
	defer cancel()
	done := 0
	for _, id := range ids {
		item, err := s.store.Get(ctx, id)
		if errors.Is(err, Err{{Entity}}NotFound) {
			continue
		}
		if err != nil {
			return done, err
		}
		if item.{{Field2}} < 0 {
			item.{{Field2}} = 0
		}
		if err := s.store.Save(ctx, item); err != nil {
			return done, err
		}
		done++
	}
	return done, nil
}
"##;

const GO_METHOD_SET: &str = r##"
func (s *{{Entity}}{{Kind}}) Set{{Entity}}{{Field}}(ctx context.Context, id, value string) (bool, error) {
	item, err := s.store.Get(ctx, id)
	if err != nil {
		return false, err
	}
	if item.{{Field}} == value {
		return false, nil
	}
	item.{{Field}} = value
	item.UpdatedAt = s.now()
	return true, s.store.Save(ctx, item)
}
"##;

const PY_MODULE: &str = r##""""{{Module_words}}: {{entity_words}} {{kind_words}}.

{{note}}
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone


@dataclass
class {{Entity}}:
    id: str
    {{field_snake}}: str
    {{field2_snake}}: int = 0
    updated_at: datetime = field(default_factory=lambda: datetime.now(timezone.utc))


class {{Entity}}{{Kind}}:
    def __init__(self, session, batch_size: int = {{n}}):
        self._session = session
        self._batch_size = batch_size
{{methods}}"##;

const PY_METHOD_GET: &str = r##"
    def {{verb}}_{{entity_snake}}(self, item_id: str) -> {{Entity}} | None:
        item = self._session.get({{Entity}}, item_id)
        if item is None:
            return None
        item.updated_at = datetime.now(timezone.utc)
        return item
"##;

const PY_METHOD_SELECT: &str = r##"
    def {{verb}}_{{entities_snake}}(self, items: list[{{Entity}}]) -> list[{{Entity}}]:
        """{{note2}}"""
        selected = [item for item in items if item.{{field2_snake}} >= 0]
        selected.sort(key=lambda item: item.updated_at, reverse=True)
        return selected[: self._batch_size]
"##;

const PY_METHOD_SET: &str = r##"
    def set_{{field_snake}}(self, item: {{Entity}}, value: str) -> bool:
        if item.{{field_snake}} == value:
            return False
        item.{{field_snake}} = value
        item.updated_at = datetime.now(timezone.utc)
        return True
"##;

const PY_TEST: &str = r##"from ledger.{{module_snake}}.{{area}}.{{entity_snake}}_{{kind_snake}} import {{Entity}}


def test_{{entity_snake}}_defaults():
    item = {{Entity}}(id="x1", {{field_snake}}="value")
    assert item.{{field2_snake}} == 0
    assert item.updated_at.tzinfo is not None


def test_{{entity_snake}}_{{field_snake}}_is_kept():
    item = {{Entity}}(id="x2", {{field_snake}}="other")
    assert item.{{field_snake}} == "other"
"##;

const RUST_FILE: &str = r##"//! {{note}}

use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct {{Entity}} {
    pub id: String,
    pub {{field_snake}}: String,
    pub {{field2_snake}}: i64,
}

pub struct {{Entity}}{{Kind}} {
    batch_size: usize,
    interval: Duration,
}

impl {{Entity}}{{Kind}} {
    pub fn new(batch_size: usize) -> Self {
        Self {
            batch_size,
            interval: Duration::from_secs({{n}}),
        }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }
{{methods}}}
"##;

const RS_METHOD_IDS: &str = r##"
    pub fn {{verb}}_{{entities_snake}}(&self, items: &[{{Entity}}]) -> Vec<String> {
        items
            .iter()
            .filter(|item| item.{{field2_snake}} >= 0)
            .take(self.batch_size)
            .map(|item| item.id.clone())
            .collect()
    }
"##;

const RS_METHOD_SET: &str = r##"
    /// {{note2}}
    pub fn set_{{field_snake}}(&self, item: &mut {{Entity}}, value: &str) -> bool {
        if item.{{field_snake}} == value {
            return false;
        }
        item.{{field_snake}} = value.to_owned();
        true
    }
"##;

const RS_METHOD_SUMMARY: &str = r##"
    pub fn {{verb}}_summary(&self, items: &[{{Entity}}]) -> String {
        let total: i64 = items.iter().map(|item| item.{{field2_snake}}).sum();
        format!("{} {{entities_words}}, {{field2_words}} total {}", items.len(), total)
    }
"##;

const PROTO_FILE: &str = r##"syntax = "proto3";

package acme.{{module_flat}}.v{{version}};

option go_package = "example.com/acme/contracts/gen/go/{{module_flat}}/v{{version}};{{module_flat}}v{{version}}";

// {{note}}
service {{Entity}}{{Kind}}Service {
{{rpcs}}}

message {{Entity}} {
  string id = 1;
  string {{field_snake}} = 2;
  int64 {{field2_snake}} = 3;
  int64 updated_at_unix = 4;
}
{{messages}}"##;

const PROTO_RPC: &str =
    "  rpc {{Verb}}{{Entity}}({{Verb}}{{Entity}}Request) returns ({{Verb}}{{Entity}}Response);\n";

const PROTO_MESSAGES: &str = r##"
message {{Verb}}{{Entity}}Request {
  string id = 1;
}

message {{Verb}}{{Entity}}Response {
  {{Entity}} {{entity_snake}} = 1;
}
"##;

const EVENT_SCHEMA: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://contracts.example.com/events/{{module_snake}}.{{entity_snake}}.{{past}}.v{{version}}.json",
  "title": "{{module_snake}}.{{entity_snake}}.{{past}}",
  "description": "{{note}}",
  "type": "object",
  "required": ["id", "{{field}}"],
  "properties": {
    "id": { "type": "string" },
    "{{field}}": { "type": "string" },
    "{{field2}}": { "type": "integer" },
    "occurredAt": { "type": "string", "format": "date-time" }
  }
}
"##;

const SQL_CREATE: &str = r##"-- {{note}}
CREATE TABLE {{table}} (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    {{field_snake}} varchar(120) NOT NULL,
    {{field2_snake}} integer NOT NULL DEFAULT 0,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX {{table}}_{{field_snake}}_idx ON {{table}} ({{field_snake}});
"##;

const SQL_ADD: &str = r##"-- {{note}}
ALTER TABLE {{table}} ADD COLUMN {{field_snake}} varchar(120);

UPDATE {{table}} SET {{field_snake}} = '' WHERE {{field_snake}} IS NULL;

ALTER TABLE {{table}} ALTER COLUMN {{field_snake}} SET NOT NULL;
"##;

const SQL_INDEX: &str = r##"-- {{note}}
CREATE INDEX CONCURRENTLY IF NOT EXISTS {{table}}_{{field2_snake}}_idx
    ON {{table}} ({{field2_snake}} DESC);
"##;

const SQL_BACKFILL: &str = r##"-- {{note}}
-- {{note2}}
UPDATE {{table}}
SET {{field_snake}} = lower({{field_snake}}),
    updated_at = now()
WHERE {{field_snake}} <> lower({{field_snake}});
"##;

const K8S_DEPLOYMENT: &str = r##"# {{note}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{qualified_kebab}}
  labels:
    app: {{qualified_kebab}}
spec:
  replicas: {{replicas}}
  selector:
    matchLabels:
      app: {{qualified_kebab}}
  template:
    metadata:
      labels:
        app: {{qualified_kebab}}
    spec:
      containers:
        - name: app
          image: registry.example.com/acme/{{qualified_kebab}}:{{image_tag}}
          env:
            - name: {{QUALIFIED}}_BATCH_SIZE
              value: "{{n}}"
            - name: READ_REPLICA_URL
              valueFrom:
                secretKeyRef:
                  name: {{module_kebab}}-replica
                  key: url
          resources:
            requests:
              cpu: {{cpu}}m
              memory: {{mem}}Mi
"##;

const K8S_CRONJOB: &str = r##"# {{note}}
apiVersion: batch/v1
kind: CronJob
metadata:
  name: {{qualified_kebab}}-{{verb_kebab}}
spec:
  schedule: "{{minute}} {{hour}} * * *"
  concurrencyPolicy: Forbid
  jobTemplate:
    spec:
      backoffLimit: {{replicas}}
      template:
        spec:
          restartPolicy: OnFailure
          containers:
            - name: job
              image: registry.example.com/acme/{{module_kebab}}-jobs:{{image_tag}}
              args: ["{{verb_kebab}}-{{entities_kebab}}"]
              env:
                - name: {{QUALIFIED}}_DRY_RUN
                  value: "false"
"##;

const K8S_CONFIGMAP: &str = r##"# {{note}}
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{qualified_kebab}}-config
data:
  {{field_kebab}}-source: "{{module_kebab}}"
  batch-size: "{{n}}"
  schedule: "{{minute}} {{hour}} * * *"
"##;

const K8S_AUTOSCALER: &str = r##"# {{note}}
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: {{qualified_kebab}}
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: {{qualified_kebab}}
  minReplicas: 1
  maxReplicas: {{replicas}}
  metrics:
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: 70
"##;

const MARKDOWN_NOTE: &str = r##"# {{Module_words}}: {{topic_words}} on {{entities_words}} ({{date}})

Attendees: {{people}}

## Context

{{note}}

## Discussion

- {{discussion1}}
- {{discussion2}}

## Decisions

- {{decision}}

## Follow-ups

- [ ] {{person}} to {{verb}} the {{entity_words}} {{field_words}} handling by {{date2}}.
"##;

const MARKDOWN_RUNBOOK: &str = r##"# Runbook: {{entity_words}} {{verb}} failures

Owner: {{module_words}} team

## Symptoms

- Alerts from `{{module_kebab}}-{{entity_kebab}}` about failed {{verb}} runs.
- {{note}}

## Checks

1. Logs of the last run: look for timeouts and validation errors.
2. {{check}}

## Remedies

- Re-run the job; it is safe to run again for the same day.
- If {{field_words}} values are missing, run the backfill and re-check.
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_helpers() {
        assert_eq!(pascal("stock_level"), "StockLevel");
        assert_eq!(camel("stock_level"), "stockLevel");
        assert_eq!(kebab("stock_level"), "stock-level");
        assert_eq!(plural("category"), "categories");
        assert_eq!(plural("address"), "addresses");
        assert_eq!(plural("stock_level"), "stock_levels");
        assert_eq!(plural("search_index"), "search_indexes");
        assert_eq!(plural("survey"), "surveys");
        assert_eq!(qualified("audit", "audit_entry"), "audit_entry");
        assert_eq!(qualified("bundles", "bundle"), "bundle");
        assert_eq!(qualified("catalog", "product"), "catalog_product");
    }

    #[test]
    fn render_replaces_known_and_keeps_unknown() {
        let mut vars = Vars::default();
        vars.set("a", "X");
        assert_eq!(
            render("{{a}}-{{b}}-{ {{a}} }-{{", &vars),
            "X-{{b}}-{ X }-{{"
        );
        assert_eq!(render("{row.{{a}}}", &vars), "{row.X}");
    }

    #[test]
    fn every_style_renders_without_leftover_placeholders() {
        let styles = [
            NoiseStyle::React,
            NoiseStyle::Nest,
            NoiseStyle::Dart,
            NoiseStyle::Go,
            NoiseStyle::Python,
            NoiseStyle::Rust,
            NoiseStyle::Contracts,
            NoiseStyle::Sql,
            NoiseStyle::Kubernetes,
            NoiseStyle::Markdown,
        ];
        for style in styles {
            let mut rng = SplitMix64::new(1);
            let mut used = BTreeSet::new();
            let files = generate(style, &mut rng, 300, &mut used);
            assert_eq!(files.len(), 300);
            assert_eq!(used.len(), 300, "{style:?} paths must be unique");
            for (path, content) in &files {
                assert!(
                    !content.contains("{{"),
                    "{style:?} {path} has a leftover placeholder"
                );
                assert!(!content.contains('\r'), "{path}");
                assert!(content.ends_with('\n'), "{path} must end with a newline");
            }
        }
    }
}

//! Key and attribute templates of rules.
//!
//! Syntax: literal text with `{var}` or `{var|filter|filter:arg}` holes;
//! `{}` is an explicitly dynamic part; `{{` and `}}` are literal braces.
//! Variables are query captures, lookups, defaults or the built-in path
//! variables (`path.dir`, `path.stem`, `path.name`, `route`).

use std::collections::{BTreeMap, BTreeSet};

use crate::model::DYN;

/// Most alternatives one template renders to (lists multiply).
pub(crate) const MAX_ALTERNATIVES: usize = 64;

/// A text transformation applied to a variable's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Filter {
    Upper,
    Lower,
    Snake,
    Plural,
    LastSegment,
    Trim,
    StripPrefix(String),
    StripSuffix(String),
    Default(String),
    Else(String),
    GoTag { tag: String, option: String },
    GrpcService,
    HttpMethod,
}

impl Filter {
    fn parse(text: &str) -> Result<Self, String> {
        let (name, arg) = match text.split_once(':') {
            Some((name, arg)) => (name.trim(), Some(arg)),
            None => (text.trim(), None),
        };
        let need = |arg: Option<&str>| -> Result<String, String> {
            arg.map(str::to_owned)
                .ok_or_else(|| format!("filter `{name}` needs an argument (`{name}:...`)"))
        };
        Ok(match name {
            "upper" => Self::Upper,
            "lower" => Self::Lower,
            "snake" => Self::Snake,
            "plural" => Self::Plural,
            "last_segment" => Self::LastSegment,
            "trim" => Self::Trim,
            "strip_prefix" => Self::StripPrefix(need(arg)?),
            "strip_suffix" => Self::StripSuffix(need(arg)?),
            "default" => Self::Default(need(arg)?),
            "else" => Self::Else(need(arg)?.trim().to_owned()),
            "gotag" => {
                let arg = need(arg)?;
                let (tag, option) = arg
                    .split_once(':')
                    .ok_or_else(|| "filter `gotag` needs `gotag:<tag>:<option>`".to_owned())?;
                Self::GoTag {
                    tag: tag.to_owned(),
                    option: option.to_owned(),
                }
            }
            "grpc_service" => Self::GrpcService,
            "http_method" => Self::HttpMethod,
            other => return Err(format!("unknown filter `{other}`")),
        })
    }

    fn apply(&self, value: &str, vars: &dyn Fn(&str) -> Option<String>) -> String {
        match self {
            Self::Upper => value.to_uppercase(),
            Self::Lower => value.to_lowercase(),
            Self::Snake => snake_case(value),
            Self::Plural => plural(value),
            Self::LastSegment => last_segment(value).to_owned(),
            Self::Trim => value.trim().to_owned(),
            Self::StripPrefix(prefix) => value
                .strip_prefix(prefix.as_str())
                .unwrap_or(value)
                .to_owned(),
            Self::StripSuffix(suffix) => value
                .strip_suffix(suffix.as_str())
                .unwrap_or(value)
                .to_owned(),
            Self::Default(text) => {
                if value.is_empty() {
                    text.clone()
                } else {
                    value.to_owned()
                }
            }
            Self::Else(var) => {
                if value.is_empty() {
                    vars(var).unwrap_or_default()
                } else {
                    value.to_owned()
                }
            }
            Self::GoTag { tag, option } => go_tag_option(value, tag, option),
            Self::GrpcService => grpc_service(value),
            Self::HttpMethod => {
                let last = last_segment(value);
                let last = last.strip_prefix("Method").unwrap_or(last);
                last.to_uppercase()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    Dynamic,
    Var { name: String, filters: Vec<Filter> },
}

/// A parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Template {
    parts: Vec<Part>,
}

impl Template {
    /// Parses `source`; the error explains what is wrong.
    pub(crate) fn parse(source: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = source.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    literal.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    literal.push('}');
                }
                '{' => {
                    let mut inner = String::new();
                    let mut closed = false;
                    for d in chars.by_ref() {
                        if d == '}' {
                            closed = true;
                            break;
                        }
                        inner.push(d);
                    }
                    if !closed {
                        return Err(format!("unclosed `{{` in template `{source}`"));
                    }
                    if !literal.is_empty() {
                        parts.push(Part::Literal(std::mem::take(&mut literal)));
                    }
                    if inner.trim().is_empty() {
                        parts.push(Part::Dynamic);
                        continue;
                    }
                    let mut pieces = inner.split('|');
                    let name = pieces.next().unwrap_or("").trim().to_owned();
                    if name.is_empty() {
                        return Err(format!("empty variable name in template `{source}`"));
                    }
                    let filters = pieces.map(Filter::parse).collect::<Result<Vec<_>, _>>()?;
                    parts.push(Part::Var { name, filters });
                }
                '}' => return Err(format!("unmatched `}}` in template `{source}`")),
                _ => literal.push(c),
            }
        }
        if !literal.is_empty() {
            parts.push(Part::Literal(literal));
        }
        Ok(Self { parts })
    }

    /// Every variable the template reads (including `else:` targets).
    pub(crate) fn variables(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for part in &self.parts {
            if let Part::Var { name, filters } = part {
                out.insert(name.clone());
                for filter in filters {
                    if let Filter::Else(var) = filter {
                        out.insert(var.clone());
                    }
                }
            }
        }
        out
    }

    /// Renders every combination of the variables' alternatives (capped at
    /// [`MAX_ALTERNATIVES`]). A variable without value renders as empty text.
    pub(crate) fn render(&self, values: &BTreeMap<String, Vec<String>>) -> Vec<String> {
        let mut outputs: Vec<String> = vec![String::new()];
        let first =
            |name: &str| -> Option<String> { values.get(name).and_then(|v| v.first()).cloned() };
        for part in &self.parts {
            let options: Vec<String> = match part {
                Part::Literal(text) => vec![text.clone()],
                Part::Dynamic => vec![DYN.to_string()],
                Part::Var { name, filters } => {
                    let raw = match values.get(name) {
                        Some(list) if !list.is_empty() => list.clone(),
                        _ => vec![String::new()],
                    };
                    raw.into_iter()
                        .map(|value| {
                            filters
                                .iter()
                                .fold(value, |acc, filter| filter.apply(&acc, &first))
                        })
                        .collect()
                }
            };
            let mut next = Vec::new();
            for prefix in &outputs {
                for option in &options {
                    if next.len() >= MAX_ALTERNATIVES {
                        break;
                    }
                    next.push(format!("{prefix}{option}"));
                }
            }
            outputs = next;
        }
        outputs.sort();
        outputs.dedup();
        outputs
    }
}

/// `CamelCase`, `camelCase`, `kebab-case` -> `snake_case`.
pub(crate) fn snake_case(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len() + 4);
    for (index, c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev = index.checked_sub(1).and_then(|i| chars.get(i));
            let next = chars.get(index + 1);
            let boundary = match prev {
                Some(p) if p.is_lowercase() || p.is_ascii_digit() => true,
                Some(p) if p.is_uppercase() => next.is_some_and(|n| n.is_lowercase()),
                _ => false,
            };
            if boundary && !out.ends_with('_') {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else if *c == '-' || *c == ' ' {
            out.push('_');
        } else {
            out.push(*c);
        }
    }
    out
}

/// Naive English plural (`category` -> `categories`, `box` -> `boxes`).
pub(crate) fn plural(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let lower = value.to_lowercase();
    if let Some(stem) = value.strip_suffix('y') {
        let before = stem.chars().last();
        if before.is_some_and(|c| !"aeiou".contains(c.to_ascii_lowercase())) {
            return format!("{stem}ies");
        }
    }
    if ["s", "x", "z", "ch", "sh"]
        .iter()
        .any(|s| lower.ends_with(s))
    {
        return format!("{value}es");
    }
    format!("{value}s")
}

/// Text after the last `.`, `::` or `/`.
pub(crate) fn last_segment(value: &str) -> &str {
    let cut = value
        .rfind("::")
        .map(|i| i + 2)
        .into_iter()
        .chain(value.rfind('.').map(|i| i + 1))
        .chain(value.rfind('/').map(|i| i + 1))
        .max()
        .unwrap_or(0);
    value.get(cut..).unwrap_or(value)
}

/// The value of `option` inside the `tag:"..."` part of a Go struct tag
/// (`gorm:"column:id;primaryKey"` -> `id` for `gorm`/`column`). A bare
/// option (`gorm:"-"` with option `-`) yields the option itself.
pub(crate) fn go_tag_option(tag_text: &str, tag: &str, option: &str) -> String {
    let text = tag_text.trim_matches('`');
    let marker = format!("{tag}:\"");
    let Some(start) = text.find(&marker) else {
        return String::new();
    };
    let body = text.get(start + marker.len()..).unwrap_or("");
    let body = body.split('"').next().unwrap_or("");
    for item in body.split(';') {
        let item = item.trim();
        if let Some((key, value)) = item.split_once(':') {
            if key.trim().eq_ignore_ascii_case(option) {
                return value.trim().to_owned();
            }
        } else if item == option {
            return item.to_owned();
        }
    }
    String::new()
}

/// The gRPC service name inside a generated identifier
/// (`UnimplementedRouteServiceServer`, `NewRouteServiceClient`,
/// `RouteServiceStub`, `add_RouteServiceServicer_to_server`,
/// `pkg.RouteService.service`, `RouteServiceService` -> `RouteService`).
pub(crate) fn grpc_service(value: &str) -> String {
    let mut parts: Vec<&str> = value.split('.').filter(|p| !p.is_empty()).collect();
    if parts.last() == Some(&"service") {
        parts.pop();
    }
    let mut name = parts.last().copied().unwrap_or("").to_owned();
    for prefix in ["Unimplemented", "Unsafe", "New", "Register", "add_"] {
        if let Some(rest) = name.strip_prefix(prefix)
            && rest.chars().next().is_some_and(char::is_uppercase)
        {
            name = rest.to_owned();
        }
    }
    if let Some(rest) = name.strip_suffix("_to_server") {
        name = rest.to_owned();
    }
    for suffix in ["Servicer", "Server", "Client", "Stub", "Impl"] {
        if let Some(rest) = name.strip_suffix(suffix)
            && !rest.is_empty()
        {
            name = rest.to_owned();
            break;
        }
    }
    // `RouteServiceService` (grpc-js service definitions) names the service
    // `RouteService`.
    if name.ends_with("ServiceService") {
        name.truncate(name.len().saturating_sub("Service".len()));
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.iter().map(|s| (*s).to_owned()).collect()))
            .collect()
    }

    #[test]
    fn parse_and_render() {
        let t = Template::parse("{verb|upper} /{prefix}/{path}").unwrap();
        assert_eq!(
            t.variables().into_iter().collect::<Vec<_>>(),
            ["path", "prefix", "verb"]
        );
        let out = t.render(&vals(&[("verb", &["get"]), ("prefix", &["v1/x"])]));
        assert_eq!(out, ["GET /v1/x/"]);
        let out = t.render(&vals(&[("verb", &["get", "post"]), ("path", &["a"])]));
        assert_eq!(out, ["GET //a", "POST //a"]);
    }

    #[test]
    fn dynamic_and_escapes() {
        let t = Template::parse("{{lit}} {}/{x|default:none}").unwrap();
        assert_eq!(t.render(&BTreeMap::new()), [format!("{{lit}} {DYN}/none")]);
        assert!(Template::parse("{open").is_err());
        assert!(Template::parse("close}").is_err());
        assert!(Template::parse("{x|nope}").is_err());
        assert!(Template::parse("{x|strip_prefix}").is_err());
        assert!(Template::parse("{|upper}").is_err());
    }

    #[test]
    fn else_filter_and_cap() {
        let t = Template::parse("{table|else:entity|snake}").unwrap();
        assert_eq!(
            t.render(&vals(&[("entity", &["LoadingSlot"])])),
            ["loading_slot"]
        );
        let many: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let t = Template::parse("{a}{b}").unwrap();
        let out = t.render(&vals(&[("a", &refs), ("b", &refs)]));
        assert!(out.len() <= MAX_ALTERNATIVES);
    }

    #[test]
    fn filters() {
        assert_eq!(snake_case("LoadingSlot"), "loading_slot");
        assert_eq!(snake_case("HTTPServer"), "http_server");
        assert_eq!(snake_case("maxLoad"), "max_load");
        assert_eq!(snake_case("StationID"), "station_id");
        assert_eq!(plural("charging_station"), "charging_stations");
        assert_eq!(plural("category"), "categories");
        assert_eq!(plural("box"), "boxes");
        assert_eq!(plural("day"), "days");
        assert_eq!(last_segment("RequestMethod.PUT"), "PUT");
        assert_eq!(last_segment("a::b"), "b");
        assert_eq!(
            go_tag_option("`gorm:\"column:id;primaryKey\"`", "gorm", "column"),
            "id"
        );
        assert_eq!(go_tag_option("`gorm:\"-\"`", "gorm", "-"), "-");
        assert_eq!(go_tag_option("`json:\"x\"`", "gorm", "column"), "");
        for raw in [
            "UnimplementedRouteServiceServer",
            "NewRouteServiceClient",
            "RouteServiceStub",
            "add_RouteServiceServicer_to_server",
            "pkg.v1.RouteService.service",
            "RouteServiceService",
            "RouteServiceClient",
            "route_pb2_grpc.RouteServiceServicer",
            "RouteService",
        ] {
            assert_eq!(grpc_service(raw), "RouteService", "{raw}");
        }
        let method = Filter::HttpMethod;
        assert_eq!(method.apply("http.MethodPost", &|_| None), "POST");
        assert_eq!(method.apply("RequestMethod.PUT", &|_| None), "PUT");
        assert_eq!(method.apply("delete", &|_| None), "DELETE");
    }
}

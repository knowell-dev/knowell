//! Structured parsing of SCIP symbol strings.
//!
//! The grammar follows the SCIP specification:
//!
//! ```text
//! <symbol>     ::= <scheme> ' ' <package> ' ' (<descriptor>)+ | 'local ' <local-id>
//! <package>    ::= <manager> ' ' <name> ' ' <version>        ('.' means empty)
//! <descriptor> ::= <name> '/' | <name> '#' | <name> '.' | <name> ':' | <name> '!'
//!                | <name> '(' <disambiguator> ').' | '[' <name> ']' | '(' <name> ')'
//! ```
//!
//! Space-delimited fields escape a space by doubling it; descriptor names that
//! are not plain identifiers are wrapped in backticks with embedded backticks
//! doubled. The parser is hand written (instead of the one in the `scip`
//! crate) so that hostile input yields an error and never a panic.

use std::fmt;

/// Longest symbol string the parser accepts, in bytes.
pub const MAX_SYMBOL_LEN: usize = 4096;

/// Why a symbol string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SymbolParseError {
    /// The string is empty.
    #[error("symbol is empty")]
    Empty,
    /// The string is longer than [`MAX_SYMBOL_LEN`].
    #[error("symbol is longer than {MAX_SYMBOL_LEN} bytes")]
    TooLong,
    /// A `local N` symbol has an empty or non-identifier id.
    #[error("invalid local symbol id")]
    InvalidLocal,
    /// A field ended before its terminator.
    #[error("unexpected end of symbol while reading {0}")]
    UnexpectedEnd(&'static str),
    /// A field that must not be empty was empty.
    #[error("empty {0}")]
    EmptyField(&'static str),
    /// A character that is not allowed at this position.
    #[error("unexpected character `{found}` while reading {context}")]
    Unexpected {
        /// What was being parsed.
        context: &'static str,
        /// The offending character.
        found: char,
    },
}

/// Package part of a global symbol. Empty fields are kept as empty strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolPackage {
    /// Package manager, for example `cargo`, `npm`, `maven`.
    pub manager: String,
    /// Package name.
    pub name: String,
    /// Package version.
    pub version: String,
}

/// The syntactic role of one descriptor (its suffix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DescriptorKind {
    /// `name/`: namespace, module or package.
    Namespace,
    /// `name#`: class, struct, trait, interface, enum, type alias.
    Type,
    /// `name.`: field, constant, variable, function value.
    Term,
    /// `name(disambiguator).`: method or function.
    Method,
    /// `[name]`: type parameter.
    TypeParameter,
    /// `(name)`: parameter.
    Parameter,
    /// `name:`: meta descriptor.
    Meta,
    /// `name!`: macro.
    Macro,
}

/// One component of a global symbol's path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Descriptor {
    /// Unescaped name.
    pub name: String,
    /// Suffix kind.
    pub kind: DescriptorKind,
    /// Overload disambiguator; empty unless `kind` is [`DescriptorKind::Method`].
    pub disambiguator: String,
}

/// A parsed SCIP symbol.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScipSymbol {
    /// `local <id>`: only meaningful inside one document.
    Local {
        /// The id after `local `.
        id: String,
    },
    /// A symbol that is unique across documents and projects.
    Global {
        /// Scheme, usually the indexer family (`rust-analyzer`, `scip-typescript`, …).
        scheme: String,
        /// Package, absent when manager, name and version are all empty.
        package: Option<SymbolPackage>,
        /// Descriptors from the outermost namespace to the symbol itself.
        descriptors: Vec<Descriptor>,
    },
}

impl ScipSymbol {
    /// Parses a SCIP symbol string.
    pub fn parse(text: &str) -> Result<Self, SymbolParseError> {
        if text.is_empty() {
            return Err(SymbolParseError::Empty);
        }
        if text.len() > MAX_SYMBOL_LEN {
            return Err(SymbolParseError::TooLong);
        }
        if let Some(id) = text.strip_prefix("local ") {
            if id.is_empty() || !id.chars().all(is_simple_char) {
                return Err(SymbolParseError::InvalidLocal);
            }
            return Ok(Self::Local { id: id.to_owned() });
        }
        let mut p = Parser::new(text);
        let scheme = p.space_field("scheme")?;
        if scheme.is_empty() {
            return Err(SymbolParseError::EmptyField("scheme"));
        }
        let manager = dot_is_empty(p.space_field("package manager")?);
        let name = dot_is_empty(p.space_field("package name")?);
        let version = dot_is_empty(p.space_field("package version")?);
        let package = if manager.is_empty() && name.is_empty() && version.is_empty() {
            None
        } else {
            Some(SymbolPackage {
                manager,
                name,
                version,
            })
        };
        let descriptors = p.descriptors()?;
        Ok(Self::Global {
            scheme,
            package,
            descriptors,
        })
    }

    /// Whether this is a document-local symbol.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    /// The package of a global symbol, if it has one.
    pub fn package(&self) -> Option<&SymbolPackage> {
        match self {
            Self::Global { package, .. } => package.as_ref(),
            Self::Local { .. } => None,
        }
    }

    /// The descriptors of a global symbol (empty for locals).
    pub fn descriptors(&self) -> &[Descriptor] {
        match self {
            Self::Global { descriptors, .. } => descriptors,
            Self::Local { .. } => &[],
        }
    }

    /// The short, human-facing name: the last descriptor that is not a type
    /// parameter or parameter, or the local id.
    pub fn short_name(&self) -> Option<&str> {
        match self {
            Self::Local { id } => Some(id),
            Self::Global { descriptors, .. } => descriptors
                .iter()
                .rev()
                .find(|d| {
                    !matches!(
                        d.kind,
                        DescriptorKind::Parameter | DescriptorKind::TypeParameter
                    )
                })
                .or_else(|| descriptors.last())
                .map(|d| d.name.as_str()),
        }
    }

    /// A readable qualified name such as `payments.PaymentService.cancel()`:
    /// descriptor names joined by `.`, methods suffixed with `()`, parameters
    /// rendered as `(param x)` and type parameters as `<T>`.
    /// The package is not included. Locals render as `local N`.
    pub fn qualified_name(&self) -> String {
        match self {
            Self::Local { id } => format!("local {id}"),
            Self::Global { descriptors, .. } => {
                let mut out = String::new();
                let mut need_separator = false;
                for d in descriptors {
                    match d.kind {
                        DescriptorKind::Parameter => {
                            out.push_str("(param ");
                            out.push_str(&d.name);
                            out.push(')');
                        }
                        DescriptorKind::TypeParameter => {
                            out.push('<');
                            out.push_str(&d.name);
                            out.push('>');
                        }
                        kind => {
                            if need_separator {
                                out.push('.');
                            }
                            out.push_str(&d.name);
                            match kind {
                                DescriptorKind::Method => out.push_str("()"),
                                DescriptorKind::Macro => out.push('!'),
                                _ => {}
                            }
                            need_separator = true;
                        }
                    }
                }
                out
            }
        }
    }
}

/// Renders the canonical SCIP string, so `parse(s)?.to_string()` parses back
/// to an equal value for every symbol the parser accepts.
impl fmt::Display for ScipSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local { id } => write!(f, "local {id}"),
            Self::Global {
                scheme,
                package,
                descriptors,
            } => {
                write_space_field(f, scheme)?;
                match package {
                    Some(p) => {
                        write_space_field(f, &p.manager)?;
                        write_space_field(f, &p.name)?;
                        write_space_field(f, &p.version)?;
                    }
                    None => f.write_str(". . . ")?,
                }
                for d in descriptors {
                    write_descriptor(f, d)?;
                }
                Ok(())
            }
        }
    }
}

fn write_space_field(f: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    if value.is_empty() {
        return f.write_str(". ");
    }
    for c in value.chars() {
        if c == ' ' {
            f.write_str("  ")?;
        } else {
            write!(f, "{c}")?;
        }
    }
    f.write_str(" ")
}

fn write_name(f: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
    if !name.is_empty() && name.chars().all(is_simple_char) {
        return f.write_str(name);
    }
    f.write_str("`")?;
    for c in name.chars() {
        if c == '`' {
            f.write_str("``")?;
        } else {
            write!(f, "{c}")?;
        }
    }
    f.write_str("`")
}

fn write_descriptor(f: &mut fmt::Formatter<'_>, d: &Descriptor) -> fmt::Result {
    match d.kind {
        DescriptorKind::Parameter => {
            f.write_str("(")?;
            write_name(f, &d.name)?;
            f.write_str(")")
        }
        DescriptorKind::TypeParameter => {
            f.write_str("[")?;
            write_name(f, &d.name)?;
            f.write_str("]")
        }
        DescriptorKind::Method => {
            write_name(f, &d.name)?;
            write!(f, "({}).", d.disambiguator)
        }
        DescriptorKind::Namespace => {
            write_name(f, &d.name)?;
            f.write_str("/")
        }
        DescriptorKind::Type => {
            write_name(f, &d.name)?;
            f.write_str("#")
        }
        DescriptorKind::Term => {
            write_name(f, &d.name)?;
            f.write_str(".")
        }
        DescriptorKind::Meta => {
            write_name(f, &d.name)?;
            f.write_str(":")
        }
        DescriptorKind::Macro => {
            write_name(f, &d.name)?;
            f.write_str("!")
        }
    }
}

fn is_simple_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '$' | '+' | '-' | '_')
}

fn dot_is_empty(value: String) -> String {
    if value == "." { String::new() } else { value }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos.saturating_add(offset)).copied()
    }

    fn bump(&mut self) {
        self.pos = self.pos.saturating_add(1);
    }

    /// Reads a field terminated by a single space; two spaces are an escaped space.
    fn space_field(&mut self, what: &'static str) -> Result<String, SymbolParseError> {
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(SymbolParseError::UnexpectedEnd(what)),
                Some(' ') => {
                    if self.peek_at(1) == Some(' ') {
                        out.push(' ');
                        self.pos = self.pos.saturating_add(2);
                    } else {
                        self.bump();
                        return Ok(out);
                    }
                }
                Some(c) => {
                    out.push(c);
                    self.bump();
                }
            }
        }
    }

    /// Reads a descriptor name: plain identifier or backtick-escaped.
    fn name(&mut self, what: &'static str) -> Result<String, SymbolParseError> {
        if self.peek() == Some('`') {
            self.bump();
            let mut out = String::new();
            loop {
                match self.peek() {
                    None => return Err(SymbolParseError::UnexpectedEnd(what)),
                    Some('`') => {
                        if self.peek_at(1) == Some('`') {
                            out.push('`');
                            self.pos = self.pos.saturating_add(2);
                        } else {
                            self.bump();
                            return Ok(out);
                        }
                    }
                    Some(c) => {
                        out.push(c);
                        self.bump();
                    }
                }
            }
        }
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if !is_simple_char(c) {
                break;
            }
            out.push(c);
            self.bump();
        }
        if out.is_empty() {
            return match self.peek() {
                None => Err(SymbolParseError::UnexpectedEnd(what)),
                Some(found) => Err(SymbolParseError::Unexpected {
                    context: what,
                    found,
                }),
            };
        }
        Ok(out)
    }

    fn expect(&mut self, want: char, what: &'static str) -> Result<(), SymbolParseError> {
        match self.peek() {
            Some(c) if c == want => {
                self.bump();
                Ok(())
            }
            Some(found) => Err(SymbolParseError::Unexpected {
                context: what,
                found,
            }),
            None => Err(SymbolParseError::UnexpectedEnd(what)),
        }
    }

    fn descriptors(&mut self) -> Result<Vec<Descriptor>, SymbolParseError> {
        let mut out = Vec::new();
        while let Some(c) = self.peek() {
            let (name, kind, disambiguator) = match c {
                '(' => {
                    self.bump();
                    let name = self.name("parameter name")?;
                    self.expect(')', "parameter")?;
                    (name, DescriptorKind::Parameter, String::new())
                }
                '[' => {
                    self.bump();
                    let name = self.name("type parameter name")?;
                    self.expect(']', "type parameter")?;
                    (name, DescriptorKind::TypeParameter, String::new())
                }
                _ => {
                    let name = self.name("descriptor name")?;
                    match self.peek() {
                        Some('/') => {
                            self.bump();
                            (name, DescriptorKind::Namespace, String::new())
                        }
                        Some('#') => {
                            self.bump();
                            (name, DescriptorKind::Type, String::new())
                        }
                        Some('.') => {
                            self.bump();
                            (name, DescriptorKind::Term, String::new())
                        }
                        Some(':') => {
                            self.bump();
                            (name, DescriptorKind::Meta, String::new())
                        }
                        Some('!') => {
                            self.bump();
                            (name, DescriptorKind::Macro, String::new())
                        }
                        Some('(') => {
                            self.bump();
                            let mut disambiguator = String::new();
                            while let Some(d) = self.peek() {
                                if !is_simple_char(d) {
                                    break;
                                }
                                disambiguator.push(d);
                                self.bump();
                            }
                            self.expect(')', "method disambiguator")?;
                            self.expect('.', "method suffix")?;
                            (name, DescriptorKind::Method, disambiguator)
                        }
                        Some(found) => {
                            return Err(SymbolParseError::Unexpected {
                                context: "descriptor suffix",
                                found,
                            });
                        }
                        None => return Err(SymbolParseError::UnexpectedEnd("descriptor suffix")),
                    }
                }
            };
            out.push(Descriptor {
                name,
                kind,
                disambiguator,
            });
        }
        if out.is_empty() {
            return Err(SymbolParseError::EmptyField("descriptors"));
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_rust_analyzer_method() {
        let s = ScipSymbol::parse("rust-analyzer cargo payments 0.1.0 service/Payment#cancel().")
            .unwrap();
        let ScipSymbol::Global {
            scheme,
            package,
            descriptors,
        } = &s
        else {
            panic!("expected global");
        };
        assert_eq!(scheme, "rust-analyzer");
        let pkg = package.as_ref().unwrap();
        assert_eq!(pkg.manager, "cargo");
        assert_eq!(pkg.name, "payments");
        assert_eq!(pkg.version, "0.1.0");
        assert_eq!(descriptors.len(), 3);
        assert_eq!(descriptors[2].kind, DescriptorKind::Method);
        assert_eq!(s.short_name(), Some("cancel"));
        assert_eq!(s.qualified_name(), "service.Payment.cancel()");
        assert_eq!(
            s.to_string(),
            "rust-analyzer cargo payments 0.1.0 service/Payment#cancel()."
        );
    }

    #[test]
    fn parses_local() {
        let s = ScipSymbol::parse("local 42").unwrap();
        assert_eq!(s, ScipSymbol::Local { id: "42".into() });
        assert!(s.is_local());
        assert_eq!(s.short_name(), Some("42"));
        assert_eq!(s.to_string(), "local 42");
    }

    #[test]
    fn rejects_bad_locals() {
        assert_eq!(
            ScipSymbol::parse("local "),
            Err(SymbolParseError::InvalidLocal)
        );
        assert_eq!(
            ScipSymbol::parse("local a b"),
            Err(SymbolParseError::InvalidLocal)
        );
    }

    #[test]
    fn empty_package_fields_use_dot() {
        let s = ScipSymbol::parse("semanticdb . . . com/example/Foo#bar(+1).").unwrap();
        assert!(s.package().is_none());
        assert_eq!(s.descriptors().len(), 4);
        let method = s.descriptors().last().unwrap();
        assert_eq!(method.disambiguator, "+1");
        assert_eq!(s.to_string(), "semanticdb . . . com/example/Foo#bar(+1).");
    }

    #[test]
    fn handles_backtick_escapes() {
        let text = "scip-go gomod github.com/a/b v1.0.0 `github.com/a/b`/`we``ird name`#";
        let s = ScipSymbol::parse(text).unwrap();
        assert_eq!(s.descriptors()[1].name, "we`ird name");
        assert_eq!(s.descriptors()[0].name, "github.com/a/b");
        assert_eq!(ScipSymbol::parse(&s.to_string()).unwrap(), s);
    }

    #[test]
    fn handles_escaped_spaces_in_fields() {
        let s = ScipSymbol::parse("my  scheme mgr  x name  y 1.0 Foo#").unwrap();
        let ScipSymbol::Global {
            scheme, package, ..
        } = &s
        else {
            panic!("expected global");
        };
        assert_eq!(scheme, "my scheme");
        let pkg = package.as_ref().unwrap();
        assert_eq!(pkg.manager, "mgr x");
        assert_eq!(pkg.name, "name y");
        assert_eq!(ScipSymbol::parse(&s.to_string()).unwrap(), s);
    }

    #[test]
    fn parameters_and_type_parameters() {
        assert!(ScipSymbol::parse("scip-ts npm pkg 1.0.0 mod/fn()(arg)").is_err());
        let s = ScipSymbol::parse("scip-ts npm pkg 1.0.0 mod/fn().(arg)[T]").unwrap();
        let kinds: Vec<_> = s.descriptors().iter().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DescriptorKind::Namespace,
                DescriptorKind::Method,
                DescriptorKind::Parameter,
                DescriptorKind::TypeParameter
            ]
        );
        assert_eq!(s.short_name(), Some("fn"));
        assert_eq!(s.to_string(), "scip-ts npm pkg 1.0.0 mod/fn().(arg)[T]");
        assert_eq!(s.qualified_name(), "mod.fn()(param arg)<T>");
    }

    #[test]
    fn macros_meta_terms() {
        let s = ScipSymbol::parse("rust-analyzer cargo c 1 m/println!x:y.").unwrap();
        let kinds: Vec<_> = s.descriptors().iter().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DescriptorKind::Namespace,
                DescriptorKind::Macro,
                DescriptorKind::Meta,
                DescriptorKind::Term
            ]
        );
    }

    #[test]
    fn hostile_inputs_error_without_panicking() {
        for bad in [
            "",
            " ",
            "a",
            "a b",
            "a b c",
            "a b c d",
            "a b c d ",
            "a b c d `unterminated",
            "a b c d name",
            "a b c d name(",
            "a b c d name(x)",
            "a b c d (x",
            "a b c d [x",
            "a b c d ?",
            "  b c d x#",
            "a b c d \u{0}#",
        ] {
            assert!(ScipSymbol::parse(bad).is_err(), "should reject {bad:?}");
        }
        let long = "x".repeat(MAX_SYMBOL_LEN + 1);
        assert_eq!(ScipSymbol::parse(&long), Err(SymbolParseError::TooLong));
    }

    #[test]
    fn unicode_names_are_accepted() {
        let s = ScipSymbol::parse("scip-go gomod m v1 pkg/Ödeme#").unwrap();
        assert_eq!(s.short_name(), Some("Ödeme"));
    }
}

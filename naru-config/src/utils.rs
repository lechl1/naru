use std::str::FromStr;

use knuffel::errors::DecodeError;
use miette::miette;
use regex::Regex;

mod merge_with;
pub use merge_with::*;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Percent(pub f64);

// MIN and MAX generics are only used during parsing to check the value.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct FloatOrInt<const MIN: i32, const MAX: i32>(pub f64);

/// Flag, with an optional explicit value.
///
/// Intended to be used as an `Option<MaybeBool>` field, as a tri-state:
/// - (missing): unset, `None`
/// - just `field`: set, `Some(true)`
/// - explicitly `field true` or `field false`: set, `Some(true)` or `Some(false)`
#[derive(knuffel::Decode, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag(#[knuffel(argument, default = true)] pub bool);

/// `Regex` that implements `PartialEq` by its string form.
#[derive(Debug, Clone)]
pub struct RegexEq(pub Regex);

impl PartialEq for RegexEq {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for RegexEq {}

impl FromStr for RegexEq {
    type Err = <Regex as FromStr>::Err;

    /// Parses a regex, with CSS-selector-style comma lists. A value containing
    /// commas — e.g. `app-id="firefox, chromium, org.kde.konsole"` — is split
    /// on commas into individual patterns that are OR'd together into a single
    /// regex, so the rule matches any of the listed app-ids/titles. Whitespace
    /// around each entry is trimmed.
    ///
    /// This stays backward-compatible with single regexes that legitimately
    /// contain a comma (e.g. a `{1,3}` quantifier): the split is only used when
    /// *every* comma-separated piece is itself a non-empty valid regex;
    /// otherwise the whole string is parsed as one regex, exactly as before.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains(',') {
            let parts: Vec<&str> = s.split(',').map(str::trim).collect();
            if parts
                .iter()
                .all(|p| !p.is_empty() && Regex::from_str(p).is_ok())
            {
                let joined = parts
                    .iter()
                    .map(|p| format!("(?:{p})"))
                    .collect::<Vec<_>>()
                    .join("|");
                return Regex::from_str(&joined).map(Self);
            }
        }
        Regex::from_str(s).map(Self)
    }
}

impl FromStr for Percent {
    type Err = miette::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((value, empty)) = s.split_once('%') else {
            return Err(miette!("value must end with '%'"));
        };

        if !empty.is_empty() {
            return Err(miette!("trailing characters after '%' are not allowed"));
        }

        let value: f64 = value.parse().map_err(|_| miette!("error parsing value"))?;
        Ok(Percent(value / 100.))
    }
}

impl<const MIN: i32, const MAX: i32> MergeWith<FloatOrInt<MIN, MAX>> for f64 {
    fn merge_with(&mut self, part: &FloatOrInt<MIN, MAX>) {
        *self = part.0;
    }
}

impl MergeWith<Flag> for bool {
    fn merge_with(&mut self, part: &Flag) {
        *self = part.0;
    }
}

impl<S: knuffel::traits::ErrorSpan, const MIN: i32, const MAX: i32> knuffel::DecodeScalar<S>
    for FloatOrInt<MIN, MAX>
{
    fn type_check(
        type_name: &Option<knuffel::span::Spanned<knuffel::ast::TypeName, S>>,
        ctx: &mut knuffel::decode::Context<S>,
    ) {
        if let Some(type_name) = &type_name {
            ctx.emit_error(DecodeError::unexpected(
                type_name,
                "type name",
                "no type name expected for this node",
            ));
        }
    }

    fn raw_decode(
        val: &knuffel::span::Spanned<knuffel::ast::Literal, S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        match &**val {
            knuffel::ast::Literal::Int(ref value) => match value.try_into() {
                Ok(v) => {
                    if (MIN..=MAX).contains(&v) {
                        Ok(FloatOrInt(f64::from(v)))
                    } else {
                        ctx.emit_error(DecodeError::conversion(
                            val,
                            format!("value must be between {MIN} and {MAX}"),
                        ));
                        Ok(FloatOrInt::default())
                    }
                }
                Err(e) => {
                    ctx.emit_error(DecodeError::conversion(val, e));
                    Ok(FloatOrInt::default())
                }
            },
            knuffel::ast::Literal::Decimal(ref value) => match value.try_into() {
                Ok(v) => {
                    if (f64::from(MIN)..=f64::from(MAX)).contains(&v) {
                        Ok(FloatOrInt(v))
                    } else {
                        ctx.emit_error(DecodeError::conversion(
                            val,
                            format!("value must be between {MIN} and {MAX}"),
                        ));
                        Ok(FloatOrInt::default())
                    }
                }
                Err(e) => {
                    ctx.emit_error(DecodeError::conversion(val, e));
                    Ok(FloatOrInt::default())
                }
            },
            _ => {
                ctx.emit_error(DecodeError::unsupported(
                    val,
                    "Unsupported value, only numbers are recognized",
                ));
                Ok(FloatOrInt::default())
            }
        }
    }
}

pub fn expect_only_children<S>(
    node: &knuffel::ast::SpannedNode<S>,
    ctx: &mut knuffel::decode::Context<S>,
) where
    S: knuffel::traits::ErrorSpan,
{
    if let Some(type_name) = &node.type_name {
        ctx.emit_error(DecodeError::unexpected(
            type_name,
            "type name",
            "no type name expected for this node",
        ));
    }

    for val in node.arguments.iter() {
        ctx.emit_error(DecodeError::unexpected(
            &val.literal,
            "argument",
            "no arguments expected for this node",
        ))
    }

    for name in node.properties.keys() {
        ctx.emit_error(DecodeError::unexpected(
            name,
            "property",
            "no properties expected for this node",
        ))
    }
}

pub fn parse_arg_node<S: knuffel::traits::ErrorSpan, T: knuffel::traits::DecodeScalar<S>>(
    name: &str,
    node: &knuffel::ast::SpannedNode<S>,
    ctx: &mut knuffel::decode::Context<S>,
) -> Result<T, DecodeError<S>> {
    let mut iter_args = node.arguments.iter();
    let val = iter_args.next().ok_or_else(|| {
        DecodeError::missing(node, format!("additional argument `{name}` is required"))
    })?;

    let value = knuffel::traits::DecodeScalar::decode(val, ctx)?;

    if let Some(val) = iter_args.next() {
        ctx.emit_error(DecodeError::unexpected(
            &val.literal,
            "argument",
            "unexpected argument",
        ));
    }
    for name in node.properties.keys() {
        ctx.emit_error(DecodeError::unexpected(
            name,
            "property",
            format!("unexpected property `{}`", name.escape_default()),
        ));
    }
    for child in node.children() {
        ctx.emit_error(DecodeError::unexpected(
            child,
            "node",
            format!("unexpected node `{}`", child.node_name.escape_default()),
        ));
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_eq_comma_list_matches_any() {
        // CSS-selector-style list: matches any listed app-id, none else.
        let re = RegexEq::from_str("firefox, org.kde.konsole, kitty").unwrap();
        assert!(re.0.is_match("firefox"));
        assert!(re.0.is_match("org.kde.konsole"));
        assert!(re.0.is_match("kitty"));
        assert!(!re.0.is_match("chromium"));
    }

    #[test]
    fn regex_eq_single_pattern_is_unchanged() {
        // A value with no comma is parsed verbatim (no OR-wrapping), so the
        // stored regex string — which PartialEq and the snapshot tests compare
        // — is byte-for-byte the input.
        let re = RegexEq::from_str(".*alacritty").unwrap();
        assert_eq!(re.0.as_str(), ".*alacritty");
        assert!(re.0.is_match("org.alacritty"));
    }

    #[test]
    fn regex_eq_quantifier_comma_falls_back_to_single_regex() {
        // `a{1,2}` contains a comma but is a single regex; the split must not
        // shred it — every piece isn't a valid regex, so we fall back.
        let re = RegexEq::from_str("^a{1,2}$").unwrap();
        assert!(re.0.is_match("a"));
        assert!(re.0.is_match("aa"));
        assert!(!re.0.is_match("aaa"));
    }
}

// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::types::{Atom, Expr, TargetEnum};
use nom::{
    branch::alt,
    bytes::complete::{escaped_transform, tag},
    character::complete::{char, none_of, space0},
    combinator::{all_consuming, cut, map, opt},
    error::ErrorKind,
    multi::separated_list,
    sequence::{delimited, preceded, separated_pair, terminated},
    AsChar, IResult, InputTakeAtPosition,
};
use std::{error, fmt};

/// An error that occurred while attempting to parse a target specification.
#[derive(Clone, Debug, PartialEq)]
pub struct ParseError(pub(crate) nom::Err<(String, nom::error::ErrorKind)>);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "target spec parsing failed")
    }
}

impl error::Error for ParseError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.0)
    }
}

fn identifier(input: &str) -> IResult<&str, Atom> {
    let (i, start) = input
        .split_at_position1_complete(|item| !item.is_alpha() && item != '_', ErrorKind::Alpha)?;
    let (i, rest) = i.split_at_position_complete(|item| !item.is_alphanum() && item != '_')?;
    Ok((i, Atom::Ident([start, rest].concat())))
}

fn value(input: &str) -> IResult<&str, Atom> {
    map(
        preceded(
            char('"'),
            cut(terminated(
                opt(escaped_transform(
                    none_of("\\\""),
                    '\\',
                    alt((tag("\\"), tag("\""))),
                )),
                char('"'),
            )),
        ),
        |s| Atom::Value(s.unwrap_or_else(|| "".to_string())),
    )(input)
}

fn any(input: &str) -> IResult<&str, Expr> {
    map(
        delimited(
            space0,
            preceded(
                tag("any"),
                delimited(char('('), separated_list(char(','), expression), char(')')),
            ),
            space0,
        ),
        Expr::Any,
    )(input)
}

fn all(input: &str) -> IResult<&str, Expr> {
    map(
        delimited(
            space0,
            preceded(
                tag("all"),
                delimited(char('('), separated_list(char(','), expression), char(')')),
            ),
            space0,
        ),
        Expr::All,
    )(input)
}

fn not(input: &str) -> IResult<&str, Expr> {
    map(
        delimited(
            space0,
            preceded(tag("not"), delimited(char('('), expression, char(')'))),
            space0,
        ),
        |e| Expr::Not(Box::new(e)),
    )(input)
}

fn test_set(input: &str) -> IResult<&str, Expr> {
    map(delimited(space0, identifier, opt(space0)), Expr::TestSet)(input)
}

fn test_equal(input: &str) -> IResult<&str, Expr> {
    map(
        delimited(
            space0,
            separated_pair(identifier, delimited(space0, char('='), space0), value),
            space0,
        ),
        Expr::TestEqual,
    )(input)
}

fn expression(input: &str) -> IResult<&str, Expr> {
    alt((any, all, not, test_equal, test_set))(input)
}

fn spec(input: &str) -> IResult<&str, TargetEnum> {
    map(
        delimited(
            space0,
            preceded(
                tag("cfg"),
                // This "cut" is here to prevent backtracking and trying out the "triple" parser if
                // the initial "cfg" is recognized.
                cut(delimited(char('('), expression, char(')'))),
            ),
            space0,
        ),
        TargetEnum::Spec,
    )(input)
}

fn triple_string(input: &str) -> IResult<&str, &str> {
    input.split_at_position1_complete(
        |item| !item.is_alphanum() && item != '_' && item != '-',
        ErrorKind::AlphaNumeric,
    )
}

fn triple(input: &str) -> IResult<&str, TargetEnum> {
    map(triple_string, |s| TargetEnum::Triple(s.to_string()))(input)
}

fn target(input: &str) -> IResult<&str, TargetEnum> {
    alt((spec, triple))(input)
}

pub(crate) fn parse_impl(
    input: &str,
) -> Result<TargetEnum, nom::Err<(&str, nom::error::ErrorKind)>> {
    let (_, expr) = all_consuming(target)(input)?;
    Ok(expr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_id() {
        assert_eq!(
            identifier("target"),
            Ok(("", Atom::Ident("target".to_string()))),
        );
    }

    #[test]
    fn test_id_underscore() {
        assert_eq!(
            identifier("target_os"),
            Ok(("", Atom::Ident("target_os".to_string()))),
        );
    }

    #[test]
    fn test_value() {
        assert_eq!(
            value("\"bar \\\" foo\""),
            Ok(("", Atom::Value("bar \" foo".to_string()))),
        );
    }

    #[test]
    fn test_empty_value() {
        assert_eq!(value("\"\""), Ok(("", Atom::Value("".to_string()))));
    }

    #[test]
    fn test_any() {
        assert_eq!(
            any("any(unix, target_os = \"redox\")"),
            Ok((
                "",
                Expr::Any(vec![
                    Expr::TestSet(Atom::Ident("unix".to_string())),
                    Expr::TestEqual((
                        Atom::Ident("target_os".to_string()),
                        Atom::Value("redox".to_string())
                    ))
                ])
            )),
        );
    }

    #[test]
    fn test_test_equal() {
        assert_eq!(
            test_equal("foo = \"bar\""),
            Ok((
                "",
                Expr::TestEqual((
                    Atom::Ident("foo".to_string()),
                    Atom::Value("bar".to_string())
                ))
            )),
        );
    }
}

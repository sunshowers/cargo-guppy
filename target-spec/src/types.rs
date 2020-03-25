// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::parser::parse_impl;
use crate::platform::Platform;
use crate::{eval_target, EvalError, ParseError};
use std::str::FromStr;

/// A parsed target specification or triple, as found in a `Cargo.toml` file.
///
/// Use the `FromStr` implementation or `str::parse` to obtain an instance.
///
/// ## Examples
///
/// ```
/// use target_spec::{Platform, TargetFeatures, TargetSpec};
///
/// let i686_windows = Platform::new("i686-pc-windows-gnu", TargetFeatures::All).unwrap();
/// let x86_64_mac = Platform::new("x86_64-apple-darwin", TargetFeatures::none()).unwrap();
/// let i686_linux = Platform::new("i686-unknown-linux-gnu", TargetFeatures::features(&["sse2"])).unwrap();
///
/// let spec: TargetSpec = "cfg(any(windows, target_arch = \"x86_64\"))".parse().unwrap();
/// assert!(spec.eval(&i686_windows).unwrap(), "i686 Windows");
/// assert!(spec.eval(&x86_64_mac).unwrap(), "x86_64 MacOS");
/// assert!(!spec.eval(&i686_linux).unwrap(), "i686 Linux (should not match)");
///
/// let spec: TargetSpec = "cfg(any(target_feature = \"sse2\", target_feature = \"sse\"))".parse().unwrap();
/// assert!(spec.eval(&i686_windows).unwrap(), "i686 Windows matches all features");
/// assert!(!spec.eval(&x86_64_mac).unwrap(), "x86_64 MacOS matches no features");
/// assert!(spec.eval(&i686_linux).unwrap(), "i686 Linux matches some features");
/// ```
#[derive(Clone, Debug)]
pub struct TargetSpec {
    target: TargetEnum,
}

impl TargetSpec {
    /// Evaluates this specification against the given platform triple, defaulting to accepting all
    /// target features.
    #[inline]
    pub fn eval(&self, platform: &Platform<'_>) -> Result<bool, EvalError> {
        eval_target(&self.target, platform)
    }
}

impl FromStr for TargetSpec {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match parse_impl(input) {
            Ok(target) => Ok(Self { target }),
            Err(err) => Err(ParseError(err.to_owned())),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Atom {
    Ident(String),
    Value(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Any(Vec<Expr>),
    All(Vec<Expr>),
    Not(Box<Expr>),
    TestSet(Atom),
    TestEqual((Atom, Atom)),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TargetEnum {
    Triple(String),
    Spec(Expr),
}

// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;

/// A platform to evaluate target specs against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Platform<'a> {
    platform: &'a platforms::Platform,
    target_features: TargetFeatures<'a>,
}

impl<'a> Platform<'a> {
    /// Creates a new `Platform` from the given triple and target features.
    ///
    /// Returns `None` if this platform wasn't found in the database.
    pub fn new(triple: impl AsRef<str>, target_features: TargetFeatures<'a>) -> Option<Self> {
        Some(Self {
            platform: platforms::find(triple)?,
            target_features,
        })
    }

    /// Returns the target triple for this platform.
    pub fn triple(&self) -> &'static str {
        self.platform.target_triple
    }

    /// Returns the underlying `platforms::Platform`.
    ///
    /// This is not exported since semver compatibility isn't guaranteed.
    pub(crate) fn platform(&self) -> &'a platforms::Platform {
        self.platform
    }

    /// Returns the set of target features for this platform.
    pub fn target_features(&self) -> &TargetFeatures<'a> {
        &self.target_features
    }
}

/// A set of target features to match.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TargetFeatures<'a> {
    /// Match all target features.
    All,
    /// Only match the specified features.
    Features(HashSet<&'a str>),
}

impl<'a> TargetFeatures<'a> {
    /// Creates a new `TargetFeatures` which matches some features.
    pub fn features(features: &[&'a str]) -> Self {
        TargetFeatures::Features(features.into_iter().copied().collect())
    }

    /// Creates a new `TargetFeatures` which doesn't match any features.
    pub fn none() -> Self {
        TargetFeatures::Features(HashSet::new())
    }

    /// Returns true if the given feature is known to
    pub fn matches(&self, feature: &str) -> bool {
        match self {
            TargetFeatures::All => true,
            TargetFeatures::Features(features) => features.contains(feature),
        }
    }
}

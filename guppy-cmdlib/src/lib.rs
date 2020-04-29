// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Support for CLI operations with guppy, with structopt integration.
//!
//! This library allows translating command-line arguments into guppy's data structures.

use anyhow::Result;
use guppy::graph::feature::{
    all_filter, default_filter, feature_filter, none_filter, FeatureFilter, FeatureQuery,
};
use guppy::graph::PackageGraph;
use structopt::StructOpt;

/// Support for packages and features.
///
/// The options here mirror Cargo's.
#[derive(Debug, StructOpt)]
pub struct PackagesAndFeatures {
    #[structopt(long = "package", short = "p", number_of_values = 1)]
    /// Packages to start the query from (default: entire workspace)
    pub packages: Vec<String>,

    // TODO: support --workspace and --exclude
    /// List of features to activate across all packages
    #[structopt(long = "features", use_delimiter = true)]
    pub features: Vec<String>,

    /// Activate all available features
    #[structopt(long = "all-features")]
    pub all_features: bool,

    /// Do not activate the `default` feature
    #[structopt(long = "no-default-features")]
    pub no_default_features: bool,
}

impl PackagesAndFeatures {
    /// Evaluates this struct against the given graph, and converts it into a `FeatureQuery`.
    pub fn make_feature_query<'g>(&self, graph: &'g PackageGraph) -> Result<FeatureQuery<'g>> {
        let package_query = if self.packages.is_empty() {
            graph.query_workspace()
        } else {
            graph.query_workspace_names(self.packages.iter().map(|s| s.as_str()))?
        };

        let base_filter: Box<dyn FeatureFilter> =
            match (self.all_features, self.no_default_features) {
                (true, _) => Box::new(all_filter()),
                (false, false) => Box::new(default_filter()),
                (false, true) => Box::new(none_filter()),
            };
        // TODO: support package/feature format
        // TODO: support feature name validation similar to cargo
        let feature_filter = feature_filter(base_filter, self.features.iter().map(|s| s.as_str()));

        Ok(graph
            .feature_graph()
            .query_packages(&package_query, feature_filter))
    }
}

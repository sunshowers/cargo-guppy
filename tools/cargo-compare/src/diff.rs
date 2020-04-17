// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use cargo::core::{PackageIdSpec, Workspace};

use crate::type_conversions::ToGuppy;
use crate::GuppyCargoCommon;
use anyhow::Result;
use cargo::core::compiler::{CompileKind, CompileTarget, RustcTargetData};
use cargo::core::resolver::features::FeaturesFor;
use cargo::core::resolver::{HasDevUnits, ResolveOpts};
use cargo::ops::resolve_ws_with_opts;
use cargo::Config;
use diffus::{edit, Diffable};
use guppy::graph::cargo::{CargoOptions, CargoSet};
use guppy::graph::feature::FeatureSet;
use guppy::graph::{DependencyDirection, PackageGraph};
use guppy::{MetadataCommand, PackageId};
use std::collections::{BTreeMap, BTreeSet};
use structopt::StructOpt;

/// Options for cargo/guppy comparisons.
#[derive(Debug, StructOpt)]
pub struct DiffOpts {
    #[structopt(flatten)]
    common: GuppyCargoCommon,
}

impl DiffOpts {
    /// Executes this command.
    pub fn exec(self) -> Result<()> {
        let cargo_map = self.resolve_cargo()?;
        let guppy_map = self.resolve_guppy()?;

        println!("** target diff:");
        print_diff(&guppy_map.target_map, &cargo_map.target_map);

        println!("\n** host diff:");
        print_diff(&guppy_map.host_map, &cargo_map.host_map);

        Ok(())
    }

    pub fn resolve_cargo(&self) -> Result<FeatureMap> {
        let config = self.make_cargo_config()?;
        let cwd = config.cwd();
        let manifest_path = cwd.join("Cargo.toml");
        let workspace = Workspace::new(&manifest_path, &config)?;

        let compile_kind = match &self.common.target_platform {
            Some(platform) => CompileKind::Target(CompileTarget::new(platform)?),
            None => CompileKind::Host,
        };
        let target_data = RustcTargetData::new(&workspace, compile_kind)?;

        let resolve_opts = ResolveOpts::new(
            // dev_deps is always set to true regardless of include_dev (note that it is only set
            // to false by `cargo install` or -Zavoid-dev-deps).
            true,
            &self.common.pf.features,
            self.common.pf.all_features,
            !self.common.pf.no_default_features,
        );
        let packages = &self.common.pf.packages;
        let specs: Vec<_> = if packages.is_empty() {
            // Pass in the entire workspace.
            workspace
                .members()
                .map(|package| PackageIdSpec::from_package_id(package.package_id()))
                .collect()
        } else {
            packages
                .iter()
                .map(|spec| PackageIdSpec::parse(&spec))
                .collect::<Result<_>>()?
        };

        let ws_resolve = resolve_ws_with_opts(
            &workspace,
            &target_data,
            compile_kind,
            &resolve_opts,
            &specs,
            if self.common.include_dev {
                HasDevUnits::Yes
            } else {
                HasDevUnits::No
            },
        )?;

        let targeted_resolve = ws_resolve.targeted_resolve;
        let resolved_features = ws_resolve.resolved_features;

        let mut target_map = BTreeMap::new();
        let mut host_map = BTreeMap::new();
        for pkg_id in targeted_resolve.iter() {
            // Note that for the V1 resolver the maps are going to be identical, since
            // platform-specific filtering happens much later in the process.
            let target_features =
                resolved_features.activated_features(pkg_id, FeaturesFor::NormalOrDev);
            target_map.insert(pkg_id.to_guppy(), target_features.to_guppy());
            let host_features = resolved_features.activated_features(pkg_id, FeaturesFor::BuildDep);
            host_map.insert(pkg_id.to_guppy(), host_features.to_guppy());
        }

        Ok(FeatureMap {
            target_map,
            host_map,
        })
    }

    pub fn resolve_guppy(&self) -> Result<FeatureMap> {
        let mut metadata_cmd = MetadataCommand::new();
        let package_graph = PackageGraph::from_command(&mut metadata_cmd)?;

        let feature_query = self.common.pf.make_feature_query(&package_graph)?;
        let cargo_opts = CargoOptions::new()
            .with_dev_deps(self.common.include_dev)
            // Cargo's V1 resolver does filtering after considering the platform.
            // XXX change this for the V2 resolver.
            .with_host_platform(None)
            .with_target_platform(None);
        let cargo_set = feature_query.resolve_cargo(&cargo_opts)?;

        // XXX V1 resolver requires merging maps.
        Ok(FeatureMap::from_guppy(&cargo_set, true))
    }

    // ---
    // Helper methods
    // ---

    fn make_cargo_config(&self) -> Result<Config> {
        let mut config = Config::default()?;

        // Prevent cargo from accessing the network.
        let frozen = true;
        let locked = true;
        let offline = true;

        // TODO: set unstable flag for V2 resolver
        let unstable_flags = &[];

        config.configure(
            2,
            false,
            None,
            frozen,
            locked,
            offline,
            &None,
            unstable_flags,
            &[],
        )?;

        Ok(config)
    }
}

#[derive(Clone, Debug)]
pub struct FeatureMap {
    pub target_map: BTreeMap<PackageId, BTreeSet<String>>,
    pub host_map: BTreeMap<PackageId, BTreeSet<String>>,
}

impl FeatureMap {
    fn from_guppy(cargo_set: &CargoSet<'_>, merge_maps: bool) -> Self {
        if merge_maps {
            let unified_set = cargo_set.target_features().union(cargo_set.host_features());
            let unified_map = Self::feature_set_to_map(&unified_set);
            Self {
                target_map: unified_map.clone(),
                host_map: unified_map,
            }
        } else {
            let target_map = Self::feature_set_to_map(cargo_set.target_features());
            let host_map = Self::feature_set_to_map(cargo_set.host_features());
            Self {
                target_map,
                host_map,
            }
        }
    }

    fn feature_set_to_map(feature_set: &FeatureSet<'_>) -> BTreeMap<PackageId, BTreeSet<String>> {
        feature_set
            .packages_with_features::<Vec<_>>(DependencyDirection::Forward)
            .map(|(package, features)| {
                let features = features
                    .into_iter()
                    .filter_map(|f| f.map(|s| s.to_string()))
                    .collect();
                (package.id().clone(), features)
            })
            .collect()
    }
}

fn print_diff(
    a: &BTreeMap<PackageId, BTreeSet<String>>,
    b: &BTreeMap<PackageId, BTreeSet<String>>,
) {
    if let edit::Edit::Change(diff) = a.diff(&b) {
        for (pkg_id, diff) in diff {
            if !diff.is_copy() {
                println!("{}: {:?}", pkg_id, diff);
            }
        }
    }
}

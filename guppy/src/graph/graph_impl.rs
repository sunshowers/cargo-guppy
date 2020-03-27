// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::graph::feature::{FeatureGraphImpl, FeatureId, FeatureNode};
use crate::graph::{cargo_version_matches, kind_str, Cycles, DependencyDirection, PackageIx};
use crate::petgraph_support::scc::Sccs;
use crate::{Error, JsonValue, Metadata, MetadataCommand, PackageId};
use cargo_metadata::{DependencyKind, NodeDep};
use fixedbitset::FixedBitSet;
use indexmap::IndexMap;
use once_cell::sync::OnceCell;
use petgraph::algo::{has_path_connecting, DfsSpace};
use petgraph::prelude::*;
use semver::{Version, VersionReq};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::iter;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use target_spec::{EvalError, TargetSpec};

/// A graph of packages and dependencies between them, parsed from metadata returned by `cargo
/// metadata`.
///
/// For examples on how to use `PackageGraph`, see
/// [the `examples` directory](https://github.com/calibra/cargo-guppy/tree/master/guppy/examples)
/// in this crate.
#[derive(Clone, Debug)]
pub struct PackageGraph {
    // Source of truth data.
    pub(super) dep_graph: Graph<PackageId, DependencyEdge, Directed, PackageIx>,
    // The strongly connected components of the graph, computed on demand.
    pub(super) sccs: OnceCell<Sccs<PackageIx>>,
    // Feature graph, computed on demand.
    pub(super) feature_graph: OnceCell<FeatureGraphImpl>,
    // XXX Should this be in an Arc for quick cloning? Not clear how this would work with node
    // filters though.
    pub(super) data: PackageGraphData,
}

/// Per-package data for a PackageGraph instance.
#[derive(Clone, Debug)]
pub struct PackageGraphData {
    pub(super) packages: HashMap<PackageId, PackageMetadata>,
    pub(super) workspace: Workspace,
}

impl PackageGraph {
    /// Constructs a package graph from the given command.
    pub fn from_command(command: &mut MetadataCommand) -> Result<Self, Error> {
        Self::new(command.exec().map_err(Error::CommandError)?)
    }

    /// Constructs a package graph from the given JSON output of `cargo metadata`.
    pub fn from_json(json: impl AsRef<str>) -> Result<Self, Error> {
        let metadata = serde_json::from_str(json.as_ref()).map_err(Error::MetadataParseError)?;
        Self::new(metadata)
    }

    /// Constructs a package graph from the given metadata.
    pub fn new(metadata: Metadata) -> Result<Self, Error> {
        Self::build(metadata)
    }

    /// Verifies internal invariants on this graph. Not part of the documented API.
    #[doc(hidden)]
    pub fn verify(&self) -> Result<(), Error> {
        // Graph structure checks.
        let node_count = self.dep_graph.node_count();
        let package_count = self.data.packages.len();
        if node_count != package_count {
            return Err(Error::PackageGraphInternalError(format!(
                "number of nodes = {} different from packages = {}",
                node_count, package_count,
            )));
        }

        // TODO: The dependency graph can have cyclic dev-dependencies. Add a check to ensure that
        // the graph without any dev-only dependencies is acyclic.

        let workspace = self.workspace();
        let workspace_ids: HashSet<_> = workspace.member_ids().collect();

        for metadata in self.packages() {
            let package_id = metadata.id();

            match metadata.workspace_path() {
                Some(workspace_path) => {
                    // This package is in the workspace, so the workspace should have information
                    // about it.
                    let workspace_id = workspace.member_by_path(workspace_path);
                    if workspace_id != Some(package_id) {
                        return Err(Error::PackageGraphInternalError(format!(
                            "package {} has workspace path {:?} but query by path returned {:?}",
                            package_id, workspace_path, workspace_id,
                        )));
                    }
                }
                None => {
                    // This package is not in the workspace.
                    if workspace_ids.contains(package_id) {
                        return Err(Error::PackageGraphInternalError(format!(
                            "package {} has no workspace path but is in workspace",
                            package_id,
                        )));
                    }
                }
            }

            for dep in self.dep_links_ixs_directed(metadata.package_ix, Outgoing) {
                let to_id = dep.to.id();
                let to_version = dep.to.version();

                let version_check = |dep_metadata: &DependencyMetadata, kind: DependencyKind| {
                    let req = dep_metadata.version_req();
                    // A requirement of "*" filters out pre-release versions with the semver crate,
                    // but cargo accepts them.
                    // See https://github.com/steveklabnik/semver/issues/98.
                    if cargo_version_matches(req, to_version) {
                        Ok(())
                    } else {
                        Err(Error::PackageGraphInternalError(format!(
                            "{} -> {} ({}): version ({}) doesn't match requirement ({:?})",
                            package_id,
                            to_id,
                            kind_str(kind),
                            to_version,
                            req,
                        )))
                    }
                };

                // Two invariants:
                // 1. At least one of the edges should be specified.
                // 2. The specified package should match the version dependency.
                let mut edge_set = false;
                if let Some(dep_metadata) = &dep.edge.normal {
                    edge_set = true;
                    version_check(dep_metadata, DependencyKind::Normal)?;
                }
                if let Some(dep_metadata) = &dep.edge.build {
                    edge_set = true;
                    version_check(dep_metadata, DependencyKind::Build)?;
                }
                if let Some(dep_metadata) = &dep.edge.dev {
                    edge_set = true;
                    version_check(dep_metadata, DependencyKind::Development)?;
                }

                if !edge_set {
                    return Err(Error::PackageGraphInternalError(format!(
                        "{} -> {}: no edge info found",
                        package_id, to_id,
                    )));
                }
            }
        }

        // Constructing the feature graph may cause panics to happen.
        self.feature_graph();

        Ok(())
    }

    /// Returns information about the workspace.
    pub fn workspace(&self) -> &Workspace {
        &self.data.workspace()
    }

    /// Returns an iterator over all the package IDs in this graph.
    pub fn package_ids(&self) -> impl Iterator<Item = &PackageId> + ExactSizeIterator {
        self.data.package_ids()
    }

    /// Returns an iterator over all the packages in this graph.
    pub fn packages(&self) -> impl Iterator<Item = &PackageMetadata> + ExactSizeIterator {
        self.data.packages()
    }

    /// Returns the number of packages in this graph.
    pub fn package_count(&self) -> usize {
        // This can be obtained in two different ways: self.dep_graph.node_count() or
        // self.data.packages.len(). verify() checks that they return the same results.
        //
        // Use this way for symmetry with link_count below (which can only be obtained through the
        // graph).
        self.dep_graph.node_count()
    }

    /// Returns the number of links in this graph.
    pub fn link_count(&self) -> usize {
        self.dep_graph.edge_count()
    }

    /// Returns the metadata for the given package ID.
    pub fn metadata(&self, package_id: &PackageId) -> Option<&PackageMetadata> {
        self.data.metadata(package_id)
    }

    /// Keeps all edges that return true from the visit closure, and removes the others.
    ///
    /// The order edges are visited is not specified.
    pub fn retain_edges<F>(&mut self, visit: F)
    where
        F: Fn(&PackageGraphData, DependencyLink<'_>) -> bool,
    {
        let data = &self.data;
        self.dep_graph.retain_edges(|frozen_graph, edge_idx| {
            // This could use self.edge_to_dep for part of it but that that isn't compatible with
            // the borrow checker :(
            let (source, target) = frozen_graph
                .edge_endpoints(edge_idx)
                .expect("edge_idx should be valid");
            let from = &data.packages[&frozen_graph[source]];
            let to = &data.packages[&frozen_graph[target]];
            let edge = &frozen_graph[edge_idx];
            visit(data, DependencyLink { from, to, edge })
        });

        self.invalidate_caches();
    }

    /// Creates a new cache for `depends_on` queries.
    ///
    /// The cache is optional but can speed up some queries.
    pub fn new_depends_cache(&self) -> DependsCache {
        DependsCache::new(self)
    }

    /// Returns true if `package_a` depends (directly or indirectly) on `package_b`.
    ///
    /// In other words, this returns true if `package_b` is a (possibly transitive) dependency of
    /// `package_a`.
    ///
    /// For repeated queries, consider using `new_depends_cache` to speed up queries.
    pub fn depends_on(&self, package_a: &PackageId, package_b: &PackageId) -> Result<bool, Error> {
        let mut depends_cache = self.new_depends_cache();
        depends_cache.depends_on(package_a, package_b)
    }

    /// Returns information about dependency cycles in this graph.
    ///
    /// For more information, see the documentation for `Cycles`.
    pub fn cycles(&self) -> Cycles {
        Cycles::new(self)
    }

    // ---
    // Dependency traversals
    // ---

    /// Returns the direct dependencies for the given package ID in the specified direction.
    pub fn dep_links_directed<'g>(
        &'g self,
        package_id: &PackageId,
        dep_direction: DependencyDirection,
    ) -> Option<impl Iterator<Item = DependencyLink<'g>> + 'g> {
        self.dep_links_impl(package_id, dep_direction.into())
    }

    /// Returns the direct dependencies for the given package ID.
    pub fn dep_links<'g>(
        &'g self,
        package_id: &PackageId,
    ) -> Option<impl Iterator<Item = DependencyLink<'g>> + 'g> {
        self.dep_links_impl(package_id, Outgoing)
    }

    /// Returns the direct reverse dependencies for the given package ID.
    pub fn reverse_dep_links<'g>(
        &'g self,
        package_id: &PackageId,
    ) -> Option<impl Iterator<Item = DependencyLink<'g>> + 'g> {
        self.dep_links_impl(package_id, Incoming)
    }

    fn dep_links_impl<'g>(
        &'g self,
        package_id: &PackageId,
        dir: Direction,
    ) -> Option<impl Iterator<Item = DependencyLink<'g>> + 'g> {
        self.metadata(package_id)
            .map(|metadata| self.dep_links_ixs_directed(metadata.package_ix, dir))
    }

    fn dep_links_ixs_directed<'g>(
        &'g self,
        package_ix: NodeIndex<PackageIx>,
        dir: Direction,
    ) -> impl Iterator<Item = DependencyLink<'g>> + 'g {
        self.dep_graph
            .edges_directed(package_ix, dir)
            .map(move |edge| self.edge_to_link(edge.source(), edge.target(), edge.weight()))
    }

    // For more traversals, see select.rs.

    // ---
    // Helper methods
    // ---

    /// Constructs a map of strongly connected components for this graph.
    pub(super) fn sccs(&self) -> &Sccs<PackageIx> {
        self.sccs.get_or_init(|| Sccs::new(&self.dep_graph))
    }

    /// Invalidates internal caches. Meant to be called whenever the graph is mutated.
    pub(super) fn invalidate_caches(&mut self) {
        mem::replace(&mut self.sccs, OnceCell::new());
        mem::replace(&mut self.feature_graph, OnceCell::new());
    }

    /// Returns the inner dependency graph.
    ///
    /// Should this be exposed publicly? Not sure.
    pub(super) fn dep_graph(&self) -> &Graph<PackageId, DependencyEdge, Directed, PackageIx> {
        &self.dep_graph
    }

    /// Maps an edge source, target and weight to a dependency link.
    pub(super) fn edge_to_link<'g>(
        &'g self,
        source: NodeIndex<PackageIx>,
        target: NodeIndex<PackageIx>,
        edge: &'g DependencyEdge,
    ) -> DependencyLink<'g> {
        // Note: It would be really lovely if this could just take in any EdgeRef with the right
        // parameters, but 'weight' wouldn't live long enough unfortunately.
        //
        // https://docs.rs/petgraph/0.4.13/petgraph/graph/struct.EdgeReference.html#method.weight
        // is defined separately for the same reason.
        let from = self
            .metadata(&self.dep_graph[source])
            .expect("'from' should have associated metadata");
        let to = self
            .metadata(&self.dep_graph[target])
            .expect("'to' should have associated metadata");
        DependencyLink { from, to, edge }
    }

    /// Maps an iterator of package IDs to their internal graph node indexes.
    pub(super) fn package_ixs<'g, 'a, B>(
        &'g self,
        package_ids: impl IntoIterator<Item = &'a PackageId>,
    ) -> Result<B, Error>
    where
        B: iter::FromIterator<NodeIndex<PackageIx>>,
    {
        package_ids
            .into_iter()
            .map(|package_id| {
                self.package_ix(package_id)
                    .ok_or_else(|| Error::UnknownPackageId(package_id.clone()))
            })
            .collect()
    }

    /// Maps a package ID to its internal graph node index.
    pub(super) fn package_ix(&self, package_id: &PackageId) -> Option<NodeIndex<PackageIx>> {
        self.metadata(package_id)
            .map(|metadata| metadata.package_ix)
    }
}

impl PackageGraphData {
    /// Returns information about the workspace.
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// Returns an iterator over all the package IDs in this graph.
    pub fn package_ids(&self) -> impl Iterator<Item = &PackageId> + ExactSizeIterator {
        self.packages.keys()
    }

    /// Returns an iterator over all the packages in this graph.
    pub fn packages(&self) -> impl Iterator<Item = &PackageMetadata> + ExactSizeIterator {
        self.packages.values()
    }

    /// Returns the metadata for the given package ID.
    pub fn metadata(&self, package_id: &PackageId) -> Option<&PackageMetadata> {
        self.packages.get(package_id)
    }
}

/// An optional cache used to speed up `depends_on` queries.
///
/// Created with `PackageGraph::new_depends_cache()`.
#[derive(Clone, Debug)]
pub struct DependsCache<'g> {
    package_graph: &'g PackageGraph,
    dfs_space: DfsSpace<NodeIndex<PackageIx>, FixedBitSet>,
}

impl<'g> DependsCache<'g> {
    /// Creates a new cache for `depends_on` queries for this package graph.
    ///
    /// This holds a shared reference to the package graph. This is to ensure that the cache is
    /// invalidated if the package graph is mutated.
    pub fn new(package_graph: &'g PackageGraph) -> Self {
        Self {
            package_graph,
            dfs_space: DfsSpace::new(&package_graph.dep_graph),
        }
    }

    /// Returns true if `package_a` depends (directly or indirectly) on `package_b`.
    ///
    /// In other words, this returns true if `package_b` is a (possibly transitive) dependency of
    /// `package_a`.
    pub fn depends_on(
        &mut self,
        package_a: &PackageId,
        package_b: &PackageId,
    ) -> Result<bool, Error> {
        let a_ix = self
            .package_graph
            .package_ix(package_a)
            .ok_or_else(|| Error::UnknownPackageId(package_a.clone()))?;
        let b_ix = self
            .package_graph
            .package_ix(package_b)
            .ok_or_else(|| Error::UnknownPackageId(package_b.clone()))?;
        Ok(has_path_connecting(
            self.package_graph.dep_graph(),
            a_ix,
            b_ix,
            Some(&mut self.dfs_space),
        ))
    }
}

/// Information about a workspace, parsed from metadata returned by `cargo metadata`.
///
/// For more about workspaces, see
/// [Cargo Workspaces](https://doc.rust-lang.org/book/ch14-03-cargo-workspaces.html) in *The Rust
/// Programming Language*.
#[derive(Clone, Debug)]
pub struct Workspace {
    pub(super) root: PathBuf,
    // This is a BTreeMap to allow presenting data in sorted order.
    pub(super) members_by_path: BTreeMap<PathBuf, PackageId>,
}

impl Workspace {
    /// Returns the workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns an iterator over of workspace paths and members, sorted by the path they're in.
    pub fn members(&self) -> impl Iterator<Item = (&Path, &PackageId)> + ExactSizeIterator {
        self.members_by_path
            .iter()
            .map(|(path, id)| (path.as_path(), id))
    }

    /// Returns an iterator over package IDs for workspace members. The package IDs will be returned
    /// in the same order as `members`, sorted by the path they're in.
    pub fn member_ids(&self) -> impl Iterator<Item = &PackageId> + ExactSizeIterator {
        self.members_by_path.iter().map(|(_path, id)| id)
    }

    /// Maps the given path to the corresponding workspace member.
    pub fn member_by_path(&self, path: impl AsRef<Path>) -> Option<&PackageId> {
        self.members_by_path.get(path.as_ref())
    }
}

/// Represents a dependency from one package to another.
#[derive(Copy, Clone, Debug)]
pub struct DependencyLink<'g> {
    /// The package which depends on the `to` package.
    pub from: &'g PackageMetadata,
    /// The package which is depended on by the `from` package.
    pub to: &'g PackageMetadata,
    /// Information about the specifics of this dependency.
    pub edge: &'g DependencyEdge,
}

/// Information about a specific package in a `PackageGraph`.
///
/// Most of the metadata is extracted from `Cargo.toml` files. See
/// [the `Cargo.toml` reference](https://doc.rust-lang.org/cargo/reference/manifest.html) for more
/// details.
#[derive(Clone, Debug)]
pub struct PackageMetadata {
    // Implementation note: we use Box<str> and Box<Path> to save on memory use when possible.

    // Fields extracted from the package.
    pub(super) id: PackageId,
    pub(super) name: String,
    pub(super) version: Version,
    pub(super) authors: Vec<String>,
    pub(super) description: Option<Box<str>>,
    pub(super) license: Option<Box<str>>,
    pub(super) license_file: Option<Box<Path>>,
    pub(super) manifest_path: Box<Path>,
    pub(super) categories: Vec<String>,
    pub(super) keywords: Vec<String>,
    pub(super) readme: Option<Box<Path>>,
    pub(super) repository: Option<Box<str>>,
    pub(super) edition: Box<str>,
    pub(super) metadata_table: JsonValue,
    pub(super) links: Option<Box<str>>,
    pub(super) publish: Option<Vec<String>>,
    // Some(...) means named feature with listed dependencies.
    // None means an optional dependency.
    pub(super) features: IndexMap<Box<str>, Option<Vec<String>>>,

    // Other information.
    pub(super) package_ix: NodeIndex<PackageIx>,
    pub(super) workspace_path: Option<Box<Path>>,
    pub(super) has_default_feature: bool,
    pub(super) resolved_deps: Vec<NodeDep>,
    pub(super) resolved_features: Vec<String>,
}

impl PackageMetadata {
    /// Returns the unique identifier for this package.
    pub fn id(&self) -> &PackageId {
        &self.id
    }

    /// Returns the name of this package.
    ///
    /// This is the same as the `name` field of `Cargo.toml`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the version of this package as resolved by Cargo.
    ///
    /// This is the same as the `version` field of `Cargo.toml`.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Returns the authors of this package.
    ///
    /// This is the same as the `authors` field of `Cargo.toml`.
    pub fn authors(&self) -> &[String] {
        &self.authors
    }

    /// Returns a short description for this package.
    ///
    /// This is the same as the `description` field of `Cargo.toml`.
    pub fn description(&self) -> Option<&str> {
        self.description.as_ref().map(|x| x.as_ref())
    }

    /// Returns an SPDX 2.1 license expression for this package, if specified.
    ///
    /// This is the same as the `license` field of `Cargo.toml`. Note that `guppy` does not perform
    /// any validation on this, though `crates.io` does if a crate is uploaded there.
    pub fn license(&self) -> Option<&str> {
        self.license.as_ref().map(|x| x.as_ref())
    }

    /// Returns the path to a license file for this package, if specified.
    ///
    /// This is the same as the `license_file` field of `Cargo.toml`. It is typically only specified
    /// for nonstandard licenses.
    pub fn license_file(&self) -> Option<&Path> {
        self.license_file.as_ref().map(|path| path.as_ref())
    }

    /// Returns the full path to the `Cargo.toml` for this package.
    ///
    /// This is specific to the system that `cargo metadata` was run on.
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    /// Returns categories for this package.
    ///
    /// This is the same as the `categories` field of `Cargo.toml`. For packages on `crates.io`,
    /// returned values are guaranteed to be
    /// [valid category slugs](https://crates.io/category_slugs).
    pub fn categories(&self) -> &[String] {
        &self.categories
    }

    /// Returns keywords for this package.
    ///
    /// This is the same as the `keywords` field of `Cargo.toml`.
    pub fn keywords(&self) -> &[String] {
        &self.keywords
    }

    /// Returns a path to the README for this package, if specified.
    ///
    /// This is the same as the `readme` field of `Cargo.toml`. The path returned is relative to the
    /// directory the `Cargo.toml` is in (i.e. relative to the parent of `self.manifest_path()`).
    pub fn readme(&self) -> Option<&Path> {
        self.readme.as_ref().map(|path| path.as_ref())
    }

    /// Returns the source code repository for this package, if specified.
    ///
    /// This is the same as the `repository` field of `Cargo.toml`.
    pub fn repository(&self) -> Option<&str> {
        self.repository.as_ref().map(|x| x.as_ref())
    }

    /// Returns the Rust edition this package is written against.
    ///
    /// This is the same as the `edition` field of `Cargo.toml`. It is `"2015"` by default.
    pub fn edition(&self) -> &str {
        &self.edition
    }

    /// Returns the freeform metadata table for this package.
    ///
    /// This is the same as the `package.metadata` section of `Cargo.toml`. This section is
    /// typically used by tools which would like to store package configuration in `Cargo.toml`.
    pub fn metadata_table(&self) -> &JsonValue {
        &self.metadata_table
    }

    /// Returns the name of a native library this package links to, if specified.
    ///
    /// This is the same as the `links` field of `Cargo.toml`. See [The `links` Manifest
    /// Key](https://doc.rust-lang.org/cargo/reference/build-scripts.html#the-links-manifest-key) in
    /// the Cargo book for more details.
    pub fn links(&self) -> Option<&str> {
        self.links.as_ref().map(|x| x.as_ref())
    }

    /// Returns the list of registries to which this package may be published.
    ///
    /// Returns `None` if publishing is unrestricted, and `Some(&[])` if publishing is forbidden.
    ///
    /// This is the same as the `publish` field of `Cargo.toml`.
    pub fn publish(&self) -> Option<&[String]> {
        self.publish.as_deref()
    }

    /// Returns true if this package is in the workspace.
    pub fn in_workspace(&self) -> bool {
        self.workspace_path.is_some()
    }

    /// Returns the relative path to this package in the workspace, or `None` if this package is
    /// not in the workspace.
    pub fn workspace_path(&self) -> Option<&Path> {
        self.workspace_path.as_ref().map(|path| path.as_ref())
    }

    /// Returns true if this package has a named feature named `default`.
    ///
    /// For more about default features, see [The `[features]`
    /// section](https://doc.rust-lang.org/cargo/reference/manifest.html#the-features-section) in
    /// the Cargo reference.
    pub fn has_default_feature(&self) -> bool {
        self.has_default_feature
    }

    /// Returns the `FeatureId` corresponding to the default feature.
    pub fn default_feature_id(&self) -> FeatureId {
        if self.has_default_feature {
            FeatureId::new(self.id(), "default")
        } else {
            FeatureId::base(self.id())
        }
    }

    /// Returns the list of named features available for this package. This will include a feature
    /// named "default" if it is defined.
    ///
    /// A named feature is listed in the `[features]` section of `Cargo.toml`. For more, see
    /// [the reference](https://doc.rust-lang.org/cargo/reference/manifest.html#the-features-section).
    pub fn named_features(&self) -> impl Iterator<Item = &str> {
        self.named_features_full()
            .map(|(_, named_feature, _)| named_feature)
    }

    // ---
    // Helper methods
    // --

    pub(super) fn get_feature_idx(&self, feature: &str) -> Option<usize> {
        self.features.get_full(feature).map(|(n, _, _)| n)
    }

    pub(super) fn all_feature_nodes<'g>(&'g self) -> impl Iterator<Item = FeatureNode> + 'g {
        iter::once(FeatureNode::base(self.package_ix)).chain(
            (0..self.features.len())
                .map(move |feature_idx| FeatureNode::new(self.package_ix, feature_idx)),
        )
    }

    pub(super) fn named_features_full(&self) -> impl Iterator<Item = (usize, &str, &[String])> {
        self.features
            .iter()
            // IndexMap is documented to use indexes 0..n without holes, so this enumerate()
            // is correct.
            .enumerate()
            .filter_map(|(n, (feature, deps))| {
                deps.as_ref()
                    .map(|deps| (n, feature.as_ref(), deps.as_slice()))
            })
    }

    pub(super) fn optional_deps_full(&self) -> impl Iterator<Item = (usize, &str)> {
        self.features
            .iter()
            // IndexMap is documented to use indexes 0..n without holes, so this enumerate()
            // is correct.
            .enumerate()
            .filter_map(|(n, (feature, deps))| {
                if deps.is_none() {
                    Some((n, feature.as_ref()))
                } else {
                    None
                }
            })
    }
}

/// Details about a specific dependency from a package to another package.
///
/// Usually found within the context of a [`DependencyLink`](struct.DependencyLink.html).
///
/// This struct contains information about:
/// * whether this dependency was renamed in the context of this crate.
/// * if this is a normal, dev or build dependency.
#[derive(Clone, Debug)]
pub struct DependencyEdge {
    pub(super) dep_name: String,
    pub(super) resolved_name: String,
    pub(super) normal: Option<DependencyMetadata>,
    pub(super) build: Option<DependencyMetadata>,
    pub(super) dev: Option<DependencyMetadata>,
}

impl DependencyEdge {
    /// Returns the name for this dependency edge. This can be affected by a crate rename.
    pub fn dep_name(&self) -> &str {
        &self.dep_name
    }

    /// Returns the resolved name for this dependency edge. This may involve renaming the crate and
    /// replacing - with _.
    pub fn resolved_name(&self) -> &str {
        &self.resolved_name
    }

    /// Returns details about this dependency from the `[dependencies]` section, if they exist.
    pub fn normal(&self) -> Option<&DependencyMetadata> {
        self.normal.as_ref()
    }

    /// Returns details about this dependency from the `[build-dependencies]` section, if they exist.
    pub fn build(&self) -> Option<&DependencyMetadata> {
        self.build.as_ref()
    }

    /// Returns details about this dependency from the `[dev-dependencies]` section, if they exist.
    pub fn dev(&self) -> Option<&DependencyMetadata> {
        // XXX should dev dependencies fall back to normal if no dev-specific data was found?
        self.dev.as_ref()
    }

    /// Returns details about this dependency from the section specified by the given dependency
    /// kind.
    pub fn metadata_for_kind(&self, kind: DependencyKind) -> Option<&DependencyMetadata> {
        match kind {
            DependencyKind::Normal => self.normal(),
            DependencyKind::Development => self.dev(),
            DependencyKind::Build => self.build(),
            _ => panic!("dependency metadata requested for unknown kind: {:?}", kind),
        }
    }

    /// Return true if this edge is dev-only, i.e. code from this edge will not be included in
    /// normal builds.
    pub fn dev_only(&self) -> bool {
        self.normal().is_none() && self.build.is_none()
    }
}

/// Information about a specific kind of dependency (normal, build or dev) from a package to another
/// package.
///
/// Usually found within the context of a [`DependencyEdge`](struct.DependencyEdge.html).
#[derive(Clone, Debug)]
pub struct DependencyMetadata {
    pub(super) version_req: VersionReq,
    pub(super) dependency_req: DependencyReq,

    // Results of some queries as evaluated on the current platform.
    pub(super) current_status: DependencyStatus,
    pub(super) current_default_features: DependencyStatus,
    pub(super) all_features: Vec<String>,

    // single_target is deprecated -- it is only Some if there's exactly one instance of this
    // dependency.
    pub(super) single_target: Option<String>,
}

impl DependencyMetadata {
    /// Returns the semver requirements specified for this dependency.
    ///
    /// To get the resolved version, see the `to` field of the `DependencyLink` this was part of.
    ///
    /// ## Notes
    ///
    /// A dependency can be requested multiple times in the normal, build and dev fields, possibly
    /// with different version requirements, even if they all end up resolving to the same version.
    ///
    /// See [Specifying Dependencies](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#specifying-dependencies)
    /// in the Cargo reference for more details.
    pub fn version_req(&self) -> &VersionReq {
        &self.version_req
    }

    /// Returns true if this is an optional dependency on the platform `guppy` is running on.
    ///
    /// This will also return true if this dependency will never be included on this platform at
    /// all. To get finer-grained information, use the `build_status` method instead.
    pub fn optional(&self) -> bool {
        self.current_status != DependencyStatus::Mandatory
    }

    /// Returns the build status of this dependency on the platform `guppy` is running on.
    ///
    /// See the documentation for `DependencyStatus` for more.
    pub fn build_status(&self) -> DependencyStatus {
        self.current_status
    }

    /// Returns the status of this dependency on the given platform (target triple).
    ///
    /// The list of target triples is specified on the [Rust
    /// Forge](https://forge.rust-lang.org/release/platform-support.html).
    ///
    /// Returns an error if the triple wasn't recognized or if an error happened during evaluation.
    pub fn build_status_on(&self, platform: &str) -> Result<DependencyStatus, Error> {
        self.dependency_req.build_status_on(platform)
    }

    /// Returns true if the default features of this dependency are enabled on the platform `guppy`
    /// is running on.
    ///
    /// It is possible for default features to be turned off by default, but be optionally included.
    /// This method returns true in those cases. To get finer-grained information, use
    /// the `default_features` method instead.
    pub fn uses_default_features(&self) -> bool {
        self.current_default_features != DependencyStatus::Never
    }

    /// Returns the status of default features on the platform `guppy` is running on.
    ///
    /// See the documentation for `DependencyStatus` for more.
    pub fn default_features(&self) -> DependencyStatus {
        self.current_default_features
    }

    /// Returns the status of default features of this dependency on the given platform (target
    /// triple).
    ///
    /// The list of target triples is specified on the [Rust
    /// Forge](https://forge.rust-lang.org/release/platform-support.html).
    ///
    /// Returns an error if the triple wasn't recognized or if an error happened during evaluation.
    pub fn default_features_on(&self, platform: &str) -> Result<DependencyStatus, Error> {
        self.dependency_req.default_features_on(platform)
    }

    /// Returns a list of every feature enabled by this dependency. This includes features that
    /// are only turned on if the dependency is optional.
    pub fn features(&self) -> &[String] {
        &self.all_features
    }

    /// Returns the target string for this dependency, if specified. This is a string like
    /// `cfg(target_arch = "x86_64")`.
    ///
    /// See [Platform specific dependencies](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#platform-specific-dependencies)
    /// in the Cargo reference for more details.
    ///
    /// This will return `None` if this dependency is specified for more than one target
    /// (including unconditionally, as e.g. `[dependencies]`). Therefore, this is deprecated in
    /// favor of the `build_status_on` and `default_features_on` methods.
    #[deprecated(
        since = "0.1.7",
        note = "use `build_status_on` and `default_features_on` instead"
    )]
    pub fn target(&self) -> Option<&str> {
        self.single_target.as_deref()
    }
}

/// Whether a dependency is included, or whether default features are included, on a specific
/// platform.
///
/// ## Examples
///
/// ```toml
/// [dependencies]
/// once_cell = "1"
/// ```
///
/// The dependency and default features are *mandatory* on all platforms.
///
/// ```toml
/// [dependencies]
/// once_cell = { version = "1", optional = true }
/// ```
///
/// The dependency and default features are *optional* on all platforms.
///
/// ```toml
/// [target.'cfg(windows)'.dependencies]
/// once_cell = { version = "1", optional = true }
/// ```
///
/// On Windows, the dependency and default features are both *optional*. On non-Windows platforms,
/// the dependency and default features are *never* included.
///
/// ```toml
/// [dependencies]
/// once_cell = { version = "1", optional = true }
///
/// [target.'cfg(windows)'.dependencies]
/// once_cell = { version = "1", optional = false, default-features = false }
/// ```
///
/// On Windows, the dependency is mandatory and default features are optional (i.e. enabled if the
/// `once_cell` feature is turned on).
///
/// On Unix platforms, the dependency and default features are both optional.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DependencyStatus {
    /// This dependency or default features are always included in the build on this platform.
    Mandatory,
    /// This dependency or default features are optionally included in the build on this platform.
    Optional,
    /// This dependency or default features are never included in the build in this platform, even
    /// if the optional dependency is turned on.
    Never,
}

/// Information about dependency requirements.
#[derive(Clone, Debug, Default)]
pub(super) struct DependencyReq {
    pub(super) mandatory: DependencyReqImpl,
    pub(super) optional: DependencyReqImpl,
}

impl DependencyReq {
    pub(super) fn build_status_on(&self, platform: &str) -> Result<DependencyStatus, Error> {
        self.eval(|req_impl| &req_impl.build_if, platform)
    }

    pub(super) fn default_features_on(&self, platform: &str) -> Result<DependencyStatus, Error> {
        self.eval(|req_impl| &req_impl.default_features_if, platform)
    }

    fn eval(
        &self,
        pred_fn: impl Fn(&DependencyReqImpl) -> &TargetPredicate,
        platform: &str,
    ) -> Result<DependencyStatus, Error> {
        let map_err = move |err: EvalError| Error::TargetEvalError {
            platform: platform.into(),
            err: Box::new(err),
        };
        if pred_fn(&self.mandatory).eval(platform).map_err(map_err)? {
            return Ok(DependencyStatus::Mandatory);
        }
        if pred_fn(&self.optional).eval(platform).map_err(map_err)? {
            return Ok(DependencyStatus::Optional);
        }
        Ok(DependencyStatus::Never)
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct DependencyReqImpl {
    pub(super) build_if: TargetPredicate,
    pub(super) default_features_if: TargetPredicate,
    pub(super) target_features: Vec<(Option<Arc<TargetSpec>>, Vec<String>)>,
}

impl DependencyReqImpl {
    pub(super) fn all_features(&self) -> impl Iterator<Item = &str> {
        self.target_features
            .iter()
            .flat_map(|(_, features)| features)
            .map(|s| s.as_str())
    }
}

#[derive(Clone, Debug)]
pub(super) enum TargetPredicate {
    Always,
    // Empty vector means never.
    Specs(Vec<Arc<TargetSpec>>),
}

impl TargetPredicate {
    /// Returns true if this is an empty predicate (i.e. will never match).
    pub(super) fn is_empty(&self) -> bool {
        match self {
            TargetPredicate::Always => false,
            TargetPredicate::Specs(specs) => specs.is_empty(),
        }
    }

    /// Evaluates this target against the given platform triple.
    pub(super) fn eval(&self, platform: &str) -> Result<bool, EvalError> {
        match self {
            TargetPredicate::Always => Ok(true),
            TargetPredicate::Specs(specs) => {
                for spec in specs.iter() {
                    if spec.eval(platform)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }
}

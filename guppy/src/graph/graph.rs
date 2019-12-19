// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::graph::{kind_str, DependencyDirection};
use crate::{Error, JsonValue, Metadata, MetadataCommand, PackageId};
use cargo_metadata::{Dependency, DependencyKind, NodeDep};
use lazy_static::lazy_static;
use petgraph::algo::{has_path_connecting, toposort, DfsSpace};
use petgraph::prelude::*;
use petgraph::visit::{IntoNeighborsDirected, IntoNodeIdentifiers, Visitable};
use semver::{Version, VersionReq};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::iter;
use std::path::{Path, PathBuf};

/// A graph of packages and dependencies between them, parsed from metadata returned by `cargo
/// metadata`.
///
/// For examples on how to use `PackageGraph`, see
/// [the `examples` directory](https://github.com/calibra/cargo-guppy/tree/master/guppy/examples)
/// in this crate.
#[derive(Clone, Debug)]
pub struct PackageGraph {
    // Source of truth data.
    pub(super) dep_graph: Graph<PackageId, DependencyEdge>,
    // XXX Should this be in an Arc for quick cloning? Not clear how this would work with node
    // filters though.
    pub(super) data: PackageGraphData,
}

/// Per-package data for a PackageGraph instance.
#[derive(Clone, Debug)]
pub struct PackageGraphData {
    pub(super) packages: HashMap<PackageId, PackageMetadataInner>,
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
        lazy_static! {
            static ref MAJOR_WILDCARD: VersionReq = VersionReq::parse("*").unwrap();
        }

        // Graph structure checks.
        let node_count = self.dep_graph.node_count();
        let package_count = self.data.packages.len();
        if node_count != package_count {
            return Err(Error::PackageGraphInternalError(format!(
                "number of nodes = {} different from packages = {}",
                node_count, package_count,
            )));
        }
        // petgraph has both is_cyclic_directed and toposort to detect cycles. is_cyclic_directed
        // is recursive and toposort is iterative. Package graphs have unbounded depth so use the
        // iterative implementation.
        if let Err(cycle) = toposort(&self.dep_graph, None) {
            return Err(Error::PackageGraphInternalError(format!(
                "unexpected cycle in dep graph: {:?}",
                cycle
            )));
        }

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

            for dep in self.dep_links_node_idx_directed(metadata.inner.node_idx, Outgoing) {
                let to_id = dep.to().id();
                let to_version = dep.to().version();

                let version_check = |dep_metadata: &DependencyMetadata, kind: DependencyKind| {
                    let req = dep_metadata.version_req();
                    // A requirement of "*" filters out pre-release versions with the semver crate,
                    // but cargo accepts them.
                    // See https://github.com/steveklabnik/semver/issues/98.
                    if req == &*MAJOR_WILDCARD || req.matches(to_version) {
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
    pub fn packages<'g>(&'g self) -> impl Iterator<Item = PackageMetadata<'g>> + ExactSizeIterator {
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
    pub fn metadata<'g>(&'g self, package_id: &PackageId) -> Option<PackageMetadata<'g>> {
        self.data.metadata(package_id)
    }

    /// Returns the inner form of the metadata for the given package Id.
    pub(super) fn metadata_inner(&self, package_id: &PackageId) -> Option<&PackageMetadataInner> {
        self.data.metadata_inner(package_id)
    }

    /// Keeps all edges that return true from the visit closure, and removes the others.
    ///
    /// The order edges are visited is not specified.
    pub fn retain_edges<F>(&mut self, visit: F)
    where
        F: Fn(DependencyLink<'_>) -> bool,
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
            visit(DependencyLink {
                data,
                from,
                to,
                edge,
            })
        });
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

    // ---
    // Dependency traversals
    // ---

    /// Returns the direct dependencies for the given package ID in the specified direction.
    pub fn dep_links_directed<'g>(
        &'g self,
        package_id: &PackageId,
        dep_direction: DependencyDirection,
    ) -> Option<impl Iterator<Item = DependencyLink<'g>> + 'g> {
        self.dep_links_impl(package_id, dep_direction.to_direction())
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
        self.metadata_inner(package_id)
            .map(|inner| self.dep_links_node_idx_directed(inner.node_idx, dir))
    }

    fn dep_links_node_idx_directed<'g>(
        &'g self,
        node_idx: NodeIndex<u32>,
        dir: Direction,
    ) -> impl Iterator<Item = DependencyLink<'g>> + 'g {
        self.dep_graph
            .edges_directed(node_idx, dir)
            .map(move |edge| self.edge_to_link(edge.source(), edge.target(), edge.weight()))
    }

    // For more traversals, see select.rs.

    // ---
    // Helper methods
    // ---

    /// Returns the inner dependency graph.
    ///
    /// Should this be exposed publicly? Not sure.
    pub(super) fn dep_graph(&self) -> &Graph<PackageId, DependencyEdge> {
        &self.dep_graph
    }

    /// Returns the nodes of a graph that have no incoming edges to them.
    pub(super) fn roots<G, B>(graph: G) -> B
    where
        G: IntoNodeIdentifiers + IntoNeighborsDirected<NodeId = NodeIndex<u32>>,
        B: iter::FromIterator<NodeIndex<u32>>,
    {
        graph
            .node_identifiers()
            .filter(move |&a| graph.neighbors_directed(a, Incoming).next().is_none())
            .collect()
    }

    /// Maps an edge source, target and weight to a dependency link.
    pub(super) fn edge_to_link<'g>(
        &'g self,
        source: NodeIndex<u32>,
        target: NodeIndex<u32>,
        edge: &'g DependencyEdge,
    ) -> DependencyLink<'g> {
        // Note: It would be really lovely if this could just take in any EdgeRef with the right
        // parameters, but 'weight' wouldn't live long enough unfortunately.
        //
        // https://docs.rs/petgraph/0.4.13/petgraph/graph/struct.EdgeReference.html#method.weight
        // is defined separately for the same reason.
        let from = self
            .metadata_inner(&self.dep_graph[source])
            .expect("'from' should have associated metadata");
        let to = self
            .metadata_inner(&self.dep_graph[target])
            .expect("'to' should have associated metadata");
        DependencyLink {
            data: &self.data,
            from,
            to,
            edge,
        }
    }

    /// Maps an iterator of package IDs to their internal graph node indexes.
    pub(super) fn node_idxs<'g, 'a, B>(
        &'g self,
        package_ids: impl IntoIterator<Item = &'a PackageId>,
    ) -> Result<B, Error>
    where
        B: iter::FromIterator<NodeIndex<u32>>,
    {
        package_ids
            .into_iter()
            .map(|package_id| {
                self.node_idx(package_id)
                    .ok_or_else(|| Error::UnknownPackageId(package_id.clone()))
            })
            .collect()
    }

    /// Maps a package ID to its internal graph node index.
    pub(super) fn node_idx(&self, package_id: &PackageId) -> Option<NodeIndex<u32>> {
        self.data.node_idx(package_id)
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
    pub fn packages<'g>(&'g self) -> impl Iterator<Item = PackageMetadata<'g>> + ExactSizeIterator {
        self.packages
            .values()
            .map(move |inner| PackageMetadata::new(self, inner))
    }

    /// Returns the metadata for the given package ID.
    pub fn metadata<'g>(&'g self, package_id: &PackageId) -> Option<PackageMetadata<'g>> {
        self.metadata_inner(package_id)
            .map(|inner| PackageMetadata::new(self, inner))
    }

    /// Returns the inner form of the metadata for the given package ID.
    pub(super) fn metadata_inner<'g>(
        &'g self,
        package_id: &PackageId,
    ) -> Option<&'g PackageMetadataInner> {
        self.packages.get(package_id)
    }

    /// Maps a package ID to its internal graph node index.
    pub(super) fn node_idx(&self, package_id: &PackageId) -> Option<NodeIndex<u32>> {
        self.metadata_inner(package_id).map(|inner| inner.node_idx)
    }
}

/// An optional cache used to speed up `depends_on` queries.
///
/// Created with `PackageGraph::new_cache()`.
#[derive(Clone, Debug)]
pub struct DependsCache<'g> {
    package_graph: &'g PackageGraph,
    dfs_space: DfsSpace<NodeIndex<u32>, <Graph<NodeIndex<u32>, EdgeIndex<u32>> as Visitable>::Map>,
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
        // XXX rewrite this to avoid an allocation? meh
        let node_idxs: Vec<_> = self
            .package_graph
            .node_idxs(iter::once(package_a).chain(iter::once(package_b)))?;
        Ok(has_path_connecting(
            self.package_graph.dep_graph(),
            node_idxs[0],
            node_idxs[1],
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
    data: &'g PackageGraphData,
    from: &'g PackageMetadataInner,
    to: &'g PackageMetadataInner,
    edge: &'g DependencyEdge,
}

impl<'g> DependencyLink<'g> {
    /// Returns the backing data store for this package graph.
    pub fn data(&self) -> &'g PackageGraphData {
        self.data
    }

    /// Returns the package which depends on the `to` package.
    pub fn from(&self) -> PackageMetadata<'g> {
        PackageMetadata::new(self.data, self.from)
    }

    /// Returns the package which is depended on by the `from` package.
    pub fn to(&self) -> PackageMetadata<'g> {
        PackageMetadata::new(self.data, self.to)
    }

    /// Returns information about the specifics of this dependency.
    pub fn edge(&self) -> &'g DependencyEdge {
        self.edge
    }

    /// Returns a triple of pointers, useful for comparison in tests.
    #[cfg(test)]
    pub(crate) fn ptr_triple(
        &self,
    ) -> (
        *const PackageMetadataInner,
        *const PackageMetadataInner,
        *const DependencyEdge,
    ) {
        (self.from, self.to, self.edge)
    }
}

/// Information about a specific package in a `PackageGraph`.
///
/// Most of the metadata is extracted from `Cargo.toml` files. See
/// [the `Cargo.toml` reference](https://doc.rust-lang.org/cargo/reference/manifest.html) for more
/// details.
#[derive(Clone, Copy, Debug)]
pub struct PackageMetadata<'g> {
    data: &'g PackageGraphData,
    inner: &'g PackageMetadataInner,
}

impl<'g> PackageMetadata<'g> {
    pub(super) fn new(data: &'g PackageGraphData, inner: &'g PackageMetadataInner) -> Self {
        Self { data, inner }
    }
}

/// Representation of package metadata stored internally.
#[derive(Clone, Debug)]
pub(crate) struct PackageMetadataInner {
    // Fields extracted from the package.
    pub(super) id: PackageId,
    pub(super) name: String,
    pub(super) version: Version,
    pub(super) authors: Vec<String>,
    pub(super) description: Option<String>,
    pub(super) license: Option<String>,
    pub(super) license_file: Option<PathBuf>,
    pub(super) deps: Vec<Dependency>,
    pub(super) manifest_path: PathBuf,
    pub(super) categories: Vec<String>,
    pub(super) keywords: Vec<String>,
    pub(super) readme: Option<PathBuf>,
    pub(super) repository: Option<String>,
    pub(super) edition: String,
    pub(super) metadata_table: JsonValue,
    pub(super) links: Option<String>,
    pub(super) publish: Option<Vec<String>>,

    // Other information.
    pub(super) node_idx: NodeIndex<u32>,
    pub(super) workspace_path: Option<PathBuf>,
    pub(super) resolved_deps: Vec<NodeDep>,
    pub(super) resolved_features: Vec<String>,
}

impl<'g> PackageMetadata<'g> {
    /// Returns the unique identifier for this package.
    pub fn id(&self) -> &'g PackageId {
        &self.inner.id
    }

    /// Returns the name of this package.
    ///
    /// This is the same as the `name` field of `Cargo.toml`.
    pub fn name(&self) -> &'g str {
        &self.inner.name
    }

    /// Returns the version of this package as resolved by Cargo.
    ///
    /// This is the same as the `version` field of `Cargo.toml`.
    pub fn version(&self) -> &'g Version {
        &self.inner.version
    }

    /// Returns the authors of this package.
    ///
    /// This is the same as the `authors` field of `Cargo.toml`.
    pub fn authors(&self) -> &[String] {
        &self.inner.authors
    }

    /// Returns a short description for this package.
    ///
    /// This is the same as the `description` field of `Cargo.toml`.
    pub fn description(&self) -> Option<&str> {
        self.inner.description.as_ref().map(|x| x.as_str())
    }

    /// Returns an SPDX 2.1 license expression for this package, if specified.
    ///
    /// This is the same as the `license` field of `Cargo.toml`. Note that `guppy` does not perform
    /// any validation on this, though `crates.io` does if a crate is uploaded there.
    pub fn license(&self) -> Option<&str> {
        self.inner.license.as_ref().map(|x| x.as_str())
    }

    /// Returns the path to a license file for this package, if specified.
    ///
    /// This is the same as the `license_file` field of `Cargo.toml`. It is typically only specified
    /// for nonstandard licenses.
    pub fn license_file(&self) -> Option<&Path> {
        self.inner.license_file.as_ref().map(|path| path.as_path())
    }

    /// Returns the full path to the `Cargo.toml` for this package.
    ///
    /// This is specific to the system that `cargo metadata` was run on.
    pub fn manifest_path(&self) -> &Path {
        &self.inner.manifest_path
    }

    /// Returns categories for this package.
    ///
    /// This is the same as the `categories` field of `Cargo.toml`. For packages on `crates.io`,
    /// returned values are guaranteed to be
    /// [valid category slugs](https://crates.io/category_slugs).
    pub fn categories(&self) -> &[String] {
        &self.inner.categories
    }

    /// Returns keywords for this package.
    ///
    /// This is the same as the `keywords` field of `Cargo.toml`.
    pub fn keywords(&self) -> &[String] {
        &self.inner.keywords
    }

    /// Returns a path to the README for this package, if specified.
    ///
    /// This is the same as the `readme` field of `Cargo.toml`. The path returned is relative to the
    /// directory the `Cargo.toml` is in (i.e. relative to the parent of `self.manifest_path()`).
    pub fn readme(&self) -> Option<&Path> {
        self.inner.readme.as_ref().map(|path| path.as_path())
    }

    /// Returns the source code repository for this package, if specified.
    ///
    /// This is the same as the `repository` field of `Cargo.toml`.
    pub fn repository(&self) -> Option<&str> {
        self.inner.repository.as_ref().map(|x| x.as_str())
    }

    /// Returns the Rust edition this package is written against.
    ///
    /// This is the same as the `edition` field of `Cargo.toml`. It is `"2015"` by default.
    pub fn edition(&self) -> &str {
        &self.inner.edition
    }

    /// Returns the freeform metadata table for this package.
    ///
    /// This is the same as the `package.metadata` section of `Cargo.toml`. This section is
    /// typically used by tools which would like to store package configuration in `Cargo.toml`.
    pub fn metadata_table(&self) -> &JsonValue {
        &self.inner.metadata_table
    }

    /// Returns the name of a native library this package links to, if specified.
    ///
    /// This is the same as the `links` field of `Cargo.toml`. See [The `links` Manifest
    /// Key](https://doc.rust-lang.org/cargo/reference/build-scripts.html#the-links-manifest-key) in
    /// the Cargo book for more details.
    pub fn links(&self) -> Option<&str> {
        self.inner.links.as_ref().map(|x| x.as_str())
    }

    /// Returns the list of registries to which this package may be published.
    ///
    /// Returns `None` if publishing is unrestricted, and `Some(&[])` if publishing is forbidden.
    ///
    /// This is the same as the `publish` field of `Cargo.toml`.
    pub fn publish(&self) -> Option<&[String]> {
        self.inner
            .publish
            .as_ref()
            .map(|publish| publish.as_slice())
    }

    /// Returns true if this package is in the workspace.
    pub fn in_workspace(&self) -> bool {
        self.inner.workspace_path.is_some()
    }

    /// Returns the relative path to this package in the workspace, or `None` if this package is
    /// not in the workspace.
    pub fn workspace_path(&self) -> Option<&Path> {
        self.inner
            .workspace_path
            .as_ref()
            .map(|path| path.as_path())
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
    // TODO: move to inner structs to facilitate string interning
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
    pub(super) optional: bool,
    pub(super) uses_default_features: bool,
    pub(super) features: Vec<String>,
    pub(super) target: Option<String>,
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

    /// Returns true if this is an optional dependency.
    pub fn optional(&self) -> bool {
        self.optional
    }

    /// Returns true if the default features of this dependency are enabled.
    pub fn uses_default_features(&self) -> bool {
        self.uses_default_features
    }

    /// Returns a list of the features enabled by this dependency.
    pub fn features(&self) -> &[String] {
        &self.features
    }

    /// Returns the target string for this dependency, if specified. This is a string like
    /// `cfg(target_arch = "x86_64")`.
    ///
    /// See [Platform specific dependencies](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#platform-specific-dependencies)
    /// in the Cargo reference for more details.
    pub fn target(&self) -> Option<&str> {
        self.target.as_ref().map(|x| x.as_str())
    }
}

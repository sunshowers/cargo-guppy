// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Support for comparing Cargo and Guppy.

use crate::diff::DiffOpts;
use anyhow::Result;
use guppy::{Platform, TargetFeatures};
use guppy_cmdlib::PackagesAndFeatures;
use structopt::StructOpt;

pub mod diff;
pub mod type_conversions;

#[derive(Debug, StructOpt)]
pub struct CargoCompare {
    // TODO: add global options
    #[structopt(subcommand)]
    cmd: Command,
}

impl CargoCompare {
    pub fn exec(self) -> Result<()> {
        match self.cmd {
            Command::Diff(opts) => opts.exec(),
        }
    }
}

#[derive(Debug, StructOpt)]
enum Command {
    #[structopt(name = "diff")]
    /// Perform a diff of Cargo's results against Guppy's
    Diff(DiffOpts),
}

/// Options that are common to Guppy and Cargo.
///
/// Guppy supports more options than Cargo. This describes the minimal set that both support.
#[derive(Debug, StructOpt)]
pub struct GuppyCargoCommon {
    #[structopt(flatten)]
    pub pf: PackagesAndFeatures,

    /// Include dev dependencies for initial packages
    #[structopt(long = "include-dev")]
    pub include_dev: bool,

    /// Evaluate for the target triple (default: current platform)
    #[structopt(long = "target")]
    pub target_platform: Option<String>,
}

impl GuppyCargoCommon {
    /// Returns a `Platform` corresponding to the target platform.
    pub fn make_target_platform(&self) -> Result<Platform<'static>> {
        match &self.target_platform {
            Some(triple) => Platform::new(triple, TargetFeatures::Unknown)
                .ok_or_else(|| anyhow::anyhow!("unknown triple: {}", triple)),
            None => Platform::current()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown current platform")),
        }
    }
}

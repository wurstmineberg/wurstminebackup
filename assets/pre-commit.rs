#!/usr/bin/env -S cargo +nightly -Zscript
---
[package]
edition = "2024"
rust-version = "1.91" # nixpkgs stable

[dependencies]
lazy-regex = "3"
serde = { version = "1", features = ["derive"] }
thiserror = "2"
tokio = { version = "1", features = ["process"] }
toml = "1"
wheel = { git = "https://github.com/fenhl/wheel" }
---

use {
    std::{
        collections::HashMap,
        process::Stdio,
    },
    lazy_regex::regex_is_match,
    serde::Deserialize,
    tokio::process::Command,
    wheel::traits::{
        AsyncCommandOutputExt as _,
        IoResultExt as _,
    },
};

#[derive(Deserialize)]
struct CargoToml {
    dependencies: HashMap<String, DependencySpec>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DependencySpec {
    VersionOnly(String),
    Table {
        version: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)] Toml(#[from] toml::de::Error),
    #[error(transparent)] Wheel(#[from] wheel::Error),
    #[error("version {version:?} for dependency {name:?} is not in its shortest form")]
    OverspecifiedVersion {
        name: String,
        version: String,
    },
}

#[wheel::main]
async fn main() -> Result<(), Error> {
    let manifest = toml::from_slice::<CargoToml>(&Command::new("git").arg("show").arg(":Cargo.toml").stdout(Stdio::piped()).check("git show").await?.stdout)?;
    for (name, value) in manifest.dependencies {
        if let Some(version) = match value {
            DependencySpec::VersionOnly(version) => Some(version),
            DependencySpec::Table { version } => version,
        } && !regex_is_match!(r"^(?:0\.)*[1-9][0-9]*$", &version) {
            return Err(Error::OverspecifiedVersion { name, version })
        }
    }

    println!("cargo deny");
    Command::new("cargo").arg("+stable").arg("deny").arg("check").arg("advisories").arg("bans").check("cargo deny").await?;

    println!("cargo check");
    Command::new("cargo").arg("+stable").arg("check").spawn().at_command("cargo check")?.check("cargo check").await?;

    cfg_select! {
        windows => {
            println!("nix build");
            Command::new("wsl").arg("nix").arg("build").arg("--no-link").spawn().at_command("nix build")?.check("nix build").await?;
        }
        _ => {
            println!("nix build");
            Command::new("nix").arg("build").arg("--no-link").spawn().at_command("nix build")?.check("nix build").await?;
        }
    }

    Ok(())
}

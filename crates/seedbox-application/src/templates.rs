//! Self-contained authoring examples, usable without the Seedbox source checkout.
//! Guest compatibility is a separate acceptance gate from generating the files.

use crate::{config, Error, Result};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Clone, Copy, Debug)]
pub enum Template {
    Etcd,
    Redis,
}

pub fn init(destination: &Path, template: Template) -> Result<()> {
    let files: Vec<(&str, &str)> = match template {
        Template::Etcd => vec![
            (
                "seedbox.yaml",
                include_str!("../../../examples/etcd/seedbox.yaml"),
            ),
            (
                "workload/Dockerfile",
                include_str!("../../../examples/etcd/workload/Dockerfile"),
            ),
            // Package the existing consistency checker rather than inventing a
            // weaker replacement workload during the authoring migration.
            (
                "workload/main.go",
                include_str!("../../../examples/etcd/workload/main.go"),
            ),
            (
                "workload/go.mod",
                include_str!("../../../examples/etcd/workload/go.mod"),
            ),
            (
                "workload/go.sum",
                include_str!("../../../examples/etcd/workload/go.sum"),
            ),
            (
                "workload/main_test.go",
                include_str!("../../../examples/etcd/workload/main_test.go"),
            ),
        ],
        Template::Redis => vec![
            (
                "seedbox.yaml",
                include_str!("../../../examples/redis/seedbox.yaml"),
            ),
            (
                "workload/Dockerfile",
                include_str!("../../../examples/redis/workload/Dockerfile"),
            ),
            (
                "workload/main.rs",
                include_str!("../../../examples/redis/workload/main.rs"),
            ),
        ],
    };
    // Reserve a new directory; never overwrite or merge with an existing project.
    fs::create_dir(destination)?;
    let write = (|| -> Result<()> {
        for (name, contents) in files {
            let path = destination.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, contents)?;
        }
        fs::write(destination.join(".gitignore"), ".seedbox/\n")?;
        fs::write(
            destination.join("workload/.dockerignore"),
            "target\n.git\n.seedbox\n",
        )?;
        fs::write(
            destination.join("README.md"),
            include_str!("../templates/README.md"),
        )?;
        config::load(&destination.join("seedbox.yaml"), &BTreeMap::new())?;
        Ok(())
    })();
    if let Err(error) = write {
        // Only this invocation's exclusively created directory is removed.
        fs::remove_dir_all(destination).map_err(|cleanup| {
            Error::Execution(format!("template error: {error}; cleanup: {cleanup}"))
        })?;
        return Err(error);
    }
    Ok(())
}

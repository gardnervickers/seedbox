//! Build the libc-free native guests for local development and acceptance tests.
use crate::error::{Error, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn checked(command: &mut Command) -> Result<()> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(Error::InvalidInput(format!(
            "guest tool failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}
pub fn assemble(source: &Path, output: &Path, atomic: bool) -> Result<()> {
    let obj = output.with_extension("o");
    let mut nasm = Command::new("nasm");
    if atomic {
        nasm.args(["-D", "ATOMIC=1"]);
    }
    checked(nasm.args(["-f", "elf64", "-o"]).arg(&obj).arg(source))?;
    checked(
        Command::new("ld.lld")
            .args(["-m", "elf_x86_64", "-static", "-o"])
            .arg(output)
            .arg(&obj),
    )
}
pub fn build_guests(root: &Path) -> Result<PathBuf> {
    let output = root.join("build/guests");
    fs::create_dir_all(&output)?;
    let mut sources: Vec<_> = fs::read_dir(root.join("guests"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    sources.sort();
    for source in sources
        .iter()
        .filter(|s| s.extension().is_some_and(|ext| ext == "asm"))
    {
        let name = source.file_stem().unwrap();
        assemble(source, &output.join(name), false)?;
        if name == "race" {
            assemble(source, &output.join("atomic"), true)?;
        }
    }
    Ok(output)
}

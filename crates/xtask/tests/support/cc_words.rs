use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    process::Command,
};

/// Preserve the progress and kernel probes' shlex.split(CC, comments=False): quotes
/// group words, escapes are literal, and no shell expansion or execution occurs.
/// In double quotes only a quote or backslash consumes its preceding backslash.
pub(super) fn compiler_words(value: &OsStr) -> Result<Vec<OsString>, &'static str> {
    let mut words = Vec::new();
    let mut word = Vec::new();
    let mut started = false;
    let mut quote = None;
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        match (quote, byte) {
            (Some(delimiter), byte) if delimiter == byte => quote = None,
            (None, b'\'' | b'"') => {
                quote = Some(byte);
                started = true;
            }
            (None | Some(b'"'), b'\\') => {
                let next = bytes.next().ok_or("CC ends with an escape")?;
                if quote.is_some() && !matches!(next, b'"' | b'\\') {
                    word.push(b'\\');
                }
                word.push(next);
                started = true;
            }
            (None, b' ' | b'\t' | b'\r' | b'\n') => {
                if started {
                    words.push(OsString::from_vec(std::mem::take(&mut word)));
                    started = false;
                }
            }
            _ => {
                word.push(byte);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("CC has an unterminated quote");
    }
    if started {
        words.push(OsString::from_vec(word));
    }
    Ok(words)
}

pub(super) fn split_compiler() -> Command {
    let value = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let words = compiler_words(&value).unwrap_or_else(|error| panic!("{error}: {value:?}"));
    let (program, arguments) = words.split_first().expect("CC has no compiler command");
    let mut command = Command::new(program);
    command.args(arguments);
    command
}

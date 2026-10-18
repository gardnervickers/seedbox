//! Shell integration. Completion only reads an existing server, with a short deadline.

use clap::{CommandFactory, ValueEnum};
use seedbox_application::{Error, Result};
use std::{io::Write, time::Duration};

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum Shell {
    /// Bash shell.
    Bash,
    /// Zsh shell.
    Zsh,
    /// Fish shell.
    Fish,
}

pub(super) fn script(shell: Shell) -> Result<u8> {
    let script = match shell {
        Shell::Bash => BASH,
        Shell::Zsh => ZSH,
        Shell::Fish => FISH,
    };
    write(script.as_bytes())?;
    Ok(0)
}

fn write(bytes: &[u8]) -> Result<()> {
    std::io::stdout()
        .lock()
        .write_all(bytes)
        .map_err(|error| Error::Setup(format!("shell completion output: {error}")))
}

pub(super) fn complete(words: &[String]) -> Result<u8> {
    let mut root = super::Cli::command();
    root.build();
    let Some((prefix, previous)) = words.split_last() else {
        return Ok(0);
    };
    let mut command = &root;
    let mut path = Vec::new();
    let mut positionals = Vec::new();
    let mut value_arg: Option<&clap::Arg> = None;
    let mut options = true;
    for word in previous {
        if value_arg.take().is_some() {
            continue;
        }
        if options && word == "--" {
            options = false;
            continue;
        }
        if options && word.starts_with('-') {
            let (name, inline) = word
                .split_once('=')
                .map_or((word.as_str(), false), |(n, _)| (n, true));
            if let Some(arg) = command.get_arguments().find(|arg| {
                arg.get_long()
                    .is_some_and(|long| name == format!("--{long}"))
                    || arg
                        .get_short()
                        .is_some_and(|short| name == format!("-{short}"))
            }) {
                if !inline && arg.get_num_args().is_some_and(|range| range.takes_values()) {
                    value_arg = Some(arg);
                }
            }
            continue;
        }
        if options {
            if let Some(child) = command.find_subcommand(word) {
                command = child;
                path.push(child.get_name());
                positionals.clear();
                continue;
            }
        }
        positionals.push(word.as_str());
    }
    let mut candidates = Vec::new();
    if let Some(arg) = value_arg {
        values(arg, &mut candidates);
    } else if options && prefix.starts_with('-') {
        for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
            if let Some(long) = arg.get_long() {
                candidates.push(format!("--{long}"));
            }
        }
    } else {
        if options && positionals.is_empty() {
            candidates.extend(
                command
                    .get_subcommands()
                    .filter(|c| !c.is_hide_set())
                    .map(|c| c.get_name().to_owned()),
            );
        }
        if let Some(arg) = command.get_positionals().nth(positionals.len()) {
            values(arg, &mut candidates);
        }
        candidates.extend(dynamic(&path, &positionals));
    }
    candidates.retain(|candidate| {
        candidate.starts_with(prefix) && !candidate.chars().any(|c| c.is_control())
    });
    candidates.sort();
    candidates.dedup();
    if !candidates.is_empty() {
        write(format!("{}\n", candidates.join("\n")).as_bytes())?;
    }
    Ok(0)
}

fn values(arg: &clap::Arg, candidates: &mut Vec<String>) {
    if let Some(values) = arg.get_value_parser().possible_values() {
        candidates.extend(
            values
                .filter(|value| !value.is_hide_set())
                .map(|value| value.get_name().to_owned()),
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Resource<'a> {
    Simulations,
    Machines(&'a str),
}

fn resource<'a>(path: &[&str], positionals: &[&'a str]) -> Option<Resource<'a>> {
    match (path, positionals) {
        (["logs"], [id]) => Some(Resource::Machines(id)),
        (["compare"], [] | [_]) => Some(Resource::Simulations),
        (
            ["rerun" | "replay" | "branch" | "stop" | "logs" | "events" | "playback" | "inject"
            | "heal" | "pause" | "resume" | "step" | "checkpoint" | "rate" | "tui" | "export"
            | "diagnostics"],
            [],
        )
        | (["get", "simulation" | "simulations" | "checkpoints"], [])
        | (["describe" | "delete", "simulation" | "checkpoint"], []) => Some(Resource::Simulations),
        _ => None,
    }
}

fn dynamic(path: &[&str], positionals: &[&str]) -> Vec<String> {
    let Some(resource) = resource(path, positionals) else {
        return Vec::new();
    };
    let Ok(root) = super::daemon::state_directory(None) else {
        return Vec::new();
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    runtime.block_on(async {
        // This wraps pagination and the machine lookup together. Never use
        // daemon::connect: that path may prompt and launch a server.
        tokio::time::timeout(Duration::from_millis(300), async {
            let client = seedbox_client::Client::connect_to(&root.join("server.sock")).ok()?;
            match resource {
                Resource::Machines(id) => {
                    let id = id.parse().ok()?;
                    let simulation = client.describe(&id).await.ok()?;
                    Some(simulation.spec.configuration.services.into_keys().collect())
                }
                Resource::Simulations => {
                    let mut candidates = Vec::new();
                    let mut cursor = None;
                    loop {
                        let page = client.list(cursor.as_ref()).await.ok()?;
                        candidates
                            .extend(page.items.into_iter().map(|s| s.metadata.id.to_string()));
                        if page.next_cursor.is_none() || page.next_cursor == cursor {
                            break;
                        }
                        cursor = page.next_cursor;
                    }
                    Some(candidates)
                }
            }
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
    })
}

const BASH: &str = r#"# Load with: source <(seedbox completions bash)
_seedbox_complete() {
    local candidate
    COMPREPLY=()
    while IFS= read -r candidate; do
        COMPREPLY+=("$candidate")
    done < <("${COMP_WORDS[0]}" __complete -- "${COMP_WORDS[@]:1:COMP_CWORD}" 2>/dev/null)
}
complete -o bashdefault -o default -F _seedbox_complete seedbox
"#;

const ZSH: &str = r#"# Load with: source <(seedbox completions zsh)
# Run compinit before loading this script.
_seedbox_complete() {
    local -a candidates
    candidates=("${(@f)$("${words[1]}" __complete -- "${(@)words[2,CURRENT]}" 2>/dev/null)}")
    if (( ${#candidates} )) && [[ -n ${candidates[1]} ]]; then
        compadd -- "${candidates[@]}"
    else
        _files
    fi
}
compdef _seedbox_complete seedbox
"#;

const FISH: &str = r#"# Load with: seedbox completions fish | source
function __seedbox_complete
    set -l words (commandline -opc)
    set -l current (commandline -ct)
    command $words[1] __complete -- $words[2..-1] "$current" 2>/dev/null
end
complete -c seedbox -a '(__seedbox_complete)'
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_positions_distinguish_ids_machines_and_paths() {
        assert_eq!(resource(&["logs"], &[]), Some(Resource::Simulations));
        assert_eq!(resource(&["tui"], &[]), Some(Resource::Simulations));
        assert_eq!(
            resource(&["logs"], &["abc"]),
            Some(Resource::Machines("abc"))
        );
        assert_eq!(resource(&["logs"], &["abc", "db"]), None);
        assert_eq!(resource(&["run"], &[]), None);
        assert_eq!(
            resource(&["compare"], &["abc"]),
            Some(Resource::Simulations)
        );
        assert_eq!(
            resource(&["describe", "simulation"], &[]),
            Some(Resource::Simulations)
        );
        assert_eq!(resource(&["delete", "checkpoint"], &["abc"]), None);
    }
}

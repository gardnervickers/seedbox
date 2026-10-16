//! Workload decisions as structured choices. Each workload client gets its
//! own SDK session, so a choice waiting for its reply never holds up another
//! client, and its own site prefix, so a choice input can target one client's
//! decision. Outside Seedbox every choice takes its first alternative.
use crate::Client;
use std::path::Path;

/// One workload client's decisions.
pub struct Decisions {
    client: Client,
    prefix: String,
}

impl Decisions {
    /// Decisions for the client named `name`, on a session of its own.
    pub fn new(name: &str) -> Self {
        Self::with_socket(name, crate::SOCKET)
    }
    pub fn with_socket(name: &str, socket: impl AsRef<Path>) -> Self {
        Self {
            client: Client::new(socket),
            prefix: name.to_owned(),
        }
    }
    fn site(&self, site: &str) -> String {
        format!("{}/{site}", self.prefix)
    }
    /// One of `names`, by index; the first when the reply path fails.
    pub fn choose(&self, site: &str, names: &[&str]) -> usize {
        self.client.choose(&self.site(site), names).unwrap_or(0)
    }
    /// One of `options`, choosing by name.
    pub fn pick<'a, T>(&self, site: &str, options: &'a [(&str, T)]) -> &'a T {
        let names: Vec<&str> = options.iter().map(|(name, _)| *name).collect();
        &options[self.choose(site, &names)].1
    }
    /// An index below `count` (at most 256), such as which key to touch.
    pub fn index(&self, site: &str, count: usize) -> usize {
        let count = count.clamp(1, crate::MAX_CHOICE_ALTERNATIVES);
        let names: Vec<String> = (0..count).map(|i| i.to_string()).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        self.choose(site, &names)
    }
    /// Yes or no, such as whether to retry; "no" outside Seedbox.
    pub fn flag(&self, site: &str) -> bool {
        self.choose(site, &["no", "yes"]) == 1
    }
}

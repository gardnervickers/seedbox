//! Read-only OCI distribution client. TLS, platform selection and content
//! verification are separate from release/backend qualification.

use super::{
    descriptor_digest, descriptor_limit, read_small, retain, sha256, Descriptor, DOCKER_INDEX,
    DOCKER_MANIFEST, META_LIMIT, OCI_INDEX, OCI_MANIFEST,
};
use crate::{
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use std::time::Duration;
use url::Url;

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryReference {
    pub registry: String,
    pub repository: String,
    pub selector: String,
}

impl RegistryReference {
    pub fn parse(value: &str) -> Result<Self> {
        let invalid = || {
            Error::Setup(format!(
                "image reference {value}: expected a registry name, optional tag or sha256 digest"
            ))
        };
        if value.is_empty()
            || value.chars().any(char::is_whitespace)
            || value.contains("://")
            || value.contains(['?', '#', '\\'])
        {
            return Err(invalid());
        }
        let (name, pinned) = match value.split_once('@') {
            Some((name, digest)) => {
                sha256(digest).map_err(|_| invalid())?;
                (name, Some(digest))
            }
            None => (value, None),
        };
        let (registry, repository) = match name.split_once('/') {
            Some((first, rest)) if first.contains(['.', ':']) || first == "localhost" => (
                if first == "index.docker.io" {
                    "docker.io"
                } else {
                    first
                },
                rest,
            ),
            _ => ("docker.io", name),
        };
        let (repository, tag) = match repository.rsplit_once(':') {
            Some((repository, tag)) => (repository, Some(tag)),
            None => (repository, None),
        };
        if repository.is_empty()
            || repository.split('/').any(|part| {
                part.is_empty()
                    || [".", ".."].contains(&part)
                    || !part.bytes().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c)
                    })
            })
            || tag.is_some_and(|tag| {
                tag.is_empty()
                    || tag.len() > 128
                    || !tag
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            })
        {
            return Err(invalid());
        }
        let endpoint = Url::parse(&format!("https://{registry}/")).map_err(|_| invalid())?;
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.host_str().is_none()
            || endpoint.path() != "/"
        {
            return Err(invalid());
        }
        Ok(Self {
            registry: registry.into(),
            repository: if registry == "docker.io" && !repository.contains('/') {
                format!("library/{repository}")
            } else {
                repository.into()
            },
            selector: pinned.or(tag).unwrap_or("latest").into(),
        })
    }
}

#[derive(Clone)]
pub struct RegistryClient {
    agent: ureq::Agent,
    endpoint: Option<Url>,
}

impl Default for RegistryClient {
    fn default() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .try_proxy_from_env(false)
                .redirects(0)
                .timeout_connect(Duration::from_secs(5))
                .timeout_read(Duration::from_secs(10))
                .timeout_write(Duration::from_secs(10))
                .build(),
            endpoint: None,
        }
    }
}

impl RegistryClient {
    /// Supply an explicit mirror or local test endpoint. Plain HTTP is accepted
    /// only for loopback; ordinary registry references always use verified TLS.
    pub fn with_endpoint(endpoint: &str) -> Result<Self> {
        let url = Url::parse(endpoint)
            .map_err(|_| Error::Setup("registry endpoint: invalid URL".into()))?;
        if !secure(&url) || url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(Error::Setup(
                "registry endpoint: use an HTTPS origin or a loopback HTTP origin".into(),
            ));
        }
        Ok(Self {
            endpoint: Some(url),
            ..Self::default()
        })
    }

    pub(super) fn session(&self, value: &str) -> Result<Session> {
        let reference = RegistryReference::parse(value)?;
        let host = if reference.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &reference.registry
        };
        let endpoint = match &self.endpoint {
            Some(endpoint) => endpoint.clone(),
            None => Url::parse(&format!("https://{host}/"))
                .map_err(|_| Error::Setup("registry reference: invalid origin".into()))?,
        };
        Ok(Session {
            client: self.clone(),
            reference,
            endpoint,
            token: None,
            captured: std::collections::BTreeSet::new(),
        })
    }
}

pub(super) struct Session {
    client: RegistryClient,
    reference: RegistryReference,
    endpoint: Url,
    token: Option<String>,
    captured: std::collections::BTreeSet<ContentDigest>,
}

impl Session {
    pub fn manifest(
        &mut self,
        store: &ContentStore,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<ContentDigest> {
        let selector = self.reference.selector.clone();
        let response = self.request("manifests", &selector, check)?;
        let advertised = response.header("Docker-Content-Digest").map(str::to_owned);
        let digest = retain(response.into_reader(), store, META_LIMIT, check)?;
        if selector.starts_with("sha256:") && digest != sha256(&selector)? {
            return Err(Error::Integrity(
                "registry manifest differs from requested digest".into(),
            ));
        }
        if advertised
            .as_deref()
            .map(sha256)
            .transpose()?
            .is_some_and(|expected| expected != digest)
        {
            return Err(Error::Integrity(
                "registry manifest digest header mismatch".into(),
            ));
        }
        Ok(digest)
    }

    pub fn blob(
        &mut self,
        descriptor: &Descriptor,
        store: &ContentStore,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<ContentDigest> {
        let expected = descriptor_digest(descriptor)?;
        descriptor_limit(descriptor)?;
        check()?;
        if self.captured.contains(&expected) {
            store.verify_checked(&expected, check)?;
            if store.size(&expected)? != descriptor.size {
                return Err(Error::Integrity(
                    "cached registry descriptor size mismatch".into(),
                ));
            }
            return Ok(expected);
        }
        let manifest = [OCI_MANIFEST, DOCKER_MANIFEST, OCI_INDEX, DOCKER_INDEX]
            .contains(&descriptor.media_type.as_str());
        let response = self.request(
            if manifest { "manifests" } else { "blobs" },
            &descriptor.digest,
            check,
        )?;
        let actual = retain(response.into_reader(), store, descriptor.size, check)?;
        if actual != expected || store.size(&actual)? != descriptor.size {
            return Err(Error::Integrity(format!(
                "registry descriptor hash/size mismatch: {}",
                descriptor.digest
            )));
        }
        self.captured.insert(actual.clone());
        Ok(actual)
    }

    fn request(
        &mut self,
        kind: &str,
        selector: &str,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<ureq::Response> {
        let url = self
            .endpoint
            .join(&format!(
                "v2/{}/{kind}/{selector}",
                self.reference.repository
            ))
            .map_err(|_| Error::Setup("registry reference: invalid request path".into()))?;
        let mut response = self.fetch(&url, true, check)?;
        if response.status() == 401 {
            let challenge = response
                .header("WWW-Authenticate")
                .ok_or_else(|| self.access_error(401))?;
            let parameters = bearer_challenge(challenge).map_err(|_| self.access_error(401))?;
            let realm = parameters
                .get("realm")
                .ok_or_else(|| self.access_error(401))?;
            let mut realm = Url::parse(realm).map_err(|_| self.access_error(401))?;
            if !secure(&realm)
                || (realm.scheme() == "http" && realm.origin() != self.endpoint.origin())
            {
                return Err(Error::Setup(
                    "registry authentication requires HTTPS".into(),
                ));
            }
            realm.query_pairs_mut().append_pair(
                "scope",
                &format!("repository:{}:pull", self.reference.repository),
            );
            if let Some(service) = parameters.get("service") {
                realm.query_pairs_mut().append_pair("service", service);
            }
            let token_response = self.fetch(&realm, false, check)?;
            if token_response.status() != 200 {
                return Err(self.access_error(token_response.status()));
            }
            let token: serde_json::Value =
                serde_json::from_slice(&read_small(token_response.into_reader(), check)?)?;
            let token = token
                .get("token")
                .or_else(|| token.get("access_token"))
                .and_then(serde_json::Value::as_str)
                .filter(|token| {
                    !token.is_empty()
                        && token.len() <= 16 * 1024
                        && !token.chars().any(char::is_control)
                })
                .ok_or_else(|| self.access_error(401))?;
            self.token = Some(token.into());
            response = self.fetch(&url, true, check)?;
        }
        if response.status() != 200 {
            return Err(self.access_error(response.status()));
        }
        Ok(response)
    }

    fn access_error(&self, status: u16) -> Error {
        Error::Setup(format!("registry {}/{}: HTTP {status}; check the image reference and access, or supply a saved archive or finished Docker/Podman image", self.reference.registry, self.reference.repository))
    }

    fn fetch(
        &self,
        initial: &Url,
        authenticated: bool,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<ureq::Response> {
        let mut url = initial.clone();
        for _ in 0..=5 {
            check()?;
            if !secure(&url) || (url.scheme() == "http" && url.origin() != self.endpoint.origin()) {
                return Err(Error::Setup("registry redirect requires HTTPS".into()));
            }
            let mut request = self.client.agent.get(url.as_str()).set("Accept", ACCEPT);
            // A blob redirect may point at a CDN. Never forward the registry
            // token to a different origin, including on later redirect hops.
            if authenticated && url.origin() == self.endpoint.origin() {
                if let Some(token) = &self.token {
                    request = request.set("Authorization", &format!("Bearer {token}"));
                }
            }
            let response = match request.call() {
                Ok(response) | Err(ureq::Error::Status(_, response)) => response,
                Err(error) => {
                    check()?;
                    return Err(Error::Setup(format!(
                        "registry {}: {}; check network access or supply a saved image archive",
                        self.reference.registry,
                        error.kind()
                    )));
                }
            };
            check()?;
            if [301, 302, 303, 307, 308].contains(&response.status()) {
                let location = response
                    .header("Location")
                    .ok_or_else(|| Error::Integrity("registry redirect lacks Location".into()))?;
                url = url
                    .join(location)
                    .map_err(|_| Error::Integrity("invalid registry redirect".into()))?;
                continue;
            }
            return Ok(response);
        }
        Err(Error::Integrity("too many registry redirects".into()))
    }
}

fn secure(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return false;
    }
    url.scheme() == "https"
        || (url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            }))
}

fn bearer_challenge(value: &str) -> Result<std::collections::BTreeMap<String, String>> {
    let invalid = || Error::Integrity("unsupported registry authentication challenge".into());
    let (scheme, parameters) = value.split_once(' ').ok_or_else(invalid)?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err(invalid());
    }
    let mut input = parameters.chars().peekable();
    let mut parsed = std::collections::BTreeMap::new();
    while input.peek().is_some() {
        while input
            .peek()
            .is_some_and(|c| c.is_ascii_whitespace() || *c == ',')
        {
            input.next();
        }
        if input.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while input
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            key.push(input.next().ok_or_else(invalid)?);
        }
        while input.peek().is_some_and(char::is_ascii_whitespace) {
            input.next();
        }
        if key.is_empty() || input.next() != Some('=') {
            return Err(invalid());
        }
        while input.peek().is_some_and(char::is_ascii_whitespace) {
            input.next();
        }
        let mut value = String::new();
        if input.peek() == Some(&'"') {
            input.next();
            loop {
                match input.next().ok_or_else(invalid)? {
                    '"' => break,
                    '\\' => value.push(input.next().ok_or_else(invalid)?),
                    c => value.push(c),
                }
            }
        } else {
            while input
                .peek()
                .is_some_and(|c| *c != ',' && !c.is_ascii_whitespace())
            {
                value.push(input.next().ok_or_else(invalid)?);
            }
        }
        if parsed.insert(key.to_ascii_lowercase(), value).is_some() {
            return Err(invalid());
        }
        while input.peek().is_some_and(char::is_ascii_whitespace) {
            input.next();
        }
        if input.peek().is_some_and(|c| *c != ',') {
            return Err(invalid());
        }
    }
    Ok(parsed)
}

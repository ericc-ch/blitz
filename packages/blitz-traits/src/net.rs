//! Abstractions of networking so that custom networking implementations can be provided

pub use bytes::Bytes;
pub use http::{self, HeaderMap, Method};
use serde::{
    Serialize,
    ser::{SerializeSeq, SerializeTuple},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{ops::Deref, path::PathBuf};
pub use url::Url;

/// A type that fetches resources for a Document.
///
/// This may be over the network via http(s), via the filesystem, or some other method.
pub trait NetProvider: Send + Sync + 'static {
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>);

    /// Whether this provider is a no-op (e.g. `DummyNetProvider`) that will never
    /// deliver resources. When true, callers must NOT register resources as
    /// "pending critical" — doing so blocks painting forever, since the
    /// completion callback never fires. Used by integrations that feed a
    /// pre-rendered DOM and perform no sub-fetches (e.g. aginxbrowser).
    fn is_noop(&self) -> bool {
        false
    }
}

/// A type that parses raw bytes from a network request into a Data and then calls
/// the NetCallack with the result.
pub trait NetHandler: Send + Sync + 'static {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes);
}

/// A callback which gets called every time a network request completes
// Q: Should we use std::task::Waker for this?
pub trait NetWaker: Send + Sync + 'static {
    fn wake(&self, client_id: usize);
}

impl<F: Fn(usize) + Send + Sync + 'static> NetWaker for F {
    fn wake(&self, doc_id: usize) {
        self(doc_id)
    }
}

/// The state of a present `crossorigin` content attribute
/// (<https://html.spec.whatwg.org/multipage/urls-and-fetching.html#cors-settings-attributes>).
///
/// An absent attribute is the `No CORS` state and is `None` where this type is
/// carried. A present attribute that names no keyword takes the invalid value
/// default, [`CorsSetting::Anonymous`], so a bad spelling never sends
/// credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CorsSetting {
    /// CORS mode with `same-origin` credentials.
    Anonymous,
    /// CORS mode with `include` credentials.
    UseCredentials,
}

impl CorsSetting {
    /// Parses one content attribute value, ASCII case-insensitively and
    /// untrimmed: enumerated attribute values keep their padding, so
    /// `" use-credentials"` is the invalid value default.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        if value.eq_ignore_ascii_case("use-credentials") {
            Self::UseCredentials
        } else {
            Self::Anonymous
        }
    }

    /// The canonical keyword the attribute reflects
    /// (<https://html.spec.whatwg.org/multipage/urls-and-fetching.html#dom-crossorigin>).
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::UseCredentials => "use-credentials",
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone)]
/// A request type loosely representing <https://fetch.spec.whatwg.org/#requests>
pub struct Request {
    pub url: Url,
    pub method: Method,
    pub content_type: Option<String>,
    pub headers: HeaderMap,
    pub body: Body,
    pub signal: Option<AbortSignal>,
    /// What the request fetches. Embedders apply per-destination policy from
    /// this instead of guessing from the URL
    /// (<https://fetch.spec.whatwg.org/#concept-request-destination>).
    pub kind: RequestKind,
    /// The element that initiated the request, when one did. The embedder
    /// reads its element-scoped state (`referrerpolicy`, CSP destination) from
    /// this node instead of matching a URL back to an element
    /// (<https://fetch.spec.whatwg.org/#concept-request-client>).
    pub node: Option<crate::node_id::NodeId>,
    /// The state of the `crossorigin` content attribute of the element that
    /// initiated the request. It is part of the image cache key, so two images
    /// on one URL with different CORS settings are different requests
    /// (<https://html.spec.whatwg.org/multipage/urls-and-fetching.html#cors-settings-attributes>).
    pub crossorigin: Option<CorsSetting>,
    /// The raw `referrerpolicy` content attribute value of the element that
    /// initiated the request, sampled when the request was created. HTML
    /// samples it with the CORS state; the embedder owns the keyword rule
    /// (<https://html.spec.whatwg.org/multipage/urls-and-fetching.html#referrer-policy-attribute>).
    pub referrer_policy: Option<String>,
    /// The CSS style sheet responsible for this request, when one is: the
    /// sheet's location is the request's referrer, and its response's
    /// `Referrer-Policy` is the request's policy
    /// (<https://w3c.github.io/webappsec-referrer-policy/#integration-with-css>).
    /// `None` for requests a document or an element initiates.
    pub style_sheet: Option<Url>,
}

/// The resource family a request belongs to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RequestKind {
    /// A destination the embedder does not need to distinguish.
    #[default]
    Other,
    /// A document stylesheet: `<link rel=stylesheet>` or `@import`.
    Stylesheet,
    /// An image resource.
    Image,
    /// A web font loaded for CSS `@font-face`.
    Font,
}

impl Request {
    /// A get request to the specified Url and an empty body
    pub fn get(url: Url) -> Self {
        Self {
            url,
            method: Method::GET,
            content_type: None,
            headers: HeaderMap::new(),
            body: Body::Empty,
            signal: None,
            kind: RequestKind::Other,
            node: None,
            crossorigin: None,
            referrer_policy: None,
            style_sheet: None,
        }
    }

    pub fn signal(mut self, signal: AbortSignal) -> Self {
        self.signal = Some(signal);
        self
    }

    /// Marks what the request fetches.
    pub fn kind(mut self, kind: RequestKind) -> Self {
        self.kind = kind;
        self
    }

    /// Marks the element that initiated the request.
    pub fn node(mut self, node: crate::node_id::NodeId) -> Self {
        self.node = Some(node);
        self
    }

    /// Marks the CORS settings state of the element that initiated the
    /// request.
    pub fn crossorigin(mut self, value: CorsSetting) -> Self {
        self.crossorigin = Some(value);
        self
    }

    /// Marks the raw `referrerpolicy` content attribute value of the element
    /// that initiated the request, sampled when the request was created.
    pub fn referrer_policy(mut self, value: String) -> Self {
        self.referrer_policy = Some(value);
        self
    }

    /// Marks the CSS style sheet this request belongs to, when one is
    /// responsible for it
    /// (<https://w3c.github.io/webappsec-referrer-policy/#integration-with-css>).
    pub fn style_sheet(mut self, url: Option<Url>) -> Self {
        self.style_sheet = url;
        self
    }
}

#[derive(Debug, Clone)]
pub enum Body {
    Bytes(Bytes),
    Form(FormData),
    Empty,
}

/// A list of form entries used for form submission
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FormData(pub Vec<Entry>);
impl FormData {
    /// Creates a new empty FormData
    pub fn new() -> Self {
        FormData(Vec::new())
    }
}
impl Serialize for FormData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq_serializer = serializer.serialize_seq(Some(self.len()))?;
        for entry in &self.0 {
            seq_serializer.serialize_element(entry)?;
        }
        seq_serializer.end()
    }
}
impl Deref for FormData {
    type Target = Vec<Entry>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A single form entry consisting of a name and value
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub value: EntryValue,
}
impl Serialize for Entry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut serializer = serializer.serialize_tuple(2)?;
        serializer.serialize_element(&self.name)?;
        match &self.value {
            EntryValue::String(s) => serializer.serialize_element(s)?,
            EntryValue::File(p) => serializer.serialize_element(p.to_str().unwrap_or_default())?,
            EntryValue::EmptyFile => serializer.serialize_element("")?,
        }
        serializer.end()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryValue {
    String(String),
    File(PathBuf),
    EmptyFile,
}
impl AsRef<str> for EntryValue {
    fn as_ref(&self) -> &str {
        match self {
            EntryValue::String(s) => s,
            EntryValue::File(p) => p.to_str().unwrap_or_default(),
            EntryValue::EmptyFile => "",
        }
    }
}

impl From<&str> for EntryValue {
    fn from(value: &str) -> Self {
        EntryValue::String(value.to_string())
    }
}
impl From<PathBuf> for EntryValue {
    fn from(value: PathBuf) -> Self {
        EntryValue::File(value)
    }
}

/// A default noop NetProvider
#[derive(Default)]
pub struct DummyNetProvider;
impl NetProvider for DummyNetProvider {
    fn fetch(&self, _doc_id: usize, _request: Request, _handler: Box<dyn NetHandler>) {}
    fn is_noop(&self) -> bool {
        true
    }
}

/// The AbortController interface represents a controller object that
/// allows you to abort one or more Web requests as and when desired.
///
/// <https://developer.mozilla.org/en-US/docs/Web/API/AbortController>
#[derive(Debug, Default)]
pub struct AbortController {
    pub signal: AbortSignal,
}

impl AbortController {
    /// The abort() method of the AbortController interface aborts
    /// an asynchronous operation before it has completed.
    /// This is able to abort fetch requests.
    ///
    /// <https://developer.mozilla.org/en-US/docs/Web/API/AbortController/abort>
    pub fn abort(self) {
        self.signal.0.store(true, Ordering::SeqCst);
    }
}

/// The AbortSignal interface represents a signal object that allows you to
/// communicate with an asynchronous operation (such as a fetch request) and
/// abort it if required via an AbortController object.
///
/// <https://developer.mozilla.org/en-US/docs/Web/API/AbortSignal>
#[derive(Debug, Default, Clone)]
pub struct AbortSignal(Arc<AtomicBool>);

impl AbortSignal {
    /// The aborted read-only property returns a value that indicates whether
    /// the asynchronous operations the signal is communicating with are
    /// aborted (true) or not (false).
    ///
    /// <https://developer.mozilla.org/en-US/docs/Web/API/AbortSignal/aborted>
    pub fn aborted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::CorsSetting;

    #[test]
    fn a_crossorigin_attribute_is_an_untrimmed_keyword() {
        assert_eq!(CorsSetting::parse(""), CorsSetting::Anonymous);
        // A padded value is invalid because an enumerated attribute does not
        // trim its value, and the invalid value default never sends
        // credentials
        // (<https://html.spec.whatwg.org/multipage/common-microsyntaxes.html#enumerated-attribute>).
        assert_eq!(
            CorsSetting::parse(" use-credentials"),
            CorsSetting::Anonymous
        );
        assert_eq!(
            CorsSetting::parse("USE-CREDENTIALS"),
            CorsSetting::UseCredentials
        );
        assert_eq!(CorsSetting::UseCredentials.keyword(), "use-credentials");
        assert_eq!(CorsSetting::Anonymous.keyword(), "anonymous");
    }
}

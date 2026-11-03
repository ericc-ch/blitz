use blitz_traits::node_id::NodeId;
use markup5ever::{LocalName, local_name};

use crate::{
    BaseDocument, ElementData,
    traversal::{AncestorTraverser, TreeTraverser},
};
use blitz_traits::{
    navigation::NavigationOptions,
    net::{Body, Entry, EntryValue, FormData, Method},
};
use core::str::FromStr;
use std::fmt::Display;

/// https://url.spec.whatwg.org/#default-encode-set
const DEFAULT_ENCODE_SET: percent_encoding::AsciiSet = percent_encoding::CONTROLS
    // Query Set
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    // Path Set
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

impl BaseDocument {
    /// Resets the form owner for a given node by either using an explicit form attribute
    /// or finding the nearest ancestor form element
    ///
    /// # Arguments
    /// * `node_id` - The ID of the node whose form owner needs to be reset
    ///
    /// <https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#reset-the-form-owner>
    pub fn reset_form_owner(&mut self, node_id: NodeId) {
        let node = &self.nodes[node_id];
        let Some(element) = node.element_data() else {
            return;
        };

        // First try explicit form attribute
        let final_owner_id = element
            .attr(local_name!("form"))
            .and_then(|owner| self.get_element_by_id(owner))
            .filter(|owner_id| {
                self.get_node(*owner_id)
                    .is_some_and(|node| node.data.is_element_with_tag_name(&local_name!("form")))
            })
            .or_else(|| {
                AncestorTraverser::new(self, node_id).find(|ancestor_id| {
                    self.nodes[*ancestor_id]
                        .data
                        .is_element_with_tag_name(&local_name!("form"))
                })
            });

        if let Some(final_owner_id) = final_owner_id {
            self.controls_to_form.insert(node_id, final_owner_id);
        } else {
            // A removed `form` attribute or a detached ancestor must not leave
            // the node pointing at the previous owner.
            self.controls_to_form.remove(&node_id);
        }
    }

    /// The form owner of a form-associated element, as last resolved by
    /// [`Self::reset_form_owner`]
    /// (<https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#form-owner>).
    pub fn form_owner(&self, node_id: NodeId) -> Option<NodeId> {
        self.controls_to_form.get(&node_id).copied()
    }

    /// Resolves the form's submission attributes, using the submitter's
    /// `form*` overrides when it has them
    /// (<https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#form-submission-algorithm>).
    ///
    /// Returns `None` when the form is not a form element, the action does not
    /// resolve, or the method has no navigation form (`dialog`).
    pub fn plan_form_submission(
        &self,
        node_id: NodeId,
        submitter_id: Option<NodeId>,
    ) -> Option<FormPlan> {
        let node = self.get_node(node_id)?;
        node.data.is_element_with_tag_name(&local_name!("form")).then_some(())?;
        let element = node.element_data()?;
        let method = get_form_attr(
            self,
            element,
            local_name!("method"),
            submitter_id,
            local_name!("formmethod"),
        )
        .and_then(|method| method.parse::<FormMethod>().ok())
        .unwrap_or(FormMethod::Get);
        if method == FormMethod::Dialog {
            return None;
        }
        let action = get_form_attr(
            self,
            element,
            local_name!("action"),
            submitter_id,
            local_name!("formaction"),
        )
        .unwrap_or_default();
        let action = self.resolve_url(action)?;
        let enctype = get_form_attr(
            self,
            element,
            local_name!("enctype"),
            submitter_id,
            local_name!("formenctype"),
        )
        .and_then(|enctype| enctype.parse::<RequestContentType>().ok())
        .unwrap_or(RequestContentType::FormUrlEncoded);
        let target = get_form_attr(
            self,
            element,
            local_name!("target"),
            submitter_id,
            local_name!("formtarget"),
        )
        .filter(|target| !target.is_empty())
        .map(str::to_owned);
        Some(FormPlan {
            action,
            method,
            enctype,
            target,
        })
    }

    /// A form's entry list, in tree order
    /// (<https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#constructing-the-form-data-set>).
    ///
    /// File controls are reported by node id rather than by path: the renderer
    /// resolves the JS `File` objects the page assigned, while the fork's own
    /// navigation reads `ElementData::file_data`.
    pub(crate) fn form_entry_list(
        &self,
        form_id: NodeId,
        submitter_id: Option<NodeId>,
    ) -> Vec<FormEntry> {
        let mut entry_list = Vec::new();
        // A missing form (a stale node id) has no controls.
        if self
            .get_node(form_id)
            .and_then(|node| node.element_data())
            .is_none()
        {
            return entry_list;
        }

        fn datalist_ancestor(doc: &BaseDocument, node_id: NodeId) -> bool {
            AncestorTraverser::new(doc, node_id).any(|node_id| {
                doc.nodes[node_id]
                    .data
                    .is_element_with_tag_name(&local_name!("datalist"))
            })
        }

        // For each element field in controls, in tree order:
        for control_id in TreeTraverser::new(self) {
            let Some(node) = self.get_node(control_id) else {
                continue;
            };
            let Some(element) = node.element_data() else {
                continue;
            };

            if self.form_owner(control_id) != Some(form_id) {
                continue;
            }

            let element_type = element.attr(local_name!("type"));
            let type_is = |name: &str| {
                element_type.is_some_and(|value| value.eq_ignore_ascii_case(name))
            };

            // A datalist descendant, a disabled field, a non-submitter button,
            // or an unchecked checkbox/radio contributes nothing.
            if datalist_ancestor(self, node.id)
                || element.attr(local_name!("disabled")).is_some()
                || self.disabled_by_fieldset(node.id)
                || (element.name.local == local_name!("button")
                    && Some(node.id) != submitter_id)
                || (element.name.local == local_name!("input")
                    && ((type_is("checkbox") || type_is("radio"))
                        && !element.checkbox_input_checked().unwrap_or(false))
                    || type_is("submit")
                    || type_is("button"))
            {
                continue;
            }

            // An image button contributes its selected coordinates. With no
            // name the entries are bare `x` / `y`; otherwise they are
            // `<name>.x` / `<name>.y`. No activation point is carried on the
            // element, so the selected coordinate is (0, 0).
            if element.name.local == local_name!("input") && type_is("image") {
                if Some(node.id) != submitter_id {
                    continue;
                }
                let name = element.attr(local_name!("name")).unwrap_or_default();
                let prefix = if name.is_empty() {
                    String::new()
                } else {
                    format!("{name}.")
                };
                entry_list.push(FormEntry {
                    name: format!("{prefix}x"),
                    value: FormEntryValue::Text("0".to_owned()),
                });
                entry_list.push(FormEntry {
                    name: format!("{prefix}y"),
                    value: FormEntryValue::Text("0".to_owned()),
                });
                continue;
            }

            let Some(name) = element.attr(local_name!("name")).filter(|name| !name.is_empty())
            else {
                continue;
            };

            if element.name.local == local_name!("input")
                && (type_is("checkbox") || type_is("radio"))
            {
                let value = element.attr(local_name!("value")).unwrap_or("on");
                entry_list.push(FormEntry {
                    name: name.to_owned(),
                    value: FormEntryValue::Text(value.to_owned()),
                });
                continue;
            }

            #[cfg(feature = "file-input")]
            if element.name.local == local_name!("input") && type_is("file") {
                entry_list.push(FormEntry {
                    name: name.to_owned(),
                    value: FormEntryValue::File(node.id),
                });
                continue;
            }

            if element.name.local == local_name!("input")
                && type_is("hidden")
                && name.eq_ignore_ascii_case("_charset_")
            {
                entry_list.push(FormEntry {
                    name: name.to_owned(),
                    value: FormEntryValue::Text("UTF-8".to_owned()),
                });
            } else if let Some(text) = element.text_input_data() {
                entry_list.push(FormEntry {
                    name: name.to_owned(),
                    value: FormEntryValue::Text(text.editor.text().to_string()),
                });
            } else if let Some(value) = element.attr(local_name!("value")) {
                entry_list.push(FormEntry {
                    name: name.to_owned(),
                    value: FormEntryValue::Text(value.to_owned()),
                });
            }
        }
        entry_list
    }

    /// Whether `node` is disabled because a `fieldset` ancestor is disabled
    /// and `node` is not inside that fieldset's first `legend`
    /// (<https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#concept-fe-disabled>).
    pub fn disabled_by_fieldset(&self, node_id: NodeId) -> bool {
        let mut node = node_id;
        while let Some(parent) = self.get_node(node).and_then(|current| current.parent) {
            let is_fieldset = self.get_node(parent).is_some_and(|current| {
                current.data.is_element_with_tag_name(&local_name!("fieldset"))
                    && current.data.has_attr(local_name!("disabled"))
            });
            if is_fieldset && !self.inside_first_legend(node, parent) {
                return true;
            }
            node = parent;
        }
        false
    }

    /// Whether `node` is inside the first `legend` child of `fieldset`.
    fn inside_first_legend(&self, node: NodeId, fieldset: NodeId) -> bool {
        let legend = self.get_node(fieldset).and_then(|current| {
            current.children.iter().copied().find(|child| {
                self.get_node(*child)
                    .is_some_and(|node| node.data.is_element_with_tag_name(&local_name!("legend")))
            })
        });
        let Some(legend) = legend else {
            return false;
        };
        // Walk from `node` to the fieldset; the legend is on the path when the
        // node is one of its descendants.
        let mut current = node;
        while let Some(parent) = self.get_node(current).and_then(|current| current.parent) {
            if parent == legend {
                return true;
            }
            if parent == fieldset {
                return false;
            }
            current = parent;
        }
        false
    }

    /// Submits a form with the given form node ID and submitter node ID
    ///
    /// <https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#form-submission-algorithm>
    pub fn submit_form(&self, node_id: NodeId, submitter_id: NodeId) {
        let Some(plan) = self.plan_form_submission(node_id, Some(submitter_id)) else {
            return;
        };
        let entry = self.form_data_from_entries(self.form_entry_list(node_id, Some(submitter_id)));
        let mut parsed_action = plan.action;
        let scheme = parsed_action.scheme();

        let mut post_resource = Body::Empty;

        match (scheme, plan.method) {
            ("http" | "https" | "data", FormMethod::Get) => {
                let pairs = convert_to_list_of_name_value_pairs(entry);
                let mut query = String::new();
                url::form_urlencoded::Serializer::new(&mut query).extend_pairs(pairs);
                parsed_action.set_query(Some(&query));
            }
            ("http" | "https", FormMethod::Post) => post_resource = Body::Form(entry),
            ("mailto", FormMethod::Get) => {
                let pairs = convert_to_list_of_name_value_pairs(entry);
                parsed_action.query_pairs_mut().extend_pairs(pairs);
            }
            ("mailto", FormMethod::Post) => {
                let pairs = convert_to_list_of_name_value_pairs(entry);
                let body = match plan.enctype {
                    RequestContentType::TextPlain => {
                        let body = encode_text_plain(&pairs);
                        percent_encoding::utf8_percent_encode(&body, &DEFAULT_ENCODE_SET)
                            .to_string()
                    }
                    _ => {
                        let mut body = String::new();
                        url::form_urlencoded::Serializer::new(&mut body).extend_pairs(pairs);
                        body
                    }
                };
                let mut query = if let Some(query) = parsed_action.query() {
                    let mut query = query.to_string();
                    query.push('&');
                    query
                } else {
                    String::new()
                };
                query.push_str("body=");
                query.push_str(&body);
                parsed_action.set_query(Some(&query));
            }
            _ => {
                #[cfg(feature = "tracing")]
                tracing::warn!(
                    "Scheme {} with method {:?} is not implemented",
                    scheme,
                    plan.method
                );
                return;
            }
        }

        let method = plan.method.try_into().unwrap_or_default();

        let navigation_options =
            NavigationOptions::new(parsed_action, Some(plan.enctype.to_string()), self.id())
                .set_document_resource(post_resource)
                .set_method(method);

        self.navigation_provider.navigate_to(navigation_options)
    }

    /// Turns the node-aware entry list into the navigation body's entries.
    fn form_data_from_entries(&self, entries: Vec<FormEntry>) -> FormData {
        let mut form_data = FormData::new();
        for entry in entries {
            match entry.value {
                FormEntryValue::Text(value) => form_data.0.push(Entry {
                    name: entry.name,
                    value: EntryValue::String(value),
                }),
                FormEntryValue::File(node_id) => {
                    let files = self
                        .get_node(node_id)
                        .and_then(|node| node.element_data())
                        .and_then(|element| element.file_data());
                    if let Some(files) = files {
                        if files.is_empty() {
                            form_data.0.push(Entry {
                                name: entry.name,
                                value: EntryValue::EmptyFile,
                            });
                        } else {
                            for path in files.iter() {
                                form_data.0.push(Entry {
                                    name: entry.name.clone(),
                                    value: EntryValue::File(path.clone()),
                                });
                            }
                        }
                    } else {
                        form_data.0.push(Entry {
                            name: entry.name,
                            value: EntryValue::EmptyFile,
                        });
                    }
                }
            }
        }
        form_data
    }
}

/// One control's contribution to a form's entry list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FormEntryValue {
    /// A text-like value (a normal control, a checkbox value, or `_charset_`).
    Text(String),
    /// A file control; `submit_form` reads its `ElementData::file_data`.
    File(NodeId),
}

/// One form entry, with the source control a file value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormEntry {
    pub(crate) name: String,
    pub(crate) value: FormEntryValue,
}

/// The resolved method, action, enctype, and target of one submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormPlan {
    pub action: url::Url,
    pub method: FormMethod,
    pub enctype: RequestContentType,
    /// The `target`/`formtarget` value, absent when the attribute is missing or
    /// empty.
    pub target: Option<String>,
}

fn get_form_attr<'a>(
    doc: &'a BaseDocument,
    form: &'a ElementData,
    form_local: impl PartialEq<LocalName>,
    submitter_id: Option<NodeId>,
    submitter_local: impl PartialEq<LocalName>,
) -> Option<&'a str> {
    submitter_id
        .and_then(|submitter_id| get_submitter_attr(doc, submitter_id, submitter_local))
        .or_else(|| form.attr(form_local))
}

fn get_submitter_attr(
    doc: &BaseDocument,
    submitter_id: NodeId,
    local_name: impl PartialEq<LocalName>,
) -> Option<&str> {
    doc.get_node(submitter_id)
        .and_then(|node| node.element_data())
        .and_then(|element_data| {
            // Any submit button can carry `form*` overrides: a `<button>`
            // (whose type defaults to `submit`), a `<input type=submit>`, and a
            // `<input type=image>`
            // (<https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#form-submission-algorithm>).
            let is_submitter = match element_data.name.local.as_str() {
                "button" => element_data.is_submit_button(),
                "input" => element_data.attr(local_name!("type")).is_some_and(|kind| {
                    kind.eq_ignore_ascii_case("submit") || kind.eq_ignore_ascii_case("image")
                }),
                _ => false,
            };
            is_submitter
                .then(|| element_data.attr(local_name))
                .flatten()
        })
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FormMethod {
    Get,
    Post,
    Dialog,
}

impl FormMethod {
    /// The canonical keyword the `method`/`formmethod` getters return.
    pub fn as_str(self) -> &'static str {
        match self {
            FormMethod::Get => "get",
            FormMethod::Post => "post",
            FormMethod::Dialog => "dialog",
        }
    }
}

impl FromStr for FormMethod {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Enumerated matching is ASCII case-insensitive and untrimmed
        // (<https://html.spec.whatwg.org/multipage/common-microsyntaxes.html#keywords-and-enumerated-attributes>).
        Ok(match () {
            _ if s.eq_ignore_ascii_case("get") => FormMethod::Get,
            _ if s.eq_ignore_ascii_case("post") => FormMethod::Post,
            _ if s.eq_ignore_ascii_case("dialog") => FormMethod::Dialog,
            _ => return Err(()),
        })
    }
}

impl Display for FormMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
impl TryFrom<FormMethod> for Method {
    type Error = &'static str;
    fn try_from(method: FormMethod) -> Result<Self, Self::Error> {
        Ok(match method {
            FormMethod::Get => Method::GET,
            FormMethod::Post => Method::POST,
            FormMethod::Dialog => return Err("Dialog is not an HTTP method"),
        })
    }
}
/// Supported content types for HTTP requests
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestContentType {
    /// application/x-www-form-urlencoded
    FormUrlEncoded,
    /// multipart/form-data
    MultipartFormData,
    /// text/plain
    TextPlain,
}

impl RequestContentType {
    /// The canonical keyword the `enctype`/`formenctype` getters return.
    pub fn as_str(self) -> &'static str {
        match self {
            RequestContentType::FormUrlEncoded => "application/x-www-form-urlencoded",
            RequestContentType::MultipartFormData => "multipart/form-data",
            RequestContentType::TextPlain => "text/plain",
        }
    }
}

impl FromStr for RequestContentType {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Enumerated matching is ASCII case-insensitive and untrimmed
        // (<https://html.spec.whatwg.org/multipage/common-microsyntaxes.html#keywords-and-enumerated-attributes>).
        Ok(match () {
            _ if s.eq_ignore_ascii_case("application/x-www-form-urlencoded") => {
                RequestContentType::FormUrlEncoded
            }
            _ if s.eq_ignore_ascii_case("multipart/form-data") => {
                RequestContentType::MultipartFormData
            }
            _ if s.eq_ignore_ascii_case("text/plain") => RequestContentType::TextPlain,
            _ => return Err(()),
        })
    }
}

impl Display for RequestContentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Converts the entry list to a vector of name-value pairs with normalized line endings
/// https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#converting-an-entry-list-to-a-list-of-name-value-pairs
fn convert_to_list_of_name_value_pairs(form_data: FormData) -> Vec<(String, String)> {
    form_data
        .iter()
        .map(|Entry { name, value }| {
            let name = normalize_line_endings(name.as_ref());
            let value = normalize_line_endings(value.as_ref());
            (name, value)
        })
        .collect()
}

/// Normalizes line endings in a string according to HTML spec
/// Converts single CR or LF to CRLF pairs according to HTML form submission requirements
fn normalize_line_endings(input: &str) -> String {
    // Replace every occurrence of U+000D (CR) not followed by U+000A (LF),
    // and every occurrence of U+000A (LF) not preceded by U+000D (CR),
    // in value, by a string consisting of U+000D (CR) and U+000A (LF).

    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(current) = chars.next() {
        match (current, chars.peek()) {
            ('\r', Some('\n')) => {
                result.push_str("\r\n");
                chars.next();
            }
            ('\r' | '\n', _) => {
                result.push_str("\r\n");
            }
            _ => result.push(current),
        }
    }

    result
}

/// Encodes form data as text/plain according to HTML spec given an slice of name-value pairs
/// https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#text/plain-encoding-algorithm
fn encode_text_plain<T: AsRef<str>, U: AsRef<str>>(input: &[(T, U)]) -> String {
    let mut out = String::new();
    for (name, value) in input {
        out.push_str(name.as_ref());
        out.push('=');
        out.push_str(value.as_ref());
        out.push_str("\r\n");
    }
    out
}

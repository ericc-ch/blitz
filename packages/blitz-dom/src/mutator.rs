use blitz_traits::node_id::NodeId;
use markup5ever::LocalName;
use std::collections::HashSet;
use std::mem;
use std::ops::{Deref, DerefMut, Range};

use crate::document::{ImageKey, ImageOutcome, ImageRequestState, ImageRequestStatus};
use crate::layout::damage::ALL_DAMAGE;
use crate::net::{ImageHandler, ResourceHandler};
use crate::node::{CanvasData, Dirtiness, ImageData, NodeFlags, SpecialElementData};
use crate::stylo_device::DeviceChanges;
use crate::util::ImageType;
use crate::{
    Attribute, BaseDocument, Document, ElementData, Node, NodeData, QualName, local_name, qual_name,
};
use blitz_traits::shell::Viewport;
use selectors::matching::ElementSelectorFlags;
use style::Atom;
use style::invalidation::element::restyle_hints::RestyleHint;
use style::stylesheets::OriginSet;
use style_dom::ElementState;
use thin_vec::ThinVec;

macro_rules! tag_and_attr {
    ($tag:tt, $attr:tt) => {
        (&local_name!($tag), &local_name!($attr))
    };
}

#[derive(Debug, Clone)]
pub enum AppendTextErr {
    /// The node is not a text node
    NotTextNode,
}

fn node_inside_shadow(doc: &BaseDocument, node_id: NodeId) -> bool {
    let mut current = doc.nodes[node_id].parent;
    while let Some(id) = current {
        if matches!(doc.nodes[id].data, NodeData::ShadowRoot { .. }) {
            return true;
        }
        current = doc.nodes[id].parent;
    }
    false
}

/// Operations that happen almost immediately, but are deferred within a
/// function for borrow-checker reasons.
enum SpecialOp {
    LoadImage(NodeId),
    LoadIframe(NodeId),
    LoadStylesheet(NodeId),
    /// A `<?xml-stylesheet?>` processing instruction names a style sheet
    /// (<https://www.w3.org/TR/xml-stylesheet/>).
    LoadStylesheetPi(NodeId),
    UnloadStylesheet(NodeId),
    /// Drops the sheet a processing instruction owned; it has no element
    /// special data to take.
    UnloadStylesheetPi(NodeId),
    LoadCustomPaintSource(NodeId),
    ProcessButtonInput(NodeId),
    UnloadSubDocument(NodeId),
    #[cfg(feature = "custom-widget")]
    UnloadCustomWidget(NodeId),
}

pub struct DocumentMutator<'doc> {
    /// Document is public as an escape hatch, but users of this API should ideally avoid using it
    /// and prefer exposing additional functionality in DocumentMutator.
    pub doc: &'doc mut BaseDocument,

    eager_op_queue: Vec<SpecialOp>,

    // Tracked nodes for deferred processing when mutations have completed
    title_node: Option<NodeId>,
    style_nodes: HashSet<NodeId>,
    form_nodes: HashSet<NodeId>,

    /// Whether an element/attribute that affect animation status has been seen
    recompute_is_animating: bool,

    /// Whether any mutation that affects rendered output has been performed
    mutations_occurred: bool,

    /// The (latest) node which has been mounted in and had autofocus=true, if any
    #[cfg(feature = "autofocus")]
    node_to_autofocus: Option<NodeId>,
}

impl Drop for DocumentMutator<'_> {
    fn drop(&mut self) {
        self.flush(); // Defined at bottom of file
        if self.mutations_occurred {
            self.doc.shell_provider.request_redraw();
        }
    }
}

impl DocumentMutator<'_> {
    pub fn new<'doc>(doc: &'doc mut BaseDocument) -> DocumentMutator<'doc> {
        DocumentMutator {
            doc,
            eager_op_queue: Vec::new(),
            title_node: None,
            style_nodes: HashSet::new(),
            form_nodes: HashSet::new(),
            recompute_is_animating: false,
            mutations_occurred: false,
            #[cfg(feature = "autofocus")]
            node_to_autofocus: None,
        }
    }

    // Query methods

    pub fn node_has_parent(&self, node_id: NodeId) -> bool {
        self.doc.nodes[node_id].parent.is_some()
    }

    pub fn previous_sibling_id(&self, node_id: NodeId) -> Option<NodeId> {
        self.doc.nodes[node_id].backward(1).map(|node| node.id)
    }

    pub fn next_sibling_id(&self, node_id: NodeId) -> Option<NodeId> {
        self.doc.nodes[node_id].forward(1).map(|node| node.id)
    }

    pub fn parent_id(&self, node_id: NodeId) -> Option<NodeId> {
        self.doc.nodes[node_id].parent
    }

    pub fn last_child_id(&self, node_id: NodeId) -> Option<NodeId> {
        self.doc.nodes[node_id].children.last().copied()
    }

    pub fn child_ids(&self, node_id: NodeId) -> ThinVec<NodeId> {
        self.doc.nodes[node_id].children.clone()
    }

    pub fn element_name(&self, node_id: NodeId) -> Option<&QualName> {
        self.doc.nodes[node_id].element_data().map(|el| &el.name)
    }

    pub fn node_at_path(&self, start_node_id: NodeId, path: &[u8]) -> NodeId {
        let mut current = &self.doc.nodes[start_node_id];
        for i in path {
            let new_id = current.children[*i as usize];
            current = &self.doc.nodes[new_id];
        }
        current.id
    }

    // Node creation methods

    pub fn create_comment_node(&mut self, contents: &str) -> NodeId {
        self.doc.create_node(NodeData::Comment {
            contents: contents.to_string(),
        })
    }

    pub fn create_doctype_node(&mut self, name: &str, public_id: &str, system_id: &str) -> NodeId {
        self.doc.create_node(NodeData::Doctype {
            name: name.to_string(),
            public_id: public_id.to_string(),
            system_id: system_id.to_string(),
        })
    }

    pub fn create_processing_instruction_node(&mut self, target: &str, contents: &str) -> NodeId {
        let attributes = crate::pseudo_attributes::parse(contents).unwrap_or_default();
        self.create_processing_instruction_node_with_attributes(target, contents, attributes)
    }

    /// Creates a processing instruction with an attribute map that may not be
    /// the one its data parses to: the DOM allows attribute names the XML
    /// grammar rejects, and a cross-document import carries the map with the
    /// node.
    pub fn create_processing_instruction_node_with_attributes(
        &mut self,
        target: &str,
        contents: &str,
        attributes: Vec<(String, String)>,
    ) -> NodeId {
        self.doc.create_node(NodeData::ProcessingInstruction {
            target: target.to_string(),
            contents: contents.to_string(),
            attributes,
        })
    }

    /// Replaces a processing instruction's data and re-derives its
    /// pseudo-attributes. A parse error leaves the accessors an empty map, and
    /// the loader parses the same data itself and loads nothing
    /// (<https://www.w3.org/TR/xml-stylesheet/#dt-parsing>).
    pub fn set_processing_instruction_data(&mut self, node_id: NodeId, data: &str) {
        let Some(node) = self.doc.nodes.get_mut(node_id) else {
            return;
        };
        let NodeData::ProcessingInstruction {
            contents,
            attributes,
            ..
        } = &mut node.data
        else {
            return;
        };
        contents.clear();
        contents.push_str(data);
        *attributes = crate::pseudo_attributes::parse(data).unwrap_or_default();
        self.refresh_pi_stylesheet(node_id);
    }

    /// Replaces a processing instruction's pseudo-attributes, serializing
    /// them into its data in the same step. A name the XML grammar rejects (an
    /// attribute local name such as `$`) would otherwise be lost; one step
    /// leaves no window where the two disagree
    /// (<https://dom.spec.whatwg.org/#update-data-from-attributes>).
    ///
    /// Returns the serialized data it stored, so the embedder's exact-unit
    /// bookkeeping and live ranges use the same string the tree holds.
    pub fn set_processing_instruction_attributes(
        &mut self,
        node_id: NodeId,
        attributes: Vec<(String, String)>,
    ) -> String {
        let data = crate::pseudo_attributes::serialize(&attributes);
        let Some(node) = self.doc.nodes.get_mut(node_id) else {
            return data;
        };
        let NodeData::ProcessingInstruction {
            contents,
            attributes: stored,
            ..
        } = &mut node.data
        else {
            return data;
        };
        contents.clear();
        contents.push_str(&data);
        *stored = attributes;
        self.refresh_pi_stylesheet(node_id);
        data
    }

    /// Reloads an `xml-stylesheet` processing instruction that is already in
    /// the document. A data or attribute write changes what the instruction
    /// names, and Blink and Gecko both re-check the sheet from the write
    /// (<https://www.w3.org/TR/xml-stylesheet/>).
    fn refresh_pi_stylesheet(&mut self, node_id: NodeId) {
        let Some(node) = self.doc.nodes.get(node_id) else {
            return;
        };
        if !node.flags.is_in_document() {
            return;
        }
        let NodeData::ProcessingInstruction { target, .. } = &node.data else {
            return;
        };
        if !target.eq_ignore_ascii_case("xml-stylesheet") {
            return;
        }
        // A response for the previous data must not install a sheet the
        // instruction no longer names; the load records only the request it
        // starts.
        self.doc.pi_stylesheet_requests.remove(&node_id);
        self.unload_pi_stylesheet(node_id);
        self.load_pi_stylesheet(node_id);
    }

    pub fn create_cdata_section_node(&mut self, contents: &str) -> NodeId {
        self.doc.create_node(NodeData::CDataSection {
            contents: contents.to_string(),
        })
    }

    pub fn create_fragment_node(&mut self) -> NodeId {
        self.doc.create_node(NodeData::Fragment { host: None })
    }

    /// Whether `id` is an HTML `template` element, the only element with
    /// template contents
    /// (<https://html.spec.whatwg.org/multipage/scripting.html#the-template-element>).
    pub fn is_template_element(&self, id: NodeId) -> bool {
        self.doc.get_node(id).is_some_and(|node| {
            node.data.downcast_element().is_some_and(|element| {
                element.name.ns == markup5ever::ns!(html)
                    && element.name.local == markup5ever::local_name!("template")
            })
        })
    }

    pub fn create_text_node(&mut self, text: &str) -> NodeId {
        self.doc.create_text_node(text)
    }

    pub fn create_element(&mut self, name: QualName, attrs: Vec<Attribute>) -> NodeId {
        let mut data = ElementData::new(name, attrs);
        data.flush_style_attribute(
            self.doc.guard(),
            &self.doc.url.url_extra_data(),
            self.doc.quirks_mode(),
        );

        // A new HTML `template` element is born with empty template contents
        // (<https://html.spec.whatwg.org/multipage/scripting.html#the-template-element>).
        let is_template = data.name.ns == markup5ever::ns!(html)
            && data.name.local == markup5ever::local_name!("template");

        let id = self.doc.create_node(NodeData::Element(Box::new(data)));
        let node = self
            .doc
            .get_node_mut(id)
            .expect("freshly created node exists");

        // Initialise style data
        *node.stylo_element_data_mut().ensure_init_mut() = style::data::ElementData {
            damage: ALL_DAMAGE,
            ..Default::default()
        };

        if is_template {
            self.template_contents(id);
        }

        id
    }

    pub fn deep_clone_node(&mut self, node_id: NodeId) -> NodeId {
        self.doc.deep_clone_node(node_id)
    }

    /// Create (or return the existing) "template contents" fragment node for a
    /// `<template>` element. The contents node is detached from the tree (so it
    /// is inert: never styled or rendered) and is exposed to JavaScript as the
    /// template's `content` DocumentFragment.
    pub fn template_contents(&mut self, template: NodeId) -> NodeId {
        debug_assert!(
            self.is_template_element(template),
            "template contents belong on <template> only"
        );
        if let Some(contents) = self.try_template_contents(template) {
            // Backfill the host link for fragments created before it existed.
            if let Some(node) = self.doc.get_node_mut(contents)
                && let NodeData::Fragment { host } = &mut node.data
                && host.is_none()
            {
                *host = Some(template);
            }
            return contents;
        }
        let fragment = self.create_fragment_node();
        if let Some(node) = self.doc.get_node_mut(fragment)
            && let NodeData::Fragment { host } = &mut node.data
        {
            *host = Some(template);
        }
        if let Some(node) = self.doc.get_node_mut(template)
            && let Some(element) = node.data.downcast_element_mut()
        {
            element.template_contents = Some(fragment);
        }
        fragment
    }

    /// The "template contents" fragment node of a `<template>` element (if any)
    pub fn try_template_contents(&self, template_id: NodeId) -> Option<NodeId> {
        self.doc
            .get_node(template_id)
            .and_then(|node| node.element_data())
            .and_then(|element| element.template_contents)
    }

    // Node mutation methods

    pub fn set_node_text(&mut self, node_id: NodeId, value: &str) {
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        let node = &mut self.doc.nodes[node_id];

        let text = match node.data {
            NodeData::Text(ref mut text) => text,
            // TODO: otherwise this is basically element.textContent which is a bit different - need to parse as html
            _ => return,
        };

        let changed = text.content != value;
        if changed {
            self.mutations_occurred |= node_is_in_document;
            text.content.clear();
            text.content.push_str(value);
            node.insert_damage(ALL_DAMAGE);
            // Mark ancestors dirty so the style traversal visits this subtree.
            // Without this, the traversal may skip nodes with pending damage.
            node.mark_ancestors_dirty();
            let parent_id = node.parent;

            // Also insert damage on the parent element, since text content changes
            // affect the parent's layout (text may wrap differently, change size, etc.)
            if let Some(parent_id) = parent_id {
                let parent = &mut self.doc.nodes[parent_id];
                parent.insert_damage(ALL_DAMAGE);
            }

            self.maybe_record_node(parent_id);
            if let Some(parent_id) = parent_id {
                self.note_placeholder_shown(parent_id);
            }
        }
    }

    pub fn append_text_to_node(
        &mut self,
        node_id: NodeId,
        text: &str,
    ) -> Result<(), AppendTextErr> {
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        let node = &mut self.doc.nodes[node_id];
        node.insert_damage(ALL_DAMAGE);
        node.mark_ancestors_dirty();
        let parent_id = node.parent;
        match node.text_data_mut() {
            Some(data) => {
                data.content += text;
                self.mutations_occurred |= node_is_in_document;
                if let Some(parent_id) = parent_id {
                    self.note_placeholder_shown(parent_id);
                    self.doc.nodes[parent_id].insert_damage(ALL_DAMAGE);
                }
                Ok(())
            }
            None => Err(AppendTextErr::NotTextNode),
        }
    }

    pub fn add_attrs_if_missing(&mut self, node_id: NodeId, attrs: Vec<Attribute>) {
        let node = &mut self.doc.nodes[node_id];
        node.insert_damage(ALL_DAMAGE);
        let element_data = node.element_data_mut().expect("Not an element");

        let existing_names = element_data
            .attrs
            .iter()
            .map(|e| e.name.clone())
            .collect::<HashSet<_>>();

        for attr in attrs
            .into_iter()
            .filter(|attr| !existing_names.contains(&attr.name))
        {
            self.set_attribute(node_id, attr.name, &attr.value);
        }
    }

    pub fn set_attribute(&mut self, node_id: NodeId, name: QualName, value: &str) {
        let quirks = self.doc.quirks_mode();
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        if node_is_in_document {
            self.doc.snapshot_node(node_id, &name.local);

            self.restyle_for_attribute_change(node_id, &name.local);
        }

        if name.local == local_name!("id") && node_is_in_document {
            if let Some(old_id) = self.doc.nodes[node_id]
                .element_data()
                .map(|element| element.id.clone())
            {
                if let Some(old_id) = old_id {
                    self.doc.remove_from_id_map(&old_id, node_id);
                }
                self.doc.add_to_id_map(value, node_id);
            }
        }

        let refresh_placeholder = matches!(
            name.local,
            ref local
                if local == &local_name!("value")
                    || local == &local_name!("placeholder")
                    || local == &local_name!("type")
        );
        let textarea_text_empty = refresh_placeholder
            && self.doc.nodes[node_id]
                .element_data()
                .is_some_and(|element| element.name.local == local_name!("textarea"))
            && self.doc.nodes[node_id].text_content().is_empty();

        let node = &mut self.doc.nodes[node_id];

        let NodeData::Element(ref mut element) = node.data else {
            return;
        };

        self.mutations_occurred |= node_is_in_document;
        // If element is a CustomWidget, then Ccall attribute_changed on it
        #[cfg(feature = "custom-widget")]
        if let SpecialElementData::CustomWidget(widget_data) = &mut element.special_data {
            let old_value = element.attrs.get(&name).as_ref().map(|attr| &*attr.value);
            widget_data
                .widget
                .attribute_changed(&name.local, old_value, Some(value));
        }

        element.attrs.set(name.clone(), value);
        if refresh_placeholder && name.local != local_name!("value") {
            element.sync_placeholder_shown(textarea_text_empty);
        }

        // Focusability is cached on the element and comes from these
        // attributes, so it has to follow a change to one of them: a widget
        // that hands the focus around its own children - a menu, a grid -
        // sets their tabindex after creating them.
        if name.local == local_name!("tabindex")
            || name.local == local_name!("href")
            || name.local == local_name!("disabled")
        {
            element.flush_is_focussable();
        }

        if name.local == local_name!("href") {
            element.flush_link_state();
        }

        if name.local == local_name!("open") {
            element.flush_open_state();
        }

        let attr = &name.local;
        let checked_attribute =
            *attr == local_name!("checked") && element.name.local == local_name!("input");
        let selected_attribute =
            *attr == local_name!("selected") && element.name.local == local_name!("option");

        if *attr == local_name!("id") {
            element.id = Some(Atom::from(value))
        }

        if *attr == local_name!("class") {
            element.classes = crate::node::parse_classes(value);
        }

        if *attr == local_name!("value") {
            // A control's current value follows the attribute while its dirty
            // value flag is clear. Other elements with a `value` attribute
            // (`<li>`, `<button>`, `<data>`) have no current value
            // (<https://html.spec.whatwg.org/multipage/input.html#concept-fe-value>).
            if !element.form_state.dirty_value() && element.name.local == local_name!("input") {
                element.store_value(value.to_owned(), Dirtiness::Keep);
                if let Some(input_data) = element.text_input_data_mut() {
                    input_data.set_text(
                        &mut self.doc.font_ctx.lock().unwrap(),
                        &mut self.doc.layout_ctx,
                        value,
                    );
                }
            }
            if refresh_placeholder {
                element.sync_placeholder_shown(textarea_text_empty);
            }
            return;
        }

        // The `type` attribute decides whether an input's checkedness applies
        // to `:checked` at all, so the selector bit follows a type change
        // (<https://drafts.csswg.org/selectors/#checked-pseudo>).
        if *attr == local_name!("type") && element.name.local == local_name!("input") {
            element.sync_checked_state();
        }

        // The checkedness and selectedness slots follow their attributes
        // whether or not the node is in a document: a detached input and a
        // `new Option()` both read them before insertion.
        if checked_attribute && !element.form_state.dirty_checkedness() {
            element.store_checkedness(true, Dirtiness::Keep);
        } else if selected_attribute && !element.form_state.dirty_selectedness() {
            element.store_selectedness(true, Dirtiness::Keep);
        }

        if *attr == local_name!("style") {
            element.flush_style_attribute(&self.doc.guard, &self.doc.url.url_extra_data(), quirks);
            node.set_restyle_hint(RestyleHint::RESTYLE_STYLE_ATTRIBUTE);
            return;
        }

        if *attr == local_name!("disabled") && element.can_be_disabled() {
            node.disable();
            return;
        }

        let tag = &element.name.local;

        // An `<img>` loads from its `src` even while detached: `new Image()`
        // never enters a document. This must run before the detached
        // early-return below.
        if (tag, attr) == tag_and_attr!("img", "src") {
            self.load_image(node_id, ImageMutation::Src);
            return;
        }
        // A source set is a relevant mutation: it restarts the request only
        // when the selected source changes, which `load_image` decides by
        // comparing request keys
        // (<https://html.spec.whatwg.org/multipage/images.html#reacting-to-dom-mutations>).
        if (tag, attr) == tag_and_attr!("img", "srcset")
            || (tag, attr) == tag_and_attr!("img", "sizes")
        {
            self.load_image(node_id, ImageMutation::SourceSet);
            return;
        }
        if (tag, attr) == tag_and_attr!("img", "crossorigin") {
            self.load_image(node_id, ImageMutation::CrossOrigin);
            return;
        }
        if (tag, attr) == tag_and_attr!("img", "referrerpolicy") {
            self.load_image(node_id, ImageMutation::ReferrerPolicy);
            return;
        }

        // If node if not in the document, then don't apply any special behaviours
        // and simply set the attribute value
        if !node.flags.is_in_document() {
            return;
        }

        // Custom widgets replace the element's built-in tag-specific behaviour
        #[cfg(feature = "custom-widget")]
        if element.custom_widget_data().is_some() {
            return;
        }

        if (tag, attr) == tag_and_attr!("canvas", "src") {
            self.load_custom_paint_src(node_id);
        } else if (tag, attr) == tag_and_attr!("link", "href")
            || (tag, attr) == tag_and_attr!("link", "rel")
            || (tag, attr) == tag_and_attr!("link", "crossorigin")
        {
            self.load_linked_stylesheet(node_id);
        } else if (tag, attr) == tag_and_attr!("iframe", "src")
            || (tag, attr) == tag_and_attr!("iframe", "srcdoc")
        {
            self.load_iframe(node_id);
        }
    }

    pub fn clear_attribute(&mut self, node_id: NodeId, name: QualName) {
        let quirks = self.doc.quirks_mode();
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        if node_is_in_document {
            self.doc.snapshot_node(node_id, &name.local);

            self.restyle_for_attribute_change(node_id, &name.local);
        }

        if name.local == local_name!("id") && node_is_in_document {
            if let Some(old_id) = self.doc.nodes[node_id]
                .element_data()
                .and_then(|element| element.id.clone())
            {
                self.doc.remove_from_id_map(&old_id, node_id);
            }
        }

        let refresh_placeholder = matches!(
            name.local,
            ref local
                if local == &local_name!("value")
                    || local == &local_name!("placeholder")
                    || local == &local_name!("type")
        );
        let textarea_text_empty = refresh_placeholder
            && self.doc.nodes[node_id]
                .element_data()
                .is_some_and(|element| element.name.local == local_name!("textarea"))
            && self.doc.nodes[node_id].text_content().is_empty();

        let node = &mut self.doc.nodes[node_id];

        let Some(element) = node.element_data_mut() else {
            return;
        };

        let removed_attr = element.attrs.remove(&name);
        let had_attr = removed_attr.is_some();
        if !had_attr {
            return;
        }
        self.mutations_occurred |= node_is_in_document;

        // If element is a CustomWidget, then call attribute_changed on it
        #[cfg(feature = "custom-widget")]
        if let SpecialElementData::CustomWidget(widget_data) = &mut element.special_data {
            let old_value = removed_attr.as_ref().map(|attr| &*attr.value);
            widget_data
                .widget
                .attribute_changed(&name.local, old_value, None);
        }

        if name.local == local_name!("id") {
            element.id = None;
        }

        if name.local == local_name!("class") {
            element.classes.clear();
        }

        // As in `set_attribute`: taking one of these away can make the element
        // unfocusable again.
        if name.local == local_name!("tabindex")
            || name.local == local_name!("href")
            || name.local == local_name!("disabled")
        {
            element.flush_is_focussable();
        }

        if name.local == local_name!("href") {
            element.flush_link_state();
        }

        if name.local == local_name!("open") {
            element.flush_open_state();
        }

        // Update text input value
        if name.local == local_name!("value") {
            if !element.form_state.dirty_value() && element.name.local == local_name!("input") {
                element.store_value(String::new(), Dirtiness::Keep);
            }
            if let Some(input_data) = element.text_input_data_mut() {
                input_data.set_text(
                    &mut self.doc.font_ctx.lock().unwrap(),
                    &mut self.doc.layout_ctx,
                    "",
                );
            }
        }
        if refresh_placeholder {
            element.sync_placeholder_shown(textarea_text_empty);
        }

        let tag = &element.name.local;
        let attr = &name.local;

        if *attr == local_name!("disabled") && element.can_be_disabled() {
            node.enable();
            return;
        }

        if *attr == local_name!("style") {
            element.flush_style_attribute(&self.doc.guard, &self.doc.url.url_extra_data(), quirks);
            node.set_restyle_hint(RestyleHint::RESTYLE_STYLE_ATTRIBUTE);
        } else if (tag, attr) == tag_and_attr!("input", "checked") {
            if !element.form_state.dirty_checkedness() {
                element.store_checkedness(false, Dirtiness::Keep);
            }
        } else if (tag, attr) == tag_and_attr!("option", "selected") {
            if !element.form_state.dirty_selectedness() {
                element.store_selectedness(false, Dirtiness::Keep);
            }
        } else if (tag, attr) == tag_and_attr!("input", "type") {
            element.sync_checked_state();
        } else if (tag, attr) == tag_and_attr!("canvas", "src") {
            self.recompute_is_animating = true;
        } else if (tag, attr) == tag_and_attr!("img", "crossorigin") {
            self.load_image(node_id, ImageMutation::CrossOrigin);
        } else if (tag, attr) == tag_and_attr!("img", "referrerpolicy") {
            self.load_image(node_id, ImageMutation::ReferrerPolicy);
        } else if (tag, attr) == tag_and_attr!("link", "href") {
            self.unload_stylesheet(node_id);
        } else if (tag, attr) == tag_and_attr!("link", "crossorigin") {
            self.load_linked_stylesheet(node_id);
        } else if (tag, attr) == tag_and_attr!("iframe", "srcdoc") && node_is_in_document {
            // Fall back to loading from the `src` attribute (if any)
            self.load_iframe(node_id);
        }
    }

    /// Stores the current value and applies `dirtiness` to the dirty value
    /// flag, then refreshes `:placeholder-shown`. The content attribute stays
    /// the default value
    /// (<https://html.spec.whatwg.org/multipage/input.html#concept-fe-value>).
    pub fn set_value(&mut self, node_id: NodeId, value: String, dirtiness: Dirtiness) {
        self.doc
            .snapshot_node_and(node_id, ElementState::PLACEHOLDER_SHOWN, |node| {
                if let Some(element) = node.element_data_mut() {
                    element.store_value(value.clone(), dirtiness);
                }
            });
        // Once layout builds an editor it is the live value, so the text must
        // follow the dirty value or the selection APIs and `:placeholder-shown`
        // disagree with `value`
        // (<https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#set-the-selection-range>).
        let node = &mut self.doc.nodes[node_id];
        if let Some(element) = node.element_data_mut()
            && let Some(input_data) = element.text_input_data_mut()
        {
            input_data.set_text(
                &mut self.doc.font_ctx.lock().unwrap(),
                &mut self.doc.layout_ctx,
                &value,
            );
        }
    }

    /// Sets checkedness without writing the `checked` attribute, and applies
    /// `dirtiness` to the dirty checkedness flag
    /// (<https://html.spec.whatwg.org/multipage/input.html#dom-input-checked>).
    pub fn set_checkedness(&mut self, node_id: NodeId, checked: bool, dirtiness: Dirtiness) {
        self.doc
            .snapshot_node_and(node_id, ElementState::CHECKED, |node| {
                if let Some(element) = node.element_data_mut() {
                    element.store_checkedness(checked, dirtiness);
                }
            });
    }

    /// Sets selectedness without writing the `selected` attribute, and
    /// applies `dirtiness` to the option dirtiness flag
    /// (<https://html.spec.whatwg.org/multipage/form-elements.html#dom-option-selected>).
    pub fn set_selectedness(&mut self, node_id: NodeId, selected: bool, dirtiness: Dirtiness) {
        self.doc
            .snapshot_node_and(node_id, ElementState::CHECKED, |node| {
                if let Some(element) = node.element_data_mut() {
                    element.store_selectedness(selected, dirtiness);
                }
            });
    }

    /// Sets the indeterminate flag
    /// (<https://html.spec.whatwg.org/multipage/input.html#dom-input-indeterminate>).
    pub fn set_indeterminate(&mut self, node_id: NodeId, indeterminate: bool) {
        self.doc
            .snapshot_node_and(node_id, ElementState::INDETERMINATE, |node| {
                if let Some(element) = node.element_data_mut() {
                    element.store_indeterminate(indeterminate);
                }
            });
    }

    /// Clears dirty form state so attributes and descendant text apply again.
    pub fn reset_control(&mut self, node_id: NodeId) {
        let text_empty = self.doc.nodes[node_id].text_content().is_empty();
        self.doc.snapshot_node_and(
            node_id,
            ElementState::CHECKED | ElementState::INDETERMINATE | ElementState::PLACEHOLDER_SHOWN,
            |node| {
                if let Some(element) = node.element_data_mut() {
                    element.reset_control_state(text_empty);
                }
            },
        );
    }

    pub fn set_style_property(&mut self, node_id: NodeId, name: &str, value: &str) {
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        self.doc.set_style_property(node_id, name, value);
        self.mutations_occurred |= node_is_in_document;
    }

    pub fn remove_style_property(&mut self, node_id: NodeId, name: &str) {
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        self.doc.remove_style_property(node_id, name);
        self.mutations_occurred |= node_is_in_document;
    }

    pub fn set_sub_document(&mut self, node_id: NodeId, sub_document: Box<dyn Document>) {
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        self.doc.set_sub_document(node_id, sub_document);
        self.mutations_occurred |= node_is_in_document;
    }

    pub fn remove_sub_document(&mut self, node_id: NodeId) {
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        self.doc.remove_sub_document(node_id);
        self.mutations_occurred |= node_is_in_document;
    }

    #[cfg(feature = "custom-widget")]
    pub fn set_custom_widget(&mut self, node_id: NodeId, widget: Box<dyn crate::Widget>) {
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        self.doc.set_custom_widget(node_id, widget);
        self.mutations_occurred |= node_is_in_document;
    }

    #[cfg(feature = "custom-widget")]
    pub fn remove_custom_widget(&mut self, node_id: NodeId) {
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        self.doc.remove_custom_widget(node_id);
        self.mutations_occurred |= node_is_in_document;
    }

    /// Remove the node from it's parent but don't drop it
    pub fn remove_node(&mut self, node_id: NodeId) {
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        // Process the subtree *before* severing the parent link so that
        // interaction state referencing removed nodes can retarget to the
        // nearest surviving ancestor.
        self.process_removed_subtree(node_id);

        let node = &mut self.doc.nodes[node_id];

        // Update child_idx values
        if let Some(parent_id) = node.parent.take() {
            self.mutations_occurred |= node_is_in_document;
            let parent = &mut self.doc.nodes[parent_id];
            parent.insert_damage(ALL_DAMAGE);
            let index = parent.index_of_child(node_id).unwrap_or(0);
            parent.children.retain(|id| *id != node_id);
            if node_is_in_document {
                self.restyle_for_child_list_change(parent_id, index..index);
            }
            self.maybe_record_node(parent_id);
            self.note_placeholder_shown(parent_id);
        }
    }

    pub fn remove_and_drop_node(&mut self, node_id: NodeId) -> Option<Node> {
        self.remove_and_drop_node_with(node_id, &mut |_| {})
    }

    /// Like [`Self::remove_and_drop_node`], but calls `on_drop` with the id of
    /// every dropped node (the node itself and all of its descendants).
    pub fn remove_and_drop_node_with(
        &mut self,
        node_id: NodeId,
        on_drop: &mut dyn FnMut(NodeId),
    ) -> Option<Node> {
        let node_is_in_document = self.doc.nodes[node_id].flags.is_in_document();
        self.process_removed_subtree(node_id);

        let node = self.doc.drop_node_ignoring_parent_with(node_id, on_drop);
        self.mutations_occurred |= node_is_in_document;

        // Update child_idx values
        if let Some(parent_id) = node.as_ref().and_then(|node| node.parent) {
            let parent = &mut self.doc.nodes[parent_id];
            parent.insert_damage(ALL_DAMAGE);
            let parent_is_in_doc = parent.flags.is_in_document();
            let index = parent.index_of_child(node_id).unwrap_or(0);
            parent.children.retain(|id| *id != node_id);

            if parent_is_in_doc {
                self.restyle_for_child_list_change(parent_id, index..index);
            }

            self.maybe_record_node(parent_id);
            self.note_placeholder_shown(parent_id);
        }

        node
    }

    pub fn remove_and_drop_all_children(&mut self, node_id: NodeId) {
        let parent = &mut self.doc.nodes[node_id];
        let parent_is_in_doc = parent.flags.is_in_document();
        let children = mem::take(&mut parent.children);
        self.mutations_occurred |= parent_is_in_doc && !children.is_empty();

        if parent_is_in_doc {
            self.restyle_for_child_list_change(node_id, 0..0);
        }

        for child_id in children {
            self.process_removed_subtree(child_id);
            let _ = self.doc.drop_node_ignoring_parent(child_id);
        }
        self.maybe_record_node(node_id);
    }

    /// Restyle an in-document element whose attribute is about to change. Only the element
    /// is hinted: its snapshot lets Stylo's invalidator find the siblings and descendants
    /// whose selectors depend on the attribute. `style`, `class` and `id` reach layout only
    /// through the computed style (whose diff yields the damage); other attributes may not.
    fn restyle_for_attribute_change(&mut self, node_id: NodeId, attr: &LocalName) {
        let node = &mut self.doc.nodes[node_id];
        let is_style = *attr == local_name!("style");
        // Attributes that box construction or layout read directly. Any other attribute only
        // reaches layout through the cascade, so its damage comes from the computed-style diff.
        // Inline SVG is rendered from its serialized source, so every SVG attribute counts.
        let is_svg = node
            .element_data()
            .is_some_and(|element| element.name.ns == markup5ever::ns!(svg));
        let read_by_layout = is_svg
            || matches!(
                *attr,
                local_name!("type")
                    | local_name!("value")
                    | local_name!("checked")
                    | local_name!("width")
                    | local_name!("height")
                    | local_name!("colspan")
                    | local_name!("rowspan")
                    | local_name!("rows")
                    | local_name!("cols")
                    | local_name!("start")
                    | local_name!("reversed")
                    | local_name!("src")
                    | local_name!("srcdoc")
                    | local_name!("span")
            );
        if let Some(mut data) = node.try_stylo_element_data_mut().and_then(|s| s.get_mut()) {
            data.hint |= if is_style {
                RestyleHint::RESTYLE_STYLE_ATTRIBUTE
            } else {
                RestyleHint::RESTYLE_SELF
            };
            if read_by_layout {
                data.damage.insert(ALL_DAMAGE);
            }
        }
        if read_by_layout {
            node.mark_damaged();
        }
        // Mark ancestors dirty so the style traversal visits this subtree.
        node.mark_ancestors_dirty();
    }

    /// Restyle an in-document element whose child list changed, `changed` being the position
    /// of the inserted children (empty, at the former position of a removed child). New
    /// children are styled by the traversal regardless; the element and its other children
    /// only need restyling where its selector flags record a selector that depends on them.
    fn restyle_for_child_list_change(&mut self, parent_id: NodeId, changed: Range<usize>) {
        let parent = &self.doc.nodes[parent_id];
        let flags = match &parent.data {
            NodeData::Element(_) | NodeData::AnonymousBlock(_) | NodeData::Document(_) => {
                parent.selector_flags().get()
            }
            // A shadow root or a fragment has no selector flags: no selector
            // can make it a subject, so only the dirty marking below matters.
            _ => ElementSelectorFlags::empty(),
        };
        let mut hint = RestyleHint::empty();
        if flags.intersects(
            ElementSelectorFlags::HAS_EMPTY_SELECTOR
                | ElementSelectorFlags::ANCHORS_RELATIVE_SELECTOR
                | ElementSelectorFlags::ANCHORS_RELATIVE_SELECTOR_NON_SUBJECT,
        ) {
            hint |= RestyleHint::RESTYLE_SELF;
        }
        // A change with no earlier element sibling restyles all children with one hint rather
        // than hinting each later sibling, so inserting or removing at the front stays linear.
        let front_change = flags.contains(ElementSelectorFlags::HAS_SLOW_SELECTOR_LATER_SIBLINGS)
            && !parent.children[..changed.start]
                .iter()
                .any(|id| self.doc.nodes[*id].is_element());
        if front_change
            || flags.intersects(
                ElementSelectorFlags::HAS_SLOW_SELECTOR
                    | ElementSelectorFlags::HAS_SLOW_SELECTOR_NTH_OF
                    | ElementSelectorFlags::MAY_HAVE_TREE_COUNTING_FUNCTION,
            )
        {
            hint |= RestyleHint::RESTYLE_DESCENDANTS;
        } else {
            let children = mem::take(&mut self.doc.nodes[parent_id].children);
            let later = if flags.contains(ElementSelectorFlags::HAS_SLOW_SELECTOR_LATER_SIBLINGS) {
                changed.end..children.len()
            } else {
                0..0
            };
            // The element children that were first/last before an insertion, or are after a
            // removal; text nodes don't count for `:first-child`/`:last-child`.
            let mut edge = [None, None];
            if flags.contains(ElementSelectorFlags::HAS_EDGE_CHILD_SELECTOR) {
                let is_element = |id: &NodeId| self.doc.nodes[*id].is_element();
                if !children[..changed.start].iter().any(is_element) {
                    edge[0] = children[changed.end..].iter().copied().find(is_element);
                }
                if !children[changed.end..].iter().any(is_element) {
                    edge[1] = children[..changed.start]
                        .iter()
                        .rev()
                        .copied()
                        .find(is_element);
                }
            }
            for id in children[later].iter().chain(edge.iter().flatten()) {
                self.doc.nodes[*id].set_restyle_hint(RestyleHint::restyle_subtree());
            }
            self.doc.nodes[parent_id].children = children;
        }

        let parent = &mut self.doc.nodes[parent_id];
        if !hint.is_empty() {
            if let Some(mut data) = parent
                .try_stylo_element_data_mut()
                .and_then(|s| s.get_mut())
            {
                data.hint |= hint;
            }
        }
        // The traversal only descends into an element flagged as having dirty descendants.
        parent.set_dirty_descendants();
        parent.mark_ancestors_dirty();
    }

    // Tree mutation methods
    pub fn remove_node_if_unparented(&mut self, node_id: NodeId) {
        self.remove_node_if_unparented_with(node_id, &mut |_| {});
    }

    /// Like [`Self::remove_node_if_unparented`], but calls `on_drop` with the id of
    /// every dropped node (the node itself and all of its descendants).
    pub fn remove_node_if_unparented_with(
        &mut self,
        node_id: NodeId,
        on_drop: &mut dyn FnMut(NodeId),
    ) {
        if let Some(node) = self.doc.get_node(node_id) {
            if node.parent.is_none() {
                self.remove_and_drop_node_with(node_id, on_drop);
            }
        }
    }

    /// Remove all of the children from old_parent_id and append them to new_parent_id
    pub fn append_children(&mut self, parent_id: NodeId, child_ids: &[NodeId]) {
        self.add_children_to_parent(parent_id, child_ids, &|parent, child_ids| {
            let index = parent.children.len();
            parent.children.extend_from_slice(child_ids);
            index
        });
    }

    pub fn insert_nodes_before(&mut self, anchor_node_id: NodeId, new_node_ids: &[NodeId]) {
        let parent_id = self.doc.nodes[anchor_node_id].parent.unwrap();
        self.add_children_to_parent(parent_id, new_node_ids, &|parent, child_ids| {
            let node_child_idx = parent.index_of_child(anchor_node_id).unwrap();
            parent
                .children
                .splice(node_child_idx..node_child_idx, child_ids.iter().copied());
            node_child_idx
        });
    }

    fn add_children_to_parent(
        &mut self,
        parent_id: NodeId,
        child_ids: &[NodeId],
        insert_children_fn: &dyn Fn(&mut Node, &[NodeId]) -> usize,
    ) {
        let new_parent_is_in_document = self.doc.nodes[parent_id].flags.is_in_document();
        self.mutations_occurred |= new_parent_is_in_document && !child_ids.is_empty();

        // If one of the moved subtrees contains the focused node, the ancestors it is
        // detached from stop matching :focus-within. Record them while the parent links
        // are still intact; the state is transferred after the children are reattached.
        let mut focus_within_old_chain: Option<Vec<NodeId>> = None;

        // Detach the children from their old parents *before* inserting them into
        // the new parent (matching DOM `insertBefore` semantics). If a child is
        // being moved within the same parent then detaching it after insertion
        // would remove both the old and the newly-inserted entries from the
        // parent's child list, and anchor indices would be computed against a
        // child list that still contains the moved nodes.
        for child_id in child_ids.iter().copied() {
            let child = &self.doc.nodes[child_id];
            if child.contains_focus() {
                focus_within_old_chain = Some(self.doc.maybe_node_element_ancestors(child.parent));
            }

            let child = &mut self.doc.nodes[child_id];
            let child_was_in_doc = child.flags.is_in_document();
            self.mutations_occurred |= child_was_in_doc;
            let Some(old_parent_id) = child.parent.take() else {
                continue;
            };

            let old_parent = &mut self.doc.nodes[old_parent_id];
            old_parent.insert_damage(ALL_DAMAGE);
            let index = old_parent.index_of_child(child_id).unwrap_or(0);
            old_parent.children.retain(|id| *id != child_id);

            if child_was_in_doc {
                self.restyle_for_child_list_change(old_parent_id, index..index);
            }

            self.maybe_record_node(old_parent_id);
            self.note_placeholder_shown(old_parent_id);
        }

        let new_parent = &mut self.doc.nodes[parent_id];
        new_parent.insert_damage(ALL_DAMAGE);
        let index = insert_children_fn(new_parent, child_ids);

        if new_parent_is_in_document {
            self.restyle_for_child_list_change(parent_id, index..index + child_ids.len());
        }

        for child_id in child_ids.iter().copied() {
            let child = &mut self.doc.nodes[child_id];
            let child_was_in_doc = child.flags.is_in_document();
            child.parent = Some(parent_id);
            // A moved or re-attached subtree keeps its styles, but everything above it changed.
            if new_parent_is_in_document && child.primary_styles().is_some() {
                child.set_restyle_hint(RestyleHint::restyle_subtree());
            }

            if new_parent_is_in_document && !child_was_in_doc {
                self.process_added_subtree(child_id);
            } else if !new_parent_is_in_document && child_was_in_doc {
                self.process_removed_subtree(child_id);
            }
        }

        // Transfer :focus-within from the old ancestor chain to the new one (the
        // shared prefix stays). If the subtree left the document, the focus was
        // cleared with it and only the old chain needs the state removed.
        if let Some(old_chain) = focus_within_old_chain {
            let new_chain = match self.doc.focus_node_id {
                Some(_) => self.doc.node_element_ancestors(parent_id),
                None => Vec::new(),
            };
            let same_count = old_chain
                .iter()
                .zip(&new_chain)
                .take_while(|(o, n)| o == n)
                .count();
            for &id in old_chain.iter().skip(same_count) {
                self.doc
                    .snapshot_node_and(id, ElementState::FOCUS_WITHIN, |node| {
                        node.remove_focus_within()
                    });
            }
            for &id in new_chain.iter().skip(same_count) {
                self.doc
                    .snapshot_node_and(id, ElementState::FOCUS_WITHIN, |node| {
                        node.add_focus_within()
                    });
            }
        }

        self.maybe_record_node(parent_id);
        self.note_placeholder_shown(parent_id);
    }

    /// Snapshot, then recompute `:placeholder-shown` for a control whose text
    /// content just changed. Attribute edits snapshot in `set_attribute` /
    /// `clear_attribute` before they sync the bit.
    fn note_placeholder_shown(&mut self, node_id: NodeId) {
        self.doc
            .snapshot_node_and(node_id, ElementState::PLACEHOLDER_SHOWN, |node| {
                node.flush_placeholder_shown();
            });
        self.doc.nodes[node_id].mark_ancestors_dirty();
    }

    // Tree mutation methods (that defer to other methods)
    pub fn insert_nodes_after(&mut self, anchor_node_id: NodeId, new_node_ids: &[NodeId]) {
        match self.next_sibling_id(anchor_node_id) {
            Some(id) => self.insert_nodes_before(id, new_node_ids),
            None => {
                let parent_id = self.parent_id(anchor_node_id).unwrap();
                self.append_children(parent_id, new_node_ids)
            }
        }
    }

    pub fn reparent_children(&mut self, old_parent_id: NodeId, new_parent_id: NodeId) {
        let child_ids = std::mem::take(&mut self.doc.nodes[old_parent_id].children);
        self.maybe_record_node(old_parent_id);
        self.append_children(new_parent_id, &child_ids);
    }

    pub fn replace_node_with(&mut self, anchor_node_id: NodeId, new_node_ids: &[NodeId]) {
        self.insert_nodes_before(anchor_node_id, new_node_ids);
        self.remove_node(anchor_node_id);
    }

    // === ParentNode / ChildNode mixin mutation methods ===
    //
    // These implement the DOM spec's mutation semantics for the ParentNode and
    // ChildNode mixins (`append`/`prepend`/`replaceChildren` and
    // `before`/`after`/`replaceWith`). Callers pass lists of node ids with
    // string arguments already converted to (detached) text nodes and
    // DocumentFragment arguments already expanded into their children (per the
    // DOM spec's "insert a node" steps).

    /// Detach (rather than drop) any already-parented nodes, so that references
    /// to them (and their descendants) remain valid
    fn detach_all(&mut self, node_ids: &[NodeId]) {
        for node_id in node_ids {
            if self.node_has_parent(*node_id) {
                self.remove_node(*node_id);
            }
        }
    }

    /// The nearest sibling of `anchor_id` (in the direction given by `offset`:
    /// +1 = following, -1 = preceding) which is not in `excluded` — the spec's
    /// "viable next/previous sibling" for ChildNode mutation methods
    fn viable_sibling(
        &self,
        anchor_id: NodeId,
        offset: isize,
        excluded: &[NodeId],
    ) -> Option<NodeId> {
        let node = self.doc.get_node(anchor_id)?;
        let parent = self.doc.get_node(node.parent?)?;
        let mut index = parent.index_of_child(anchor_id)?;
        loop {
            index = index.checked_add_signed(offset)?;
            let sibling_id = *parent.children.get(index)?;
            if !excluded.contains(&sibling_id) {
                return Some(sibling_id);
            }
        }
    }

    /// ParentNode's `prepend()`: insert the nodes at the start of `parent_id`'s
    /// children (ParentNode's `append()` is [`append_children`](Self::append_children))
    pub fn prepend_nodes(&mut self, parent_id: NodeId, node_ids: &[NodeId]) {
        self.detach_all(node_ids);
        match self.child_ids(parent_id).first().copied() {
            Some(first_child_id) => self.insert_nodes_before(first_child_id, node_ids),
            None => self.append_children(parent_id, node_ids),
        }
    }

    /// ParentNode's `replaceChildren()`: replace all of `parent_id`'s children
    /// with the given nodes. The existing children are detached rather than
    /// dropped, so that references to them remain valid.
    pub fn replace_children(&mut self, parent_id: NodeId, node_ids: &[NodeId]) {
        self.detach_all(node_ids);
        for child_id in self.child_ids(parent_id) {
            self.remove_node(child_id);
        }
        self.append_children(parent_id, node_ids);
    }

    /// ChildNode's `before()`: insert the nodes before `anchor_id`. The
    /// insertion point is after the nearest preceding sibling which isn't
    /// itself being inserted (or at the start of the parent if there is none).
    /// Does nothing if the anchor has no parent.
    pub fn before_node(&mut self, anchor_id: NodeId, node_ids: &[NodeId]) {
        let Some(parent_id) = self.parent_id(anchor_id) else {
            return;
        };
        let viable_prev = self.viable_sibling(anchor_id, -1, node_ids);
        self.detach_all(node_ids);
        match viable_prev {
            Some(prev_id) => self.insert_nodes_after(prev_id, node_ids),
            None => match self.child_ids(parent_id).first().copied() {
                Some(first_child_id) => self.insert_nodes_before(first_child_id, node_ids),
                None => self.append_children(parent_id, node_ids),
            },
        }
    }

    /// ChildNode's `after()`: insert the nodes after `anchor_id`. The insertion
    /// point is before the nearest following sibling which isn't itself being
    /// inserted (or at the end of the parent if there is none). Does nothing if
    /// the anchor has no parent.
    pub fn after_node(&mut self, anchor_id: NodeId, node_ids: &[NodeId]) {
        let Some(parent_id) = self.parent_id(anchor_id) else {
            return;
        };
        let viable_next = self.viable_sibling(anchor_id, 1, node_ids);
        self.detach_all(node_ids);
        match viable_next {
            Some(next_id) => self.insert_nodes_before(next_id, node_ids),
            None => self.append_children(parent_id, node_ids),
        }
    }

    /// ChildNode's `replaceWith()`: replace `anchor_id` with the nodes in its
    /// parent's child list. If the anchor is itself one of the inserted nodes,
    /// they are inserted at its old position (the spec's "viable next sibling"
    /// handling). The anchor is detached rather than dropped, so references to
    /// it remain valid. Does nothing if the anchor has no parent.
    pub fn replace_with_nodes(&mut self, anchor_id: NodeId, node_ids: &[NodeId]) {
        let Some(parent_id) = self.parent_id(anchor_id) else {
            return;
        };
        let viable_next = self.viable_sibling(anchor_id, 1, node_ids);
        self.detach_all(node_ids);
        if self.node_has_parent(anchor_id) {
            self.replace_node_with(anchor_id, node_ids);
        } else {
            match viable_next {
                Some(next_id) => self.insert_nodes_before(next_id, node_ids),
                None => self.append_children(parent_id, node_ids),
            }
        }
    }
}

/// Why an image's `load_image` ran. Setting `src` always restarts; inserting
/// the element and the other two attributes only when their request state
/// changes, so a DOM move keeps the request it already has
/// (<https://html.spec.whatwg.org/multipage/images.html#reacting-to-dom-mutations>).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageMutation {
    Src,
    Insert,
    /// A `srcset` or `sizes` write: a relevant mutation whose request only
    /// restarts when the selected source changed
    /// (<https://html.spec.whatwg.org/multipage/images.html#reacting-to-dom-mutations>).
    SourceSet,
    CrossOrigin,
    ReferrerPolicy,
}

impl<'doc> DocumentMutator<'doc> {
    pub fn flush(&mut self) {
        if self.recompute_is_animating {
            self.doc.has_canvas = self.doc.compute_has_canvas();
        }

        if let Some(id) = self.title_node {
            let title = self.doc.nodes[id].text_content();
            self.doc.shell_provider.set_window_title(title);
        }

        // Add/Update inline stylesheets (<style> elements)
        for id in self.style_nodes.drain() {
            self.doc.process_style_element(id);
        }

        for id in self.form_nodes.drain() {
            self.doc.reset_form_owner(id);
        }

        #[cfg(feature = "autofocus")]
        if let Some(node_id) = self.node_to_autofocus.take() {
            if self.doc.get_node(node_id).is_some() {
                self.doc.set_focus_to(node_id);
            }
        }
    }

    pub fn set_inner_html(&mut self, node_id: NodeId, html: &str) {
        self.remove_and_drop_all_children(node_id);
        self.doc
            .html_parser_provider
            .clone()
            .parse_inner_html(self, node_id, html);
    }

    fn flush_eager_ops(&mut self) {
        let mut ops = mem::take(&mut self.eager_op_queue);
        for op in ops.drain(0..) {
            match op {
                SpecialOp::LoadImage(node_id) => {
                    self.load_image(node_id, ImageMutation::Insert);
                }
                SpecialOp::LoadIframe(node_id) => self.load_iframe(node_id),
                SpecialOp::LoadStylesheet(node_id) => self.load_linked_stylesheet(node_id),
                SpecialOp::LoadStylesheetPi(node_id) => self.load_pi_stylesheet(node_id),
                SpecialOp::UnloadStylesheet(node_id) => self.unload_stylesheet(node_id),
                SpecialOp::UnloadStylesheetPi(node_id) => self.unload_pi_stylesheet(node_id),
                SpecialOp::LoadCustomPaintSource(node_id) => self.load_custom_paint_src(node_id),
                SpecialOp::ProcessButtonInput(node_id) => self.process_button_input(node_id),
                SpecialOp::UnloadSubDocument(node_id) => self.remove_sub_document(node_id),
                #[cfg(feature = "custom-widget")]
                SpecialOp::UnloadCustomWidget(node_id) => self.remove_custom_widget(node_id),
            }
        }

        // Queue is empty, but put Vec back anyway so allocation can be reused.
        self.eager_op_queue = ops;
    }

    fn process_added_subtree(&mut self, node_id: NodeId) {
        self.doc.iter_subtree_mut(node_id, |node_id, doc| {
            let node = &mut doc.nodes[node_id];
            node.flags.set(NodeFlags::IS_IN_DOCUMENT, true);
            node.insert_damage(ALL_DAMAGE);

            // If the node has an "id" attribute, store it in the ID map.
            if let Some(id_attr) = node.attr(local_name!("id")).map(ToString::to_string) {
                doc.add_to_id_map(&id_attr, node_id);
            }

            let in_shadow = node_inside_shadow(doc, node_id);
            let node = &mut doc.nodes[node_id];
            // An `xml-stylesheet` processing instruction loads a style sheet
            // like `<link rel=stylesheet>` does; HTML keeps the instruction for
            // compatibility with XML-authored sheets
            // (<https://www.w3.org/TR/xml-stylesheet/>).
            if let NodeData::ProcessingInstruction { target, .. } = &node.data
                && target.eq_ignore_ascii_case("xml-stylesheet")
            {
                self.eager_op_queue
                    .push(SpecialOp::LoadStylesheetPi(node_id));
                return;
            }
            let NodeData::Element(ref mut element) = node.data else {
                return;
            };

            // Custom widgets replace the element's built-in tag-specific behaviour
            #[cfg(feature = "custom-widget")]
            let has_custom_widget = element.custom_widget_data().is_some();
            #[cfg(not(feature = "custom-widget"))]
            let has_custom_widget = false;

            // Custom post-processing by element tag name
            let tag = element.name.local.as_ref();
            match tag {
                "title" => self.title_node = Some(node_id),
                "style" if !in_shadow => {
                    self.style_nodes.insert(node_id);
                }
                _ if has_custom_widget => {}
                "link" if !in_shadow => {
                    self.eager_op_queue.push(SpecialOp::LoadStylesheet(node_id))
                }
                "img" => self.eager_op_queue.push(SpecialOp::LoadImage(node_id)),
                "iframe" => self.eager_op_queue.push(SpecialOp::LoadIframe(node_id)),
                "canvas" => self
                    .eager_op_queue
                    .push(SpecialOp::LoadCustomPaintSource(node_id)),
                "button" | "fieldset" | "input" | "select" | "textarea" | "object" | "output" => {
                    self.eager_op_queue
                        .push(SpecialOp::ProcessButtonInput(node_id));
                    self.form_nodes.insert(node_id);
                }
                _ => {}
            }

            #[cfg(feature = "autofocus")]
            if node.is_focussable() {
                if let NodeData::Element(ref element) = node.data {
                    // `autofocus` is a boolean attribute, so presence alone enables it
                    // (`<input autofocus>`). "false" is still ignored, because Dioxus
                    // writes `autofocus: false` as the string "false".
                    if let Some(value) = element.attr(local_name!("autofocus")) {
                        if value != "false" {
                            self.node_to_autofocus = Some(node_id);
                        }
                    }
                }
            }
        });

        let shadows = self.shadow_roots_under(node_id);
        for shadow in shadows {
            self.process_added_subtree(shadow);
        }

        self.flush_eager_ops();
    }

    fn process_removed_subtree(&mut self, node_id: NodeId) {
        self.doc.iter_subtree_mut(node_id, |node_id, doc| {
            doc.nodes[node_id]
                .flags
                .set(NodeFlags::IS_IN_DOCUMENT, false);

            // Clear any interaction state that references this node, running
            // the usual teardown steps (unhover/unactive the surviving
            // ancestor chain, IME disable on blur of a focused input).
            doc.clear_interaction_state_for_removed_node(node_id);

            let node = &mut doc.nodes[node_id];

            // Clear the text selection if one of its endpoints references this node.
            // This prevents stale selection endpoint references.
            if doc.text_selection.anchor.node_or_parent == Some(node_id)
                || doc.text_selection.focus.node_or_parent == Some(node_id)
            {
                doc.text_selection.clear();
            }

            // Remove any snapshot for this node to prevent stale snapshot references
            // during style invalidation.
            if node.has_snapshot() {
                let opaque_id = style::dom::TNode::opaque(&&*node);
                doc.snapshots.remove(&opaque_id);
                node.set_has_snapshot(false);
            }

            // If the node has an "id" attribute remove it from the ID map.
            if let Some(id_attr) = node.attr(local_name!("id")).map(ToString::to_string) {
                doc.remove_from_id_map(&id_attr, node_id);
            }

            let node = &mut doc.nodes[node_id];
            let NodeData::Element(ref mut element) = node.data else {
                // A processing instruction can own a style sheet; it has no
                // element storage, so the unload path differs.
                if matches!(&node.data, NodeData::ProcessingInstruction { .. })
                    && doc.nodes_to_stylesheet.contains_key(&node_id)
                {
                    self.eager_op_queue
                        .push(SpecialOp::UnloadStylesheetPi(node_id));
                }
                return;
            };

            match &element.special_data {
                SpecialElementData::SubDocument(_) => {
                    self.eager_op_queue
                        .push(SpecialOp::UnloadSubDocument(node_id));
                }
                #[cfg(feature = "custom-widget")]
                SpecialElementData::CustomWidget(_) => {
                    self.eager_op_queue
                        .push(SpecialOp::UnloadCustomWidget(node_id));
                }
                SpecialElementData::Stylesheet(_) => self
                    .eager_op_queue
                    .push(SpecialOp::UnloadStylesheet(node_id)),
                SpecialElementData::Image(_) => {}
                SpecialElementData::Canvas(_) => {
                    self.recompute_is_animating = true;
                }
                SpecialElementData::TableRoot(_) => {}
                SpecialElementData::TextInput(_) => {}
                SpecialElementData::CheckboxInput => {}
                #[cfg(feature = "file-input")]
                SpecialElementData::FileInput(_) => {}
                SpecialElementData::None => {}
            }
        });

        let shadows = self.shadow_roots_under(node_id);
        for shadow in shadows {
            self.process_removed_subtree(shadow);
        }

        self.flush_eager_ops();
    }

    /// Shadow roots hosted by elements in the light subtree of `node_id`.
    /// The walk does not enter those shadow trees.
    fn shadow_roots_under(&self, node_id: NodeId) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut stack = vec![node_id];
        while let Some(id) = stack.pop() {
            if let Some(shadow) = self.doc.nodes[id]
                .element_data()
                .and_then(|element| element.shadow_root)
            {
                found.push(shadow);
            }
            stack.extend(self.doc.nodes[id].children.iter().copied());
        }
        found
    }

    /// Whether `node_id` is inside a shadow tree (not the shadow root itself).
    fn inside_shadow(&self, node_id: NodeId) -> bool {
        let mut current = self.doc.nodes[node_id].parent;
        while let Some(id) = current {
            if matches!(self.doc.nodes[id].data, NodeData::ShadowRoot { .. }) {
                return true;
            }
            current = self.doc.nodes[id].parent;
        }
        false
    }

    /// Attach a shadow root to `host`. The root is not a child of the host
    /// (<https://dom.spec.whatwg.org/#concept-attach-a-shadow-root>).
    pub fn attach_shadow_root(&mut self, host: NodeId, closed: bool) -> NodeId {
        let id = self.doc.create_node(NodeData::ShadowRoot { host, closed });
        if let Some(element) = self.doc.nodes[host].element_data_mut() {
            element.shadow_root = Some(id);
        }
        if self.doc.nodes[host].flags.is_in_document() {
            self.doc.nodes[id]
                .flags
                .set(NodeFlags::IS_IN_DOCUMENT, true);
        }
        id
    }

    /// Reparent `node_id` under `parent_id` before `before` without the
    /// insertion or removing steps
    /// (<https://dom.spec.whatwg.org/#concept-node-move>).
    pub fn move_node(&mut self, node_id: NodeId, parent_id: NodeId, before: Option<NodeId>) {
        let old_parent = self.doc.nodes[node_id].parent.take();
        if let Some(old_parent) = old_parent {
            self.doc.nodes[old_parent]
                .children
                .retain(|id| *id != node_id);
            self.doc.nodes[old_parent].insert_damage(ALL_DAMAGE);
        }
        {
            let parent = &mut self.doc.nodes[parent_id];
            parent.insert_damage(ALL_DAMAGE);
            match before {
                Some(before) => {
                    let index = parent
                        .children
                        .iter()
                        .position(|id| *id == before)
                        .expect("move reference is a child of the new parent");
                    parent.children.insert(index, node_id);
                }
                None => parent.children.push(node_id),
            }
        }
        self.doc.nodes[node_id].parent = Some(parent_id);
        self.doc.nodes[node_id].insert_damage(ALL_DAMAGE);
        self.doc.nodes[node_id].mark_ancestors_dirty();
        self.doc.reorder_stylesheets_in_subtree(node_id);
        self.mutations_occurred = true;
    }

    fn maybe_record_node(&mut self, node_id: impl Into<Option<NodeId>>) {
        let Some(node_id) = node_id.into() else {
            return;
        };

        let Some(tag_name) = self.doc.nodes[node_id]
            .data
            .downcast_element()
            .map(|elem| &elem.name.local)
        else {
            return;
        };

        match tag_name.as_ref() {
            "title" => self.title_node = Some(node_id),
            // A `<style>` element applies only while it is in the document; a
            // text change on a detached one must not attach a sheet
            // (<https://html.spec.whatwg.org/multipage/semantics.html#the-style-element>).
            "style"
                if !self.inside_shadow(node_id)
                    && self.doc.nodes[node_id].flags.is_in_document() =>
            {
                self.style_nodes.insert(node_id);
            }
            _ => {}
        }
    }

    fn load_linked_stylesheet(&mut self, target_id: NodeId) {
        let node = &self.doc.nodes[target_id];

        let mut is_in_head = false;
        let mut parent_id = node.parent;
        while let Some(id) = parent_id
            && !is_in_head
        {
            let parent = &self.doc.nodes[id];
            is_in_head |= parent.data.is_element_with_tag_name(&local_name!("head"));
            parent_id = parent.parent;
        }

        let rel_attr = node.attr(local_name!("rel"));
        let href_attr = node.attr(local_name!("href"));

        let (Some(rels), Some(href)) = (rel_attr, href_attr) else {
            return;
        };
        if !rels.split_ascii_whitespace().any(|rel| rel == "stylesheet") {
            return;
        }

        let Some(url) = self.doc.resolve_url(href) else {
            // Unresolvable stylesheet URL: strictly better than the old panic,
            // but observable (no fetch, and no error event today). Matches the
            // resolve_url contract; error-event semantics per
            // <https://html.spec.whatwg.org/multipage/semantics.html#the-link-element>
            // are a known gap.
            return;
        };
        self.fetch_stylesheet(target_id, url, is_in_head);
    }
    /// Fetches the sheet at `url` for its owning node and files the response
    /// under that node. A sheet in `head` delays the document's load event.
    /// Returns the request id, so a caller that can be asked for a different
    /// sheet later can drop a response for the old one.
    fn fetch_stylesheet(&mut self, owner: NodeId, url: url::Url, critical: bool) -> usize {
        self.doc.fetch_stylesheet(owner, url, critical)
    }

    /// Loads the sheet an `xml-stylesheet` processing instruction names
    /// (<https://www.w3.org/TR/xml-stylesheet/>). The pseudo-attribute values
    /// are XML, so `&amp;` in an `href` is the character, not five bytes of
    /// URL.
    fn load_pi_stylesheet(&mut self, node_id: NodeId) {
        let Some((target, contents)) = self.doc.nodes.get(node_id).and_then(|node| {
            let NodeData::ProcessingInstruction {
                target, contents, ..
            } = &node.data
            else {
                return None;
            };
            Some((target.clone(), contents.clone()))
        }) else {
            return;
        };
        if !target.eq_ignore_ascii_case("xml-stylesheet") {
            return;
        }
        // The loader parses the data itself, as Blink's data-change path does:
        // the DOM's attribute map may hold a name the XML grammar rejects, and
        // a data that does not parse must load nothing
        // (<https://www.w3.org/TR/xml-stylesheet/#dt-parsing>).
        let Some(attributes) = crate::pseudo_attributes::parse(&contents) else {
            return;
        };
        let value = |name: &str| {
            attributes
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        let Some(href) = value("href") else {
            return;
        };
        if let Some(typ) = value("type")
            && !typ.trim().is_empty()
            && !typ.trim().eq_ignore_ascii_case("text/css")
        {
            return;
        }
        let Some(url) = self.doc.resolve_url(href) else {
            return;
        };
        let request_id = self.fetch_stylesheet(node_id, url, false);
        self.doc.pi_stylesheet_requests.insert(node_id, request_id);
    }

    fn unload_stylesheet(&mut self, node_id: NodeId) {
        let node = &mut self.doc.nodes[node_id];
        let Some(element) = node.element_data_mut() else {
            unreachable!();
        };
        let SpecialElementData::Stylesheet(stylesheet) = element.special_data.take() else {
            unreachable!();
        };

        let guard = self.doc.guard.read();
        self.doc.stylist.remove_stylesheet(stylesheet, &guard);
        self.doc
            .stylist
            .force_stylesheet_origins_dirty(OriginSet::all());
        drop(guard);

        self.doc.nodes_to_stylesheet.remove(&node_id);
        // The CSSOM sheet object outlives the owner. It stays readable and
        // editable; it just leaves the document cascade
        // (<https://drafts.csswg.org/cssom/#remove-a-css-style-sheet>).
        self.doc.detach_cssom_sheet(node_id);
        self.doc.stylesheet_generation += 1;
    }

    /// Drops the sheet a processing instruction owned. A PI has no element
    /// special data, so the owner map and the stylist are updated directly
    /// (<https://www.w3.org/TR/xml-stylesheet/>).
    fn unload_pi_stylesheet(&mut self, node_id: NodeId) {
        self.doc.pi_stylesheet_requests.remove(&node_id);
        let Some(stylesheet) = self.doc.nodes_to_stylesheet.remove(&node_id) else {
            return;
        };

        let guard = self.doc.guard.read();
        self.doc.stylist.remove_stylesheet(stylesheet, &guard);
        self.doc
            .stylist
            .force_stylesheet_origins_dirty(OriginSet::all());
        drop(guard);

        self.doc.detach_cssom_sheet(node_id);
        self.doc.stylesheet_generation += 1;
    }

    pub(crate) fn load_image(&mut self, target_id: NodeId, mutation: ImageMutation) {
        let node = &self.doc.nodes[target_id];
        let raw_src = node.attr(local_name!("src"));
        let raw_srcset = node.attr(local_name!("srcset"));
        let has_source_set = raw_srcset.is_some_and(|set| !set.trim().is_empty())
            || node.parent.is_some_and(|parent| {
                self.doc.get_node(parent).is_some_and(|parent| {
                    parent.element_data().is_some_and(|element| {
                        element.name.local == local_name!("picture")
                            && parent.children.iter().any(|child| {
                                self.doc.get_node(*child).is_some_and(|child| {
                                    child.element_data().is_some_and(|element| {
                                        element.name.local == local_name!("source")
                                            && element
                                                .attr(local_name!("srcset"))
                                                .is_some_and(|set| !set.trim().is_empty())
                                    })
                                })
                            })
                    })
                })
            });
        // No `src` and no source set aborts any request the element already
        // had: the element is complete without a source, and no event fires
        // because it has neither a `src` nor a source set
        // (<https://html.spec.whatwg.org/multipage/images.html#updating-the-image-data>).
        if raw_src.is_none() && !has_source_set {
            let previous = self.doc.image_requests.remove(&target_id);
            if let Some(previous) = previous {
                if let Some(previous_key) = previous.key
                    && let Some(waiters) = self
                        .doc
                        .pending_images
                        .get_mut(&previous_key.url)
                        .and_then(|by_crossorigin| {
                            by_crossorigin.get_mut(&previous_key.crossorigin)
                        })
                {
                    waiters.retain(|(node, _)| *node != target_id);
                }
                let node = &mut self.doc.nodes[target_id];
                if let Some(element) = node.element_data_mut() {
                    element.special_data = SpecialElementData::Image(Box::new(ImageData::None));
                }
                node.clear_layout_cache();
                node.insert_damage(ALL_DAMAGE);
                self.doc
                    .notify_image_settled(target_id, ImageOutcome::Aborted);
            }
            return;
        }
        // The source set is the `srcset` candidates plus `src` as a 1x
        // fallback when the set has no 1x entry, and the request uses the one
        // candidate selection picks
        // (<https://html.spec.whatwg.org/multipage/images.html#creating-a-source-set-from-attributes>).
        // A candidate whose URL cannot be resolved cannot be fetched either, so
        // selection runs over the resolved candidates.
        let mut candidates = raw_srcset.map(crate::srcset::parse_srcset).unwrap_or_default();
        if let Some(raw) = raw_src.filter(|src| !src.is_empty())
            && !candidates.iter().any(|candidate| candidate.density == 1.0)
        {
            candidates.push(crate::srcset::ImageCandidate {
                url: raw.to_owned(),
                density: 1.0,
            });
        }
        let resolved: Vec<(url::Url, f64)> = candidates
            .iter()
            .filter_map(|candidate| {
                self.doc
                    .resolve_url(&candidate.url)
                    .map(|url| (url, candidate.density))
            })
            .collect();
        let densities: Vec<f64> = resolved
            .iter()
            .map(|(_, density)| *density)
            .collect();
        let src = crate::srcset::choose_density(&densities, self.doc.viewport.scale_f64())
            .map(|index| resolved[index].0.clone());
        let crossorigin = node.cors_setting();
        let referrer_policy = node.referrer_policy();
        // A source that names no absolute URL starts no request, so its key is
        // absent: the element is already broken and settles without pixels.
        let key = src
            .as_ref()
            .map(|url| ImageKey { url: url.clone(), crossorigin });
        // HTML restarts the update when `crossorigin` or `referrerpolicy`
        // changes *state*; a write that leaves the state alone does nothing
        // (<https://html.spec.whatwg.org/multipage/images.html#reacting-to-dom-mutations>).
        self.doc.image_request_generation = self.doc.image_request_generation.wrapping_add(1);
        let state = ImageRequestState {
            key: key.clone(),
            referrer_policy: referrer_policy.clone(),
            status: ImageRequestStatus::Pending,
            final_url: None,
            origin_clean: false,
            generation: self.doc.image_request_generation,
        };
        if mutation != ImageMutation::Src
            && self
                .doc
                .image_requests
                .get(&target_id)
                .is_some_and(|previous| {
                    previous.key == state.key && previous.referrer_policy == state.referrer_policy
                })
        {
            return;
        }
        // The node's old pixels belong to its old request; the new request's
        // image data is unavailable until it settles
        // (<https://html.spec.whatwg.org/multipage/images.html#updating-the-image-data>).
        let previous = self.doc.image_requests.insert(target_id, state);
        if let Some(previous) = previous.as_ref()
            && previous.key != key
            && let Some(previous_key) = &previous.key
            && let Some(waiters) = self
                .doc
                .pending_images
                .get_mut(&previous_key.url)
                .and_then(|by_crossorigin| by_crossorigin.get_mut(&previous_key.crossorigin))
        {
            waiters.retain(|(node, _)| *node != target_id);
        }
        let node = &mut self.doc.nodes[target_id];
        if let Some(element) = node.element_data_mut() {
            element.special_data = SpecialElementData::Image(Box::new(ImageData::None));
        }
        node.clear_layout_cache();
        node.insert_damage(ALL_DAMAGE);

        self.doc
            .notify_image_settled(target_id, ImageOutcome::Started);
        // An empty attribute, a URL that cannot be resolved, and a source set
        // (`srcset`, or a `<picture>` `<source>`) this engine cannot select
        // from all leave the request broken. The failure is delivered on a
        // later turn, so `complete` stays false until the error is due and a
        // script that reads it right after the write sees a request in flight
        // (<https://html.spec.whatwg.org/multipage/images.html#updating-the-image-data>).
        let Some(src) = src else {
            let _ = self.doc.tx.send(crate::document::DocumentEvent::ImageFailed {
                node: target_id,
                generation: self.doc.image_request_generation,
            });
            return;
        };
        if let Some(cached) = self
            .doc
            .image_cache
            .get(&src)
            .and_then(|by_crossorigin| by_crossorigin.get(&crossorigin))
            .cloned()
        {
            let node = &mut self.doc.nodes[target_id];
            node.element_data_mut().unwrap().special_data =
                SpecialElementData::Image(Box::new(cached.image));
            node.clear_layout_cache();
            node.insert_damage(ALL_DAMAGE);
            if let Some(request) = self.doc.image_requests.get_mut(&target_id) {
                request.status = ImageRequestStatus::Loaded;
                request.final_url = cached.final_url;
                request.origin_clean = cached.origin_clean;
            }
            self.doc
                .notify_image_settled(target_id, ImageOutcome::Loaded);
            return;
        }

        if let Some(waiting_list) = self
            .doc
            .pending_images
            .get_mut(&src)
            .and_then(|by_crossorigin| by_crossorigin.get_mut(&crossorigin))
        {
            // A restart that selects the request the node is already waiting on
            // joins it: one request settles a node once, so it fires one `load`
            // or `error`. A restart that changes the request (a new
            // `referrerpolicy` state on the same URL) starts a new fetch, or the
            // in-flight request's sampled policy would stand for a request the
            // page has already replaced
            // (<https://html.spec.whatwg.org/multipage/images.html#updating-the-image-data>).
            let waiter = (target_id, ImageType::Image);
            let same_request = previous.as_ref().is_none_or(|previous| {
                // This node had no request for this key yet: it joins the one
                // another element already started. Otherwise the same key and
                // the same policy is the same request.
                previous.key != key || previous.referrer_policy == referrer_policy
            });
            if same_request {
                if !waiting_list.contains(&waiter) {
                    waiting_list.push(waiter);
                }
                return;
            }
        }

        // `previous` named the request this node was waiting on; a request that
        // differs leaves that one's waiter list and joins this one.
        let waiters = self
            .doc
            .pending_images
            .entry(src.clone())
            .or_default()
            .entry(crossorigin)
            .or_default();
        let waiter = (target_id, ImageType::Image);
        if !waiters.contains(&waiter) {
            waiters.push(waiter);
        }

        let mut request = self
            .doc
            .build_request(src)
            .kind(blitz_traits::net::RequestKind::Image)
            .node(target_id);
        if let Some(crossorigin) = crossorigin {
            request = request.crossorigin(crossorigin);
        }
        if let Some(referrer_policy) = referrer_policy {
            request = request.referrer_policy(referrer_policy);
        }
        // `src` resolved to a URL, so the key is present.
        let key = key.expect("a resolved source has a key");
        self.doc.net_provider.fetch(
            self.doc.id(),
            request,
            ResourceHandler::boxed(
                self.doc.tx.clone(),
                self.doc.id(),
                None, // Handled through `pending_images`.
                self.doc.shell_provider.clone(),
                ImageHandler::new(ImageType::Image, key),
            ),
        );
    }

    fn load_iframe(&mut self, target_id: NodeId) {
        if self.doc.subdocument_depth >= crate::iframe::MAX_SUBDOCUMENT_DEPTH {
            #[cfg(feature = "tracing")]
            tracing::warn!(
                "Not loading iframe: max sub-document nesting depth ({}) reached",
                crate::iframe::MAX_SUBDOCUMENT_DEPTH
            );
            return;
        }

        let node = &self.doc.nodes[target_id];
        let Some(element) = node.element_data() else {
            return;
        };

        // `srcdoc` takes precedence over `src`
        if let Some(srcdoc) = element.attr(local_name!("srcdoc")) {
            let srcdoc = srcdoc.to_string();
            self.doc.load_iframe_srcdoc(target_id, &srcdoc);
            return;
        }

        let Some(raw_src) = element.attr(local_name!("src")) else {
            return;
        };
        if raw_src.is_empty() {
            return;
        }
        let Some(url) = self.doc.url.resolve_relative(raw_src) else {
            #[cfg(feature = "tracing")]
            tracing::warn!("Not loading iframe: could not resolve url {raw_src}");
            return;
        };
        self.doc.start_iframe_load(target_id, url);
    }

    fn load_custom_paint_src(&mut self, target_id: NodeId) {
        let node = &mut self.doc.nodes[target_id];
        if let Some(raw_src) = node.attr(local_name!("src")) {
            if let Ok(custom_paint_source_id) = raw_src.parse::<u64>() {
                self.recompute_is_animating = true;
                let canvas_data = SpecialElementData::Canvas(CanvasData {
                    custom_paint_source_id,
                });
                node.element_data_mut().unwrap().special_data = canvas_data;
            }
        }
    }

    fn process_button_input(&mut self, target_id: NodeId) {
        let node = &self.doc.nodes[target_id];
        let Some(data) = node.element_data() else {
            return;
        };

        let tagname = data.name.local.as_ref();
        let type_attr = data.attr(local_name!("type"));
        let value = data.attr(local_name!("value"));

        // Add content of "value" attribute as a text node child if:
        //   - Tag name is
        if let ("input", Some("button" | "submit" | "reset"), Some(value)) =
            (tagname, type_attr, value)
        {
            let value = value.to_string();
            let id = self.create_text_node(&value);
            self.append_children(target_id, &[id]);
            return;
        }
        #[cfg(feature = "file-input")]
        if let ("input", Some("file")) = (tagname, type_attr) {
            let button_id = self.create_element(
                qual_name!("button", html),
                vec![
                    Attribute {
                        name: qual_name!("type", html),
                        value: "button".to_string(),
                    },
                    Attribute {
                        name: qual_name!("tabindex", html),
                        value: "-1".to_string(),
                    },
                ],
            );
            let label_id = self.create_element(qual_name!("label", html), vec![]);
            let text_id = self.create_text_node("No File Selected");
            let button_text_id = self.create_text_node("Browse");
            self.append_children(target_id, &[button_id, label_id]);
            self.append_children(label_id, &[text_id]);
            self.append_children(button_id, &[button_text_id]);
        }
    }
}

/// Type that allows mutable access to the viewport
/// And syncs it back to stylist on drop.
pub struct ViewportMut<'doc> {
    doc: &'doc mut BaseDocument,
    initial_viewport: Viewport,
}
impl ViewportMut<'_> {
    pub fn new(doc: &mut BaseDocument) -> ViewportMut<'_> {
        let initial_viewport = doc.viewport.clone();
        ViewportMut {
            doc,
            initial_viewport,
        }
    }
}
impl Deref for ViewportMut<'_> {
    type Target = Viewport;

    fn deref(&self) -> &Self::Target {
        &self.doc.viewport
    }
}
impl DerefMut for ViewportMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.doc.viewport
    }
}
impl Drop for ViewportMut<'_> {
    fn drop(&mut self) {
        if self.doc.viewport == self.initial_viewport {
            return;
        }

        let changes = DeviceChanges::from_viewports(&self.initial_viewport, &self.doc.viewport);
        self.doc.queue_device_changes(changes);
    }
}

#[cfg(test)]
mod test {
    use style::media_queries::MediaType;
    use style::servo_arc::Arc as ServoArc;
    use style_dom::ElementState;

    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use blitz_traits::shell::{ColorScheme, ShellProvider, Viewport};

    use super::{DocumentMutator, NodeId};
    use crate::{Attribute, BaseDocument, DocumentConfig, ElementData, NodeData, qual_name};

    #[cfg(feature = "custom-widget")]
    #[test]
    fn custom_widget_replays_initial_attributes() {
        use std::sync::Mutex;

        type AttributeChange = (String, Option<String>, Option<String>);
        struct RecordingWidget(Arc<Mutex<Vec<AttributeChange>>>);

        impl crate::Widget for RecordingWidget {
            fn attribute_changed(&mut self, name: &str, old: Option<&str>, new: Option<&str>) {
                self.0.lock().unwrap().push((
                    name.to_owned(),
                    old.map(str::to_owned),
                    new.map(str::to_owned),
                ));
            }
        }

        for via_mutator in [false, true] {
            for initially_empty in [false, true] {
                let mut document = BaseDocument::new(DocumentConfig::default());
                let attrs = if initially_empty {
                    vec![]
                } else {
                    vec![
                        Attribute {
                            name: qual_name!("title"),
                            value: "initial".into(),
                        },
                        Attribute {
                            name: qual_name!("lang"),
                            value: "".into(),
                        },
                    ]
                };
                let id = document.create_node(NodeData::Element(Box::new(ElementData::new(
                    qual_name!("div"),
                    attrs,
                ))));
                let calls = Arc::new(Mutex::new(Vec::new()));
                let widget = Box::new(RecordingWidget(Arc::clone(&calls)));
                if via_mutator {
                    document.mutate().set_custom_widget(id, widget);
                } else {
                    document.set_custom_widget(id, widget);
                }

                let mut expected = if initially_empty {
                    vec![]
                } else {
                    vec![
                        ("title".into(), None, Some("initial".into())),
                        ("lang".into(), None, Some("".into())),
                    ]
                };
                assert_eq!(*calls.lock().unwrap(), expected);

                let mut mutator = document.mutate();
                mutator.set_attribute(id, qual_name!("title"), "updated");
                mutator.clear_attribute(id, qual_name!("title"));
                expected.extend([
                    (
                        "title".into(),
                        (!initially_empty).then(|| "initial".into()),
                        Some("updated".into()),
                    ),
                    ("title".into(), Some("updated".into()), None),
                ]);
                assert_eq!(*calls.lock().unwrap(), expected);
            }
        }
    }

    #[test]
    fn media_type_defaults_to_screen() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        assert_eq!(*document.media_type(), MediaType::screen());
        assert_eq!(document.stylist_device().media_type(), MediaType::screen());
    }

    #[test]
    fn media_type_honors_config() {
        let mut document = BaseDocument::new(DocumentConfig {
            media_type: Some(MediaType::print()),
            ..Default::default()
        });
        assert_eq!(*document.media_type(), MediaType::print());
        assert_eq!(document.stylist_device().media_type(), MediaType::print());
    }

    #[test]
    fn set_media_type_updates_stylist_device() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        assert_eq!(document.stylist_device().media_type(), MediaType::screen());

        document.set_media_type(MediaType::print());
        assert_eq!(*document.media_type(), MediaType::print());
        assert_eq!(document.stylist_device().media_type(), MediaType::print());
    }

    #[test]
    fn mutator_remove_disabled() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let id = document.create_node(NodeData::Element(Box::new(ElementData::new(
            qual_name!("button"),
            vec![Attribute {
                name: qual_name!("disabled"),
                value: "".into(),
            }],
        ))));

        let node = document.get_node(id).unwrap();
        assert!(
            node.element_state().contains(ElementState::DISABLED),
            "form node is disabled"
        );
        assert!(
            !node.element_state().contains(ElementState::ENABLED),
            "form node is not enabled yet"
        );

        let mut mutator = document.mutate();
        mutator.clear_attribute(id, qual_name!("disabled"));
        drop(mutator);

        let node = document.get_node(id).unwrap();
        assert!(
            !node.element_state().contains(ElementState::DISABLED),
            "form node is no longer disabled"
        );
        assert!(
            node.element_state().contains(ElementState::ENABLED),
            "form node is enabled"
        );
    }

    #[test]
    fn mutator_set_disabled() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let id = document.create_node(NodeData::Element(Box::new(ElementData::new(
            qual_name!("button"),
            vec![],
        ))));

        let node = document.get_node(id).unwrap();
        assert!(
            !node.element_state().contains(ElementState::DISABLED),
            "form node is not disabled"
        );
        assert!(
            node.element_state().contains(ElementState::ENABLED),
            "form node is enabled"
        );

        let mut mutator = document.mutate();
        mutator.set_attribute(id, qual_name!("disabled"), "");
        drop(mutator);

        let node = document.get_node(id).unwrap();

        assert!(
            node.element_state().contains(ElementState::DISABLED),
            "form node is disabled"
        );
        assert!(
            !node.element_state().contains(ElementState::ENABLED),
            "form node is no longer enabled enabled"
        );
    }

    #[test]
    fn mutator_set_disabled_invalid_node() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let id = document.create_node(NodeData::Element(Box::new(ElementData::new(
            qual_name!("a"),
            vec![],
        ))));

        let node = document.get_node(id).unwrap();
        assert!(
            !node.element_state().contains(ElementState::DISABLED),
            "form node is not disabled"
        );
        assert!(
            !node.element_state().contains(ElementState::ENABLED),
            "form node is enabled"
        );

        let mut mutator = document.mutate();
        mutator.set_attribute(id, qual_name!("disabled"), "");
        drop(mutator);

        let node = document.get_node(id).unwrap();
        assert!(
            !node.element_state().contains(ElementState::DISABLED),
            "form node is not disabled"
        );
        assert!(
            !node.element_state().contains(ElementState::ENABLED),
            "form node is enabled"
        );
    }

    #[test]
    fn mutator_id_attribute_updates_id_map() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let root_id = document.root_node().id;

        let node_id = {
            let mut mutator = document.mutate();
            let node_id = mutator.create_element(
                qual_name!("div"),
                vec![Attribute {
                    name: qual_name!("id"),
                    value: "old".into(),
                }],
            );
            mutator.append_children(root_id, &[node_id]);
            node_id
        };
        assert_eq!(document.get_element_by_id("old"), Some(node_id));

        {
            let mut mutator = document.mutate();
            mutator.set_attribute(node_id, qual_name!("id"), "new");
        }
        assert_eq!(document.get_element_by_id("new"), Some(node_id));
        assert_eq!(document.get_element_by_id("old"), None);

        {
            let mut mutator = document.mutate();
            mutator.clear_attribute(node_id, qual_name!("id"));
        }
        assert_eq!(document.get_element_by_id("new"), None);
    }

    #[test]
    fn get_element_by_id_duplicate_ids_first_in_tree_order_wins() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let root_id = document.root_node().id;

        let (first_id, second_id) = {
            let mut mutator = document.mutate();
            let first_id = mutator.create_element(qual_name!("div"), vec![]);
            let second_id = mutator.create_element(qual_name!("div"), vec![]);
            mutator.append_children(root_id, &[first_id, second_id]);
            // Assign the id to the later node first so that insertion order
            // differs from tree order
            mutator.set_attribute(second_id, qual_name!("id"), "dup");
            mutator.set_attribute(first_id, qual_name!("id"), "dup");
            (first_id, second_id)
        };
        assert_eq!(document.get_element_by_id("dup"), Some(first_id));

        {
            let mut mutator = document.mutate();
            mutator.remove_node(first_id);
        }
        assert_eq!(document.get_element_by_id("dup"), Some(second_id));
    }

    #[derive(Default)]
    struct RedrawShell {
        redraw_requests: AtomicUsize,
    }

    impl ShellProvider for RedrawShell {
        fn request_redraw(&self) {
            self.redraw_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn mutator_requests_redraw_only_after_mutation() {
        let shell = Arc::new(RedrawShell::default());
        let mut document = BaseDocument::new(DocumentConfig {
            shell_provider: Some(shell.clone()),
            ..Default::default()
        });
        let root_id = document.root_node().id;

        {
            let mut mutator = document.mutate();
            let parent_id = mutator.create_element(qual_name!("div"), vec![]);
            let child_id = mutator.create_element(qual_name!("span"), vec![]);
            mutator.append_children(parent_id, &[child_id]);
            mutator.remove_and_drop_all_children(parent_id);
            mutator.set_attribute(parent_id, qual_name!("id"), "detached");
        }
        assert_eq!(shell.redraw_requests.load(Ordering::Relaxed), 0);

        {
            let mutator = document.mutate();
            assert_eq!(mutator.child_ids(root_id).len(), 0);
        }

        {
            let mut mutator = document.mutate();
            let node_id = mutator.create_element(qual_name!("div"), vec![]);
            mutator.append_children(root_id, &[node_id]);
            mutator.set_attribute(node_id, qual_name!("id"), "in-document");
        }
        assert_eq!(shell.redraw_requests.load(Ordering::Relaxed), 1);

        {
            let mut mutator = document.mutate();
            let parent_id = mutator.create_element(qual_name!("div"), vec![]);
            let child_id = mutator.create_element(qual_name!("span"), vec![]);
            mutator.append_children(root_id, &[parent_id]);
            mutator.append_children(parent_id, &[child_id]);
            mutator.remove_and_drop_all_children(parent_id);
        }
        assert_eq!(shell.redraw_requests.load(Ordering::Relaxed), 2);

        {
            let mut mutator = document.mutate();
            let parent_id = mutator.create_element(qual_name!("div"), vec![]);
            let child_id = mutator.create_element(qual_name!("span"), vec![]);
            let detached_target_id = mutator.create_element(qual_name!("div"), vec![]);
            mutator.append_children(root_id, &[parent_id]);
            mutator.append_children(parent_id, &[child_id]);
            assert_eq!(shell.redraw_requests.load(Ordering::Relaxed), 2);
            mutator.append_children(detached_target_id, &[child_id]);
        }
        assert_eq!(shell.redraw_requests.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn moving_subtree_out_of_document_clears_in_document_flag() {
        let shell = Arc::new(RedrawShell::default());
        let mut document = BaseDocument::new(DocumentConfig {
            shell_provider: Some(shell.clone()),
            ..Default::default()
        });
        let root_id = document.root_node().id;
        let (child_id, grandchild_id, detached_parent_id) = {
            let mut mutator = document.mutate();
            let in_document_parent_id = mutator.create_element(qual_name!("div"), vec![]);
            let child_id = mutator.create_element(qual_name!("div"), vec![]);
            let grandchild_id = mutator.create_element(qual_name!("span"), vec![]);
            let detached_parent_id = mutator.create_element(qual_name!("section"), vec![]);
            mutator.append_children(root_id, &[in_document_parent_id]);
            mutator.append_children(in_document_parent_id, &[child_id]);
            mutator.append_children(child_id, &[grandchild_id]);
            (child_id, grandchild_id, detached_parent_id)
        };
        assert_eq!(shell.redraw_requests.load(Ordering::Relaxed), 1);
        assert!(document.get_node(child_id).unwrap().flags.is_in_document());
        assert!(
            document
                .get_node(grandchild_id)
                .unwrap()
                .flags
                .is_in_document()
        );

        {
            let mut mutator = document.mutate();
            mutator.append_children(detached_parent_id, &[child_id]);
        }
        assert_eq!(shell.redraw_requests.load(Ordering::Relaxed), 2);
        assert!(!document.get_node(child_id).unwrap().flags.is_in_document());
        assert!(
            !document
                .get_node(grandchild_id)
                .unwrap()
                .flags
                .is_in_document()
        );

        {
            let mut mutator = document.mutate();
            mutator.set_attribute(child_id, qual_name!("id"), "detached");
        }
        assert_eq!(shell.redraw_requests.load(Ordering::Relaxed), 2);

        {
            let mut mutator = document.mutate();
            mutator.append_children(root_id, &[child_id]);
        }
        assert_eq!(shell.redraw_requests.load(Ordering::Relaxed), 3);
        assert!(document.get_node(child_id).unwrap().flags.is_in_document());
        assert!(
            document
                .get_node(grandchild_id)
                .unwrap()
                .flags
                .is_in_document()
        );
    }

    #[test]
    fn style_property_updates_nested_layout() {
        let mut document = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        let root_id = document.root_node().id;

        let mover_id = {
            let mut mutator = document.mutate();
            let parent_id = mutator.create_element(qual_name!("div"), vec![]);
            let mover_id = mutator.create_element(qual_name!("div"), vec![]);
            mutator.set_style_property(parent_id, "position", "relative");
            mutator.set_style_property(parent_id, "width", "800px");
            mutator.set_style_property(parent_id, "height", "600px");
            mutator.set_style_property(mover_id, "position", "absolute");
            mutator.set_style_property(mover_id, "left", "0px");
            mutator.set_style_property(mover_id, "top", "0px");
            mutator.append_children(parent_id, &[mover_id]);
            mutator.append_children(root_id, &[parent_id]);
            mover_id
        };

        document.resolve(0.0);
        assert_eq!(
            document
                .get_node(mover_id)
                .unwrap()
                .final_layout()
                .location
                .x,
            0.0
        );

        {
            let mut mutator = document.mutate();
            mutator.set_style_property(mover_id, "left", "120px");
        }

        document.resolve(0.0);
        assert_eq!(
            document
                .get_node(mover_id)
                .unwrap()
                .final_layout()
                .location
                .x,
            120.0
        );
    }

    /// Hit testing before the first resolve reaches elements that have no computed styles
    /// yet: a pointer already over a window as it opens.
    #[test]
    fn hit_testing_before_the_first_resolve() {
        let mut document = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        let root_id = document.root_node().id;
        {
            let mut mutator = document.mutate();
            let html = mutator.create_element(qual_name!("html", html), vec![]);
            mutator.append_children(root_id, &[html]);
        }
        // Nothing is laid out yet, so nothing is hit; what matters is that it doesn't panic.
        assert!(document.hit(10.0, 10.0).is_none());
    }

    #[test]
    fn style_property_mutation_copies_blocks_in_rule_tree() {
        let mut document = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        let root_id = document.root_node().id;

        let div_id = {
            let mut mutator = document.mutate();
            let div_id = mutator.create_element(qual_name!("div"), vec![]);
            mutator.set_style_property(div_id, "width", "100px");
            mutator.set_style_property(div_id, "height", "50px");
            mutator.append_children(root_id, &[div_id]);
            div_id
        };

        let style_block = |document: &BaseDocument| {
            document
                .get_node(div_id)
                .unwrap()
                .element_data()
                .unwrap()
                .style_attribute
                .clone()
                .unwrap()
        };

        let before_resolve = style_block(&document);
        {
            let mut mutator = document.mutate();
            mutator.set_style_property(div_id, "height", "60px");
        }
        // Not yet in the rule tree: mutated in place.
        assert!(ServoArc::ptr_eq(&before_resolve, &style_block(&document)));

        document.resolve(0.0);
        let in_rule_tree = style_block(&document);
        assert!(
            in_rule_tree
                .read_with(&document.guard.read())
                .immutable
                .load(Ordering::Relaxed)
        );

        {
            let mut mutator = document.mutate();
            mutator.set_style_property(div_id, "width", "200px");
        }
        let after_set = style_block(&document);
        assert!(!ServoArc::ptr_eq(&in_rule_tree, &after_set));
        {
            let guard = document.guard.read();
            let old = in_rule_tree.read_with(&guard);
            let new = after_set.read_with(&guard);
            assert_eq!(old.declarations().len(), 2);
            assert_eq!(new.declarations().len(), 2);
            assert_ne!(old, new);
        }

        document.resolve(0.0);
        assert_eq!(
            document.get_node(div_id).unwrap().final_layout().size.width,
            200.0
        );

        let in_rule_tree = style_block(&document);
        {
            let mut mutator = document.mutate();
            mutator.remove_style_property(div_id, "height");
        }
        let after_remove = style_block(&document);
        assert!(!ServoArc::ptr_eq(&in_rule_tree, &after_remove));
        {
            let guard = document.guard.read();
            assert_eq!(in_rule_tree.read_with(&guard).declarations().len(), 2);
            assert_eq!(after_remove.read_with(&guard).declarations().len(), 1);
        }
    }

    fn qual_name(name: &str) -> crate::QualName {
        crate::QualName::new(None, crate::ns!(), crate::LocalName::from(name))
    }

    fn attr(name: &str, value: &str) -> Attribute {
        Attribute {
            name: qual_name(name),
            value: value.to_string(),
        }
    }

    fn div(m: &mut DocumentMutator<'_>, attrs: &[(&str, &str)]) -> NodeId {
        let attrs = attrs.iter().map(|(k, v)| attr(k, v)).collect();
        m.create_element(qual_name!("div"), attrs)
    }

    /// Tag, layout and resolved style of every element and `::before`/`::after`, in tree order.
    type Snapshot = Vec<(String, [f32; 4], Vec<String>)>;

    fn snapshot(doc: &BaseDocument) -> Snapshot {
        let mut out = Vec::new();
        let mut stack = vec![(doc.root_node().id, "")];
        while let Some((id, pseudo)) = stack.pop() {
            let node = doc.get_node(id).unwrap();
            if let Some(el) = node.element_data() {
                let l = node.final_layout();
                let rect = [l.location.x, l.location.y, l.size.width, l.size.height];
                let style = crate::resolved_style_property_names()
                    .iter()
                    .map(|name| doc.resolved_style_value(id, name))
                    .collect();
                out.push((format!("{}{pseudo}", el.name.local), rect, style));
            }
            stack.extend(node.after().map(|id| (id, "::after")));
            stack.extend(node.children.iter().rev().map(|id| (*id, "")));
            stack.extend(node.before().map(|id| (id, "::before")));
        }
        out
    }

    /// Asserts that the incremental result matches the fresh one, element by element,
    /// naming the first differing property.
    fn assert_matches_fresh(incremental: &Snapshot, fresh: &Snapshot) {
        let boxes = |s: &Snapshot| -> Vec<(String, [f32; 4])> {
            s.iter().map(|(n, r, _)| (n.clone(), *r)).collect()
        };
        assert_eq!(
            boxes(incremental),
            boxes(fresh),
            "incremental layout differs from a fresh layout of the mutated document"
        );
        for (i, ((name, _, inc), (_, _, fre))) in incremental.iter().zip(fresh).enumerate() {
            let differing: Vec<String> = crate::resolved_style_property_names()
                .iter()
                .zip(inc.iter().zip(fre))
                .filter(|(_, (a, b))| a != b)
                .map(|(property, (a, b))| format!("{property}: {a:?} vs {b:?}"))
                .collect();
            assert!(
                differing.is_empty(),
                "resolved style of {name} (element {i}) differs from a fresh layout of the mutated document: {}",
                differing.join(", ")
            );
        }
    }

    /// Checks that a document mutated after its first layout ends up with the same layout
    /// and styles as a document built directly in the mutated state. `build` returns the
    /// node ids that `mutate` operates on; the mutation must change the layout or styles.
    fn check_incremental_restyle(
        css: &str,
        build: impl Fn(&mut DocumentMutator<'_>) -> Vec<NodeId>,
        mutate: impl Fn(&mut DocumentMutator<'_>, &[NodeId]),
    ) {
        check_incremental_restyle_steps(css, build, &[&mutate]);
    }

    /// One mutation batch of [`check_incremental_restyle_steps`].
    type Step<'a> = &'a dyn Fn(&mut DocumentMutator<'_>, &[NodeId]);

    /// Like [`check_incremental_restyle`], but each step is a separate mutation batch
    /// with a layout in between; the fresh document applies all steps before its layout.
    fn check_incremental_restyle_steps(
        css: &str,
        build: impl Fn(&mut DocumentMutator<'_>) -> Vec<NodeId>,
        steps: &[Step<'_>],
    ) {
        let new_document = || {
            let mut doc = BaseDocument::new(DocumentConfig {
                viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
                ..Default::default()
            });
            doc.add_user_agent_stylesheet(crate::UserAgentStylesheet::parse(&format!(
                "html, div {{ display: block }} {css}"
            )));
            let root_id = doc.root_node().id;
            let ids = {
                let mut m = doc.mutate();
                let html = m.create_element(qual_name!("html"), vec![]);
                m.append_children(root_id, &[html]);
                build(&mut m)
            };
            (doc, ids)
        };

        let (mut incremental, ids) = new_document();
        incremental.resolve(0.0);
        let before = snapshot(&incremental);
        for mutate in steps {
            mutate(&mut incremental.mutate(), &ids);
            incremental.resolve(0.0);
        }
        let after = snapshot(&incremental);

        let (mut fresh, ids) = new_document();
        for mutate in steps {
            mutate(&mut fresh.mutate(), &ids);
        }
        fresh.resolve(0.0);
        assert_matches_fresh(&after, &snapshot(&fresh));
        assert_ne!(
            before, after,
            "the mutation should change the layout or styles"
        );
    }

    fn parent_with_children(m: &mut DocumentMutator<'_>, count: usize) -> Vec<NodeId> {
        let root_id = m.doc.root_element().id;
        let parent = div(m, &[]);
        let children: Vec<NodeId> = (0..count).map(|_| div(m, &[])).collect();
        m.append_children(parent, &children);
        m.append_children(root_id, &[parent]);
        std::iter::once(parent).chain(children).collect()
    }

    /// `html > parent.hidden > [child, child]`, ids in that order; `.hidden` is `display: none`.
    fn hidden_parent_with_children(m: &mut DocumentMutator<'_>, count: usize) -> Vec<NodeId> {
        let ids = parent_with_children(m, count);
        m.set_attribute(ids[0], qual_name!("class"), "hidden");
        ids
    }

    const HIDDEN_CSS: &str = "div { height: 10px; width: 100px } .hidden { display: none }";

    fn unhide(m: &mut DocumentMutator<'_>, ids: &[NodeId]) {
        m.clear_attribute(ids[0], qual_name!("class"));
    }

    #[test]
    fn attribute_change_in_hidden_subtree_then_unhide() {
        check_incremental_restyle_steps(
            &format!("{HIDDEN_CSS} [data-a] ~ div {{ width: 200px; color: red }}"),
            |m| hidden_parent_with_children(m, 3),
            &[
                &|m, ids| m.set_attribute(ids[1], qual_name("data-a"), "1"),
                &unhide,
            ],
        );
    }

    #[test]
    fn class_change_in_hidden_subtree_then_unhide() {
        check_incremental_restyle_steps(
            &format!("{HIDDEN_CSS} .x {{ width: 200px }} .x div {{ color: red }}"),
            |m| {
                let ids = hidden_parent_with_children(m, 1);
                let grandchild = div(m, &[]);
                m.append_children(ids[1], &[grandchild]);
                ids
            },
            &[
                &|m, ids| m.set_attribute(ids[1], qual_name!("class"), "x"),
                &unhide,
            ],
        );
    }

    #[test]
    fn style_change_in_hidden_subtree_then_unhide() {
        check_incremental_restyle_steps(
            &format!("{HIDDEN_CSS} .half {{ width: 50% }}"),
            |m| {
                let ids = hidden_parent_with_children(m, 1);
                let grandchild = div(m, &[("class", "half")]);
                m.append_children(ids[1], &[grandchild]);
                ids
            },
            &[
                &|m, ids| m.set_attribute(ids[1], qual_name!("style"), "width: 400px"),
                &unhide,
            ],
        );
    }

    #[test]
    fn insert_in_hidden_subtree_then_unhide() {
        check_incremental_restyle_steps(
            &format!("{HIDDEN_CSS} div:last-child {{ height: 30px }} div:empty {{ width: 20px }}"),
            |m| hidden_parent_with_children(m, 1),
            &[
                &|m, ids| {
                    let child = div(m, &[]);
                    m.append_children(ids[0], &[child]);
                },
                &unhide,
            ],
        );
    }

    #[test]
    fn remove_in_hidden_subtree_then_unhide() {
        check_incremental_restyle_steps(
            &format!("{HIDDEN_CSS} div:first-child {{ height: 30px }}"),
            |m| hidden_parent_with_children(m, 2),
            &[
                &|m, ids| {
                    m.remove_and_drop_node(ids[1]);
                },
                &unhide,
            ],
        );
    }

    #[test]
    fn pseudo_rule_matched_in_hidden_subtree_then_unhide() {
        check_incremental_restyle_steps(
            &format!(
                "{HIDDEN_CSS} .x > div::before {{ content: 'a'; display: block; height: 20px }}"
            ),
            |m| {
                let ids = hidden_parent_with_children(m, 1);
                let grandchild = div(m, &[]);
                m.append_children(ids[1], &[grandchild]);
                ids
            },
            &[
                &|m, ids| m.set_attribute(ids[1], qual_name!("class"), "x"),
                &unhide,
            ],
        );
    }

    #[test]
    fn unhide_and_attribute_change_in_one_batch() {
        check_incremental_restyle(
            &format!("{HIDDEN_CSS} [data-a] ~ div {{ width: 200px; color: red }}"),
            |m| hidden_parent_with_children(m, 3),
            |m, ids| {
                unhide(m, ids);
                m.set_attribute(ids[1], qual_name("data-a"), "1");
            },
        );
    }

    #[test]
    fn hide_and_attribute_change_then_unhide() {
        check_incremental_restyle_steps(
            &format!("{HIDDEN_CSS} [data-a] ~ div {{ width: 200px; color: red }}"),
            |m| parent_with_children(m, 3),
            &[
                &|m, ids| {
                    m.set_attribute(ids[0], qual_name!("class"), "hidden");
                    m.set_attribute(ids[1], qual_name("data-a"), "1");
                },
                &unhide,
            ],
        );
    }

    #[test]
    fn hide_then_mutate_then_unhide() {
        check_incremental_restyle_steps(
            &format!("{HIDDEN_CSS} .x {{ color: red }} div:empty {{ width: 20px }}"),
            |m| parent_with_children(m, 2),
            &[
                &|m, ids| m.set_attribute(ids[0], qual_name!("class"), "hidden"),
                &|m, ids| {
                    m.set_attribute(ids[1], qual_name!("class"), "x");
                    m.remove_and_drop_node(ids[2]);
                },
                &unhide,
            ],
        );
    }

    #[test]
    fn class_change_restyles_descendants() {
        check_incremental_restyle(
            "div { height: 10px; width: 100px } .wide div { width: 300px }",
            |m| parent_with_children(m, 1),
            |m, ids| m.set_attribute(ids[0], qual_name!("class"), "wide"),
        );
    }

    #[test]
    fn class_removal_restyles_descendants() {
        check_incremental_restyle(
            "div { height: 10px; width: 100px } .wide div { width: 300px }",
            |m| {
                let ids = parent_with_children(m, 1);
                m.set_attribute(ids[0], qual_name!("class"), "wide");
                ids
            },
            |m, ids| m.clear_attribute(ids[0], qual_name!("class")),
        );
    }

    #[test]
    fn id_change_restyles_children() {
        check_incremental_restyle(
            "div { height: 10px; width: 100px } #x > div { height: 50px }",
            |m| parent_with_children(m, 2),
            |m, ids| m.set_attribute(ids[0], qual_name!("id"), "x"),
        );
    }

    #[test]
    fn attribute_change_restyles_later_siblings() {
        check_incremental_restyle(
            "div { height: 10px; width: 50px } [data-a] ~ div { width: 200px }",
            |m| parent_with_children(m, 3),
            |m, ids| m.set_attribute(ids[1], qual_name("data-a"), "1"),
        );
    }

    #[test]
    fn style_attribute_change_restyles_inheriting_descendants() {
        check_incremental_restyle(
            "div { height: 10px } .em { width: 2em }",
            |m| {
                let root_id = m.doc.root_element().id;
                let parent = div(m, &[]);
                let child = div(m, &[("class", "em")]);
                m.append_children(parent, &[child]);
                m.append_children(root_id, &[parent]);
                vec![parent, child]
            },
            |m, ids| m.set_attribute(ids[0], qual_name!("style"), "font-size: 40px"),
        );
    }

    #[test]
    fn style_width_change_relayouts_descendants() {
        check_incremental_restyle(
            "div { height: 10px } .half { width: 50% }",
            |m| {
                let root_id = m.doc.root_element().id;
                let parent = div(m, &[("style", "width: 400px")]);
                let child = div(m, &[("class", "half")]);
                m.append_children(parent, &[child]);
                m.append_children(root_id, &[parent]);
                vec![parent, child]
            },
            |m, ids| m.set_style_property(ids[0], "width", "200px"),
        );
    }

    #[test]
    fn appending_a_child_restyles_empty_parent() {
        check_incremental_restyle(
            "div { height: 10px; width: 100px } div:empty { height: 30px }",
            |m| parent_with_children(m, 0),
            |m, ids| {
                let child = div(m, &[]);
                m.append_children(ids[0], &[child]);
            },
        );
    }

    #[test]
    fn removing_the_last_child_restyles_empty_parent() {
        check_incremental_restyle(
            "div { height: 10px; width: 100px } div:empty { height: 30px }",
            |m| parent_with_children(m, 1),
            |m, ids| {
                m.remove_and_drop_node(ids[1]);
            },
        );
    }

    #[test]
    fn inserting_a_child_restyles_nth_child_siblings() {
        check_incremental_restyle(
            "div { height: 10px; width: 50px } div:nth-child(2n) { width: 200px }",
            |m| parent_with_children(m, 3),
            |m, ids| {
                let child = div(m, &[]);
                m.insert_nodes_before(ids[1], &[child]);
            },
        );
    }

    #[test]
    fn removing_a_child_restyles_nth_child_siblings() {
        check_incremental_restyle(
            "div { height: 10px; width: 50px } div:nth-child(2n) { width: 200px }",
            |m| parent_with_children(m, 3),
            |m, ids| {
                m.remove_and_drop_node(ids[1]);
            },
        );
    }

    #[test]
    fn appending_a_child_restyles_previous_last_child() {
        check_incremental_restyle(
            "div { height: 10px; width: 50px } div:last-child { width: 200px }",
            |m| parent_with_children(m, 2),
            |m, ids| {
                let child = div(m, &[]);
                m.append_children(ids[0], &[child]);
            },
        );
    }

    #[test]
    fn removing_the_first_child_restyles_new_first_child() {
        check_incremental_restyle(
            "div { height: 10px; width: 50px } div:first-child { width: 200px }",
            |m| parent_with_children(m, 2),
            |m, ids| {
                m.remove_and_drop_node(ids[1]);
            },
        );
    }

    #[test]
    fn inserting_a_sibling_restyles_later_siblings() {
        check_incremental_restyle(
            "div { height: 10px; width: 50px } .a ~ div { width: 200px }",
            |m| parent_with_children(m, 2),
            |m, ids| {
                let child = div(m, &[("class", "a")]);
                m.insert_nodes_before(ids[1], &[child]);
            },
        );
    }

    #[test]
    fn style_change_on_inline_element_rebuilds_inline_layout() {
        check_incremental_restyle(
            "div { width: 300px; font-size: 20px }",
            |m| {
                let root_id = m.doc.root_element().id;
                let block = div(m, &[]);
                let before = m.create_text_node("Text is available under the ");
                let link = m.create_element(qual_name!("a"), vec![]);
                let link_text =
                    m.create_text_node("Creative Commons Attribution-ShareAlike 4.0 License");
                let after = m.create_text_node("; additional terms may apply.");
                m.append_children(link, &[link_text]);
                m.append_children(block, &[before, link, after]);
                m.append_children(root_id, &[block]);
                vec![block, link]
            },
            |m, ids| m.set_attribute(ids[1], qual_name!("style"), "font-size: 40px"),
        );
    }

    #[test]
    fn moving_a_child_restyles_both_parents() {
        check_incremental_restyle(
            "div { height: 10px; width: 50px } div:empty { height: 30px } div:last-child { width: 200px }",
            |m| {
                let root_id = m.doc.root_element().id;
                let a = div(m, &[]);
                let b = div(m, &[]);
                let child = div(m, &[]);
                let other = div(m, &[]);
                m.append_children(a, &[child, other]);
                m.append_children(root_id, &[a, b]);
                vec![a, b, child]
            },
            |m, ids| m.append_children(ids[1], &[ids[2]]),
        );
    }

    #[test]
    fn class_change_recolors_descendants() {
        check_incremental_restyle(
            "div { height: 10px; width: 100px } .x div { color: red }",
            |m| parent_with_children(m, 1),
            |m, ids| m.set_attribute(ids[0], qual_name!("class"), "x"),
        );
    }

    const PSEUDO_CSS: &str = "div { height: 10px; width: 100px } \
        .x > div::before { content: 'a'; display: block; height: 20px }";

    #[test]
    fn class_change_creates_before_pseudo_on_children() {
        check_incremental_restyle(
            PSEUDO_CSS,
            |m| parent_with_children(m, 1),
            |m, ids| m.set_attribute(ids[0], qual_name!("class"), "x"),
        );
    }

    #[test]
    fn class_removal_removes_before_pseudo_on_children() {
        check_incremental_restyle(
            PSEUDO_CSS,
            |m| {
                let ids = parent_with_children(m, 1);
                m.set_attribute(ids[0], qual_name!("class"), "x");
                ids
            },
            |m, ids| m.clear_attribute(ids[0], qual_name!("class")),
        );
    }

    #[test]
    fn attribute_change_creates_after_pseudo_on_later_siblings() {
        check_incremental_restyle(
            "div { height: 10px; width: 100px } \
             [data-a] ~ div::after { content: 'a'; display: block; height: 20px }",
            |m| parent_with_children(m, 2),
            |m, ids| m.set_attribute(ids[1], qual_name("data-a"), ""),
        );
    }

    #[test]
    fn class_change_recolors_before_pseudo_on_children() {
        check_incremental_restyle(
            "div { height: 10px; width: 100px } \
             div::before { content: 'a'; display: block; height: 20px } \
             .x > div::before { color: red }",
            |m| parent_with_children(m, 1),
            |m, ids| m.set_attribute(ids[0], qual_name!("class"), "x"),
        );
    }

    #[test]
    fn attribute_change_recolors_after_pseudo_on_later_siblings() {
        check_incremental_restyle(
            "div { height: 10px; width: 100px } \
             div::after { content: 'a'; display: block; height: 20px } \
             [data-a] ~ div::after { color: red }",
            |m| parent_with_children(m, 2),
            |m, ids| m.set_attribute(ids[1], qual_name("data-a"), ""),
        );
    }

    /// Like `check_incremental_restyle`, with the focus on `ids[focus]` before the first layout.
    fn check_incremental_restyle_with_focus(
        css: &str,
        focus: usize,
        build: impl Fn(&mut DocumentMutator<'_>) -> Vec<NodeId>,
        mutate: impl Fn(&mut DocumentMutator<'_>, &[NodeId]),
    ) {
        let new_document = || {
            let mut doc = BaseDocument::new(DocumentConfig {
                viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
                ..Default::default()
            });
            doc.add_user_agent_stylesheet(crate::UserAgentStylesheet::parse(&format!(
                "html, div {{ display: block }} {css}"
            )));
            let root_id = doc.root_node().id;
            let ids = {
                let mut m = doc.mutate();
                let html = m.create_element(qual_name!("html"), vec![]);
                m.append_children(root_id, &[html]);
                build(&mut m)
            };
            doc.set_focus_to(ids[focus]);
            (doc, ids)
        };

        let (mut incremental, ids) = new_document();
        incremental.resolve(0.0);
        let before = snapshot(&incremental);
        mutate(&mut incremental.mutate(), &ids);
        incremental.resolve(0.0);
        let after = snapshot(&incremental);

        let (mut fresh, ids) = new_document();
        mutate(&mut fresh.mutate(), &ids);
        fresh.resolve(0.0);
        assert_matches_fresh(&after, &snapshot(&fresh));
        assert_ne!(
            before, after,
            "the mutation should change the layout or styles"
        );
    }

    /// `html > [container > [sibling, holder > [input]], other]`, ids in that order.
    fn focus_tree(m: &mut DocumentMutator<'_>) -> Vec<NodeId> {
        let root_id = m.doc.root_element().id;
        let container = div(m, &[]);
        let sibling = div(m, &[]);
        let holder = div(m, &[]);
        let input = m.create_element(qual_name!("input"), vec![]);
        let other = div(m, &[]);
        m.append_children(holder, &[input]);
        m.append_children(container, &[sibling, holder]);
        m.append_children(root_id, &[container, other]);
        vec![container, sibling, holder, input, other]
    }

    #[test]
    fn removing_the_focused_subtree_restyles_focus_within_ancestors() {
        check_incremental_restyle_with_focus(
            "div { height: 10px; width: 100px } div:focus-within { height: 30px }",
            3,
            focus_tree,
            |m, ids| {
                m.remove_and_drop_node(ids[0]);
            },
        );
    }

    #[test]
    fn moving_the_focused_element_restyles_focus_within_ancestors() {
        check_incremental_restyle_with_focus(
            "div { height: 10px; width: 100px } div:focus-within { height: 30px }",
            3,
            focus_tree,
            |m, ids| m.append_children(ids[4], &[ids[3]]),
        );
    }

    /// `html > parent > [" ", child, child, " "]`, ids `[parent, child, child]`.
    fn parent_with_text_and_children(m: &mut DocumentMutator<'_>, count: usize) -> Vec<NodeId> {
        let root_id = m.doc.root_element().id;
        let parent = div(m, &[]);
        let leading = m.create_text_node(" ");
        let children: Vec<NodeId> = (0..count).map(|_| div(m, &[])).collect();
        let trailing = m.create_text_node(" ");
        m.append_children(parent, &[leading]);
        m.append_children(parent, &children);
        m.append_children(parent, &[trailing]);
        m.append_children(root_id, &[parent]);
        std::iter::once(parent).chain(children).collect()
    }

    #[test]
    fn inserting_before_the_first_element_child_restyles_it_across_text() {
        check_incremental_restyle(
            "div > div:first-child { width: 200px }",
            |m| parent_with_text_and_children(m, 2),
            |m, ids| {
                let new = div(m, &[]);
                m.insert_nodes_before(ids[1], &[new]);
            },
        );
    }

    #[test]
    fn inserting_after_the_last_element_child_restyles_it_across_text() {
        check_incremental_restyle(
            "div > div:last-child { width: 200px }",
            |m| parent_with_text_and_children(m, 2),
            |m, ids| {
                let new = div(m, &[]);
                m.insert_nodes_after(ids[2], &[new]);
            },
        );
    }

    #[test]
    fn removing_the_first_element_child_restyles_the_next_across_text() {
        check_incremental_restyle(
            "div > div:first-child { width: 200px }",
            |m| parent_with_text_and_children(m, 2),
            |m, ids| m.remove_node(ids[1]),
        );
    }

    #[test]
    fn removing_the_last_element_child_restyles_the_previous_across_text() {
        check_incremental_restyle(
            "div > div:last-child { width: 200px }",
            |m| parent_with_text_and_children(m, 2),
            |m, ids| m.remove_node(ids[2]),
        );
    }

    #[test]
    fn removing_the_first_child_restyles_nth_child_siblings() {
        check_incremental_restyle(
            "div > div:nth-child(2) { width: 200px }",
            |m| parent_with_children(m, 3),
            |m, ids| m.remove_node(ids[1]),
        );
    }

    #[test]
    fn removing_children_front_to_back_restyles_nth_child_siblings() {
        check_incremental_restyle(
            "div > div:nth-child(2) { width: 200px } div + div { color: red }",
            |m| parent_with_text_and_children(m, 4),
            |m, ids| {
                m.remove_node(ids[1]);
                m.remove_node(ids[2]);
            },
        );
    }

    #[test]
    fn inserting_at_the_front_restyles_adjacent_sibling_rules() {
        check_incremental_restyle(
            "div + div { color: red } div > div:nth-child(2) { width: 200px }",
            |m| parent_with_text_and_children(m, 3),
            |m, ids| {
                let new = div(m, &[("class", "a")]);
                m.insert_nodes_before(ids[1], &[new]);
            },
        );
    }
}

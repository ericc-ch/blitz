//! An implementation for Html5ever's sink trait, allowing us to parse HTML into a DOM.
//!
//! Note: the XML path (`XmlTreeSink`) resolves only against the `ericc-ch/html5ever`
//! fork, wired in by tinybrowser's `[patch.crates-io]`. A standalone build of
//! this fork against registry `xml5ever` (which has no `XmlTreeSink`) fails;
//! build and test it through the tinybrowser workspace.

use html5ever::ParseOpts;
use html5ever::tokenizer::TokenizerOpts;
use html5ever::tree_builder::TreeBuilderOpts;
use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell, RefMut};

use blitz_dom::node::Attribute;
use blitz_dom::{DocumentMutator, HtmlParserProvider, NodeId};
use html5ever::{
    QualName,
    tendril::{StrTendril, TendrilSink},
    tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink},
};
use xml5ever::tree_builder::XmlTreeSink;

/// Convert an html5ever Attribute which uses tendril for its value to a blitz Attribute
/// which uses String.
fn html5ever_to_blitz_attr(attr: html5ever::Attribute) -> Attribute {
    Attribute {
        name: attr.name,
        value: attr.value.to_string(),
    }
}

#[derive(Copy, Clone, Default, Debug)]
pub struct HtmlProvider;

impl HtmlParserProvider for HtmlProvider {
    fn parse_inner_html<'m2, 'doc2>(
        &self,
        mutr: &'m2 mut DocumentMutator<'doc2>,
        element_id: NodeId,
        html: &str,
    ) {
        DocumentHtmlParser::parse_inner_html_into_mutator(mutr, element_id, html);
    }

    fn parse_document(
        &self,
        html: &str,
        config: blitz_dom::DocumentConfig,
    ) -> Box<dyn blitz_dom::Document> {
        Box::new(crate::HtmlDocument::from_html(html, config))
    }
}

pub struct DocumentHtmlParser<'m, 'doc> {
    document_mutator: RefCell<&'m mut DocumentMutator<'doc>>,

    /// Errors that occurred during parsing.
    pub errors: RefCell<Vec<Cow<'static, str>>>,

    /// The document's quirks mode.
    pub quirks_mode: Cell<QuirksMode>,
    pub is_xml: bool,
}

impl<'m, 'doc> DocumentHtmlParser<'m, 'doc> {
    #[track_caller]
    /// Get a mutable borrow of the DocumentMutator
    fn mutr(&self) -> RefMut<'_, &'m mut DocumentMutator<'doc>> {
        self.document_mutator.borrow_mut()
    }
}

impl<'m, 'doc> DocumentHtmlParser<'m, 'doc> {
    pub fn new(mutr: &'m mut DocumentMutator<'doc>) -> DocumentHtmlParser<'m, 'doc> {
        DocumentHtmlParser {
            document_mutator: RefCell::new(mutr),
            errors: RefCell::new(Vec::new()),
            quirks_mode: Cell::new(QuirksMode::NoQuirks),
            is_xml: false,
        }
    }

    /// Detects documents without an XML or DOCTYPE declaration whose root `<html>` element
    /// declares the XHTML namespace (e.g. `<html xmlns="http://www.w3.org/1999/xhtml">`)
    fn root_element_has_xhtml_namespace(html: &str) -> bool {
        let rest = html.trim_start_matches('\u{feff}').trim_start();
        let Some(rest) = rest.strip_prefix("<html") else {
            return false;
        };
        let Some(tag_end) = rest.find('>') else {
            return false;
        };
        rest[..tag_end].contains("xmlns=\"http://www.w3.org/1999/xhtml\"")
            || rest[..tag_end].contains("xmlns='http://www.w3.org/1999/xhtml'")
    }

    pub fn parse_into_mutator<'a, 'd>(mutr: &'a mut DocumentMutator<'d>, html: &str) {
        let is_xhtml_doc = html.starts_with("<?xml")
            || html.starts_with("<!DOCTYPE") && {
                let first_line = html.lines().next().unwrap();
                first_line.contains("XHTML") || first_line.contains("xhtml")
            }
            || Self::root_element_has_xhtml_namespace(html);

        if is_xhtml_doc {
            Self::parse_xml_into_mutator(mutr, html);
        } else {
            // Parse as HTML
            let mut sink = DocumentHtmlParser::new(mutr);
            sink.is_xml = false;
            let opts = ParseOpts {
                tokenizer: TokenizerOpts::default(),
                tree_builder: TreeBuilderOpts {
                    exact_errors: false,
                    scripting_enabled: false, // Enables parsing of <noscript> tags
                    iframe_srcdoc: false,
                    // The doctype is a real document child; the sink keeps it.
                    drop_doctype: false,
                    quirks_mode: QuirksMode::NoQuirks,
                },
            };
            html5ever::parse_document(sink, opts)
                .from_utf8()
                .read_from(&mut html.as_bytes())
                .unwrap();
        }
    }

    /// Parse the input as XML (XHTML), regardless of its content.
    ///
    /// [`parse_into_mutator`](Self::parse_into_mutator) sniffs the content to decide between HTML
    /// and XML parsing, but the sniffing cannot detect all XHTML documents (e.g. ones with an
    /// `<!DOCTYPE html>` doctype). Callers which know the document is XHTML from out-of-band
    /// information (a `Content-Type` header or an `.xht`/`.xhtml` file extension) should use
    /// this method instead.
    pub fn parse_xml_into_mutator<'a, 'd>(mutr: &'a mut DocumentMutator<'d>, xml: &str) {
        let mut sink = DocumentHtmlParser::new(mutr);
        sink.is_xml = true;
        // Internal general entities are not part of any parser's token
        // stream: expand the internal subset up front, bounded against
        // billion laughs (see `entity`).
        let (expanded, entity_errors) =
            crate::entity::expand_internal_general_entities_with_errors(xml);
        for error in entity_errors {
            sink.parse_error(Cow::Owned(error));
        }
        xml5ever::driver::parse_document(sink, Default::default())
            .from_utf8()
            .read_from(&mut expanded.as_bytes())
            .unwrap();
    }

    pub fn parse_inner_html_into_mutator<'a, 'd>(
        mutr: &'a mut DocumentMutator<'d>,
        element_id: NodeId,
        html: &str,
    ) {
        let sink = DocumentHtmlParser::new(mutr);

        let opts = ParseOpts {
            tokenizer: TokenizerOpts::default(),
            tree_builder: TreeBuilderOpts {
                exact_errors: false,
                scripting_enabled: false, // Enables parsing of <noscript> tags
                iframe_srcdoc: false,
                drop_doctype: true,
                quirks_mode: QuirksMode::NoQuirks,
            },
        };
        html5ever::driver::parse_fragment_for_element(sink, opts, element_id, false, None)
            .from_utf8()
            .read_from(&mut html.as_bytes())
            .unwrap();

        // html5ever creates a new fragment root node under the document node and parses the nodes into that fragment root.
        // So here we move the children of the fragment root to element_id and then drop the fragment root.
        // A template context receives its children in its template contents
        // instead: fragment parsing never opens the context element, so the
        // template modes have no open template to route into
        // (<https://html.spec.whatwg.org/multipage/parsing.html#parsing-html-fragments>).
        let document_id = mutr.doc.root_node().id;
        let fragment_root_id = mutr.last_child_id(document_id).unwrap();
        let child_ids = mutr.child_ids(fragment_root_id);
        let destination = {
            if mutr.is_template_element(element_id) {
                mutr.ensure_template_contents(element_id)
            } else {
                element_id
            }
        };
        mutr.append_children(destination, &child_ids);
        mutr.remove_and_drop_node(fragment_root_id);
    }

    /// Whether the document already has a document element child.
    fn document_has_element(&self) -> bool {
        let mutr = self.mutr();
        let root = mutr.doc.root_node().id;
        mutr.child_ids(root).iter().any(|child| {
            mutr.doc
                .get_node(*child)
                .is_some_and(|node| node.data.downcast_element().is_some())
        })
    }

    /// Whether the document already has a doctype child: DOM permits at most
    /// one (<https://dom.spec.whatwg.org/#concept-node-ensure-pre-insert-validity>).
    fn document_has_doctype(&self) -> bool {
        let mutr = self.mutr();
        let root = mutr.doc.root_node().id;
        mutr.child_ids(root).iter().any(|child| {
            mutr.doc.get_node(*child).is_some_and(|node| {
                matches!(node.data, blitz_dom::NodeData::Doctype { .. })
            })
        })
    }
}

impl<'m, 'doc> TreeSink for DocumentHtmlParser<'m, 'doc> {
    type Output = ();

    // we use the ID of the nodes in the tree as the handle
    type Handle = NodeId;

    type ElemName<'a>
        = Ref<'a, QualName>
    where
        Self: 'a;

    fn finish(self) -> Self::Output {
        #[cfg(feature = "tracing")]
        for error in self.errors.borrow().iter() {
            tracing::error!("{error}");
        }
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        self.errors.borrow_mut().push(msg.clone());
        // Only XML documents drain into `parsererror`: HTML fragment parses
        // (`innerHTML`) share a live document whose error store must not grow
        // across parses.
        if self.is_xml {
            self.mutr().doc.push_parse_error(msg.into_owned());
        }
    }

    fn get_document(&self) -> Self::Handle {
        self.document_mutator.borrow().doc.root_node().id
    }

    fn elem_name<'a>(&'a self, target: &'a Self::Handle) -> Self::ElemName<'a> {
        Ref::map(self.document_mutator.borrow(), |docm| {
            docm.element_name(*target)
                .expect("TreeSink::elem_name called on a node which is not an element!")
        })
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<html5ever::Attribute>,
        _flags: ElementFlags,
    ) -> Self::Handle {
        let attrs = attrs.into_iter().map(html5ever_to_blitz_attr).collect();
        self.mutr().create_element(name, attrs)
    }

    fn create_comment(&self, text: StrTendril) -> Self::Handle {
        self.mutr().create_comment_node(&text)
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> Self::Handle {
        // The XML declaration looks like a PI to the tokenizer but is not
        // a node. It only occurs before the document element, so a later
        // `<?xml?>` (malformed input with a reserved target) stays a real
        // PI (<https://www.w3.org/TR/xml/#sec-prolog-dtd>). `[Xx][Mm][Ll]`
        // targets are reserved (<https://www.w3.org/TR/xml#sec-pi>), so any
        // case variant before the root is prolog, not a node. Returns the
        // document as a sentinel (never appended: it is the root, not a
        // child) instead of allocating a node just to drop it.
        if target.eq_ignore_ascii_case("xml") && !self.document_has_element() {
            return self.get_document();
        }
        self.mutr()
            .create_processing_instruction_node(&target, &data)
    }

    fn append(&self, parent_id: &Self::Handle, child: NodeOrText<Self::Handle>) {
        let document = self.get_document();
        match child {
            NodeOrText::AppendNode(id) => {
                if id == document {
                    return;
                }
                self.mutr().append_children(*parent_id, &[id])
            }
            // If content to append is text, first attempt to append it to the last child of parent.
            // Else create a new text node and append it to the parent
            NodeOrText::AppendText(text) => {
                let last_child_id = self.mutr().last_child_id(*parent_id);
                let has_appended = if let Some(id) = last_child_id {
                    self.mutr().append_text_to_node(id, &text).is_ok()
                } else {
                    false
                };
                if !has_appended {
                    let new_child_id = self.mutr().create_text_node(&text);
                    self.mutr().append_children(*parent_id, &[new_child_id]);
                }
            }
        }
    }

    // Note: The tree builder promises we won't have a text node after the insertion point.
    // https://github.com/servo/html5ever/blob/main/rcdom/lib.rs#L338
    fn append_before_sibling(&self, sibling_id: &Self::Handle, new_node: NodeOrText<Self::Handle>) {
        let document = self.get_document();
        match new_node {
            NodeOrText::AppendNode(id) => {
                if id == document {
                    return;
                }
                self.mutr().insert_nodes_before(*sibling_id, &[id])
            }
            // If content to append is text, first attempt to append it to the node before sibling_node
            // Else create a new text node and insert it before sibling_node
            NodeOrText::AppendText(text) => {
                let previous_sibling_id = self.mutr().previous_sibling_id(*sibling_id);
                let has_appended = if let Some(id) = previous_sibling_id {
                    self.mutr().append_text_to_node(id, &text).is_ok()
                } else {
                    false
                };
                if !has_appended {
                    let new_child_id = self.mutr().create_text_node(&text);
                    self.mutr()
                        .insert_nodes_before(*sibling_id, &[new_child_id]);
                }
            }
        };
    }

    fn append_based_on_parent_node(
        &self,
        element: &Self::Handle,
        prev_element: &Self::Handle,
        child: NodeOrText<Self::Handle>,
    ) {
        if self.mutr().node_has_parent(*element) {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        // The doctype is a real child of the document
        // (<https://html.spec.whatwg.org/multipage/parsing.html#the-initial-insertion-mode>).
        // DOM permits at most one doctype child: the parser calls once, but
        // guard so a second never lands in the tree
        // (<https://dom.spec.whatwg.org/#concept-node-ensure-pre-insert-validity>).
        if self.document_has_doctype() {
            debug_assert!(false, "second doctype ignored");
            return;
        }
        let doctype = self
            .mutr()
            .create_doctype_node(&name, &public_id, &system_id);
        let document = self.get_document();
        self.mutr().append_children(document, &[doctype]);
    }

    fn get_template_contents(&self, target: &Self::Handle) -> Self::Handle {
        // Template children parse into the template contents fragment, not
        // the element's children
        // (<https://html.spec.whatwg.org/multipage/scripting.html#the-template-element>).
        self.mutr().ensure_template_contents(*target)
    }

    fn same_node(&self, x: &Self::Handle, y: &Self::Handle) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.quirks_mode.set(mode);
    }

    fn add_attrs_if_missing(&self, target: &Self::Handle, attrs: Vec<html5ever::Attribute>) {
        let attrs = attrs.into_iter().map(html5ever_to_blitz_attr).collect();
        self.mutr().add_attrs_if_missing(*target, attrs);
    }

    fn remove_from_parent(&self, target: &Self::Handle) {
        self.mutr().remove_node(*target);
    }

    fn reparent_children(&self, old_parent_id: &Self::Handle, new_parent_id: &Self::Handle) {
        self.mutr()
            .reparent_children(*old_parent_id, *new_parent_id);
    }
}

impl<'m, 'doc> XmlTreeSink for DocumentHtmlParser<'m, 'doc> {
    fn create_cdata_section(&self, contents: StrTendril) -> Self::Handle {
        self.mutr().create_cdata_section_node(&contents)
    }
}

#[test]
fn parses_some_html() {
    use blitz_dom::{BaseDocument, DocumentConfig};

    let html = "<!DOCTYPE html><html><body><h1>hello world</h1></body></html>";
    let mut doc = BaseDocument::new(DocumentConfig::default());
    let mut mutr = doc.mutate();
    let sink = DocumentHtmlParser::new(&mut mutr);

    html5ever::parse_document(sink, Default::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .unwrap();

    drop(mutr);
    doc.print_tree()

    // Now our tree should have some nodes in it
}

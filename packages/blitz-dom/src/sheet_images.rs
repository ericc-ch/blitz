//! Which style sheet declared each image URL.
//!
//! A request a style sheet initiates sends the sheet's URL as its referrer and
//! applies the sheet response's `Referrer-Policy`, not the document's
//! (<https://w3c.github.io/webappsec-referrer-policy/#integration-with-css>).
//!
//! Computed style keeps no declaration provenance — a `background-image` URL
//! computed from an external sheet is indistinguishable from the same URL in an
//! inline `style` attribute — so each sheet's image URLs are recorded against
//! the sheet's location when the sheet is parsed and looked up again when the
//! image is fetched. A URL that no sheet declared, and every URL from an inline
//! `<style>` or `style` attribute, is absent from the map and keeps the
//! document's referrer.

use std::collections::HashMap;

use style::properties::PropertyDeclaration;
use style::shared_lock::SharedRwLockReadGuard;
use style::stylesheets::CssRule;
use style::values::generics::basic_shape::ClipPath;
use style::values::generics::counters::{Content, ContentItem};
use style::values::generics::motion::OffsetPath;
use style::values::generics::svg::SVGPaintKind;
use style::values::specified::image::Image as SpecifiedImage;
use style::values::specified::ui::Cursor;
use style::values::specified::url::SpecifiedUrl;

type Referrers = HashMap<String, url::Url>;

/// Adds every image URL `rules` references as a fetch target to `referrers`,
/// keyed by absolute URL and valued by the declaring sheet's location.
pub(crate) fn record_image_referrers(
    rules: &[CssRule],
    guard: &SharedRwLockReadGuard,
    sheet_url: &url::Url,
    referrers: &mut Referrers,
) {
    for rule in rules {
        match rule {
            CssRule::Style(style) => {
                let style = style.read_with(guard);
                visit_block(&style.block, guard, sheet_url, referrers);
                if let Some(nested) = &style.rules {
                    record_image_referrers(
                        nested.read_with(guard).0.as_slice(),
                        guard,
                        sheet_url,
                        referrers,
                    );
                }
            }
            CssRule::Media(rule) => {
                record_image_referrers(
                    rule.rules.read_with(guard).0.as_slice(),
                    guard,
                    sheet_url,
                    referrers,
                );
            }
            CssRule::Supports(rule) => {
                record_image_referrers(
                    rule.rules.read_with(guard).0.as_slice(),
                    guard,
                    sheet_url,
                    referrers,
                );
            }
            CssRule::Container(rule) => {
                record_image_referrers(
                    rule.rules.read_with(guard).0.as_slice(),
                    guard,
                    sheet_url,
                    referrers,
                );
            }
            CssRule::LayerBlock(rule) => {
                record_image_referrers(
                    rule.rules.read_with(guard).0.as_slice(),
                    guard,
                    sheet_url,
                    referrers,
                );
            }
            CssRule::Scope(rule) => {
                record_image_referrers(
                    rule.rules.read_with(guard).0.as_slice(),
                    guard,
                    sheet_url,
                    referrers,
                );
            }
            CssRule::StartingStyle(rule) => {
                record_image_referrers(
                    rule.rules.read_with(guard).0.as_slice(),
                    guard,
                    sheet_url,
                    referrers,
                );
            }
            CssRule::Page(page) => {
                let page = page.read_with(guard);
                visit_block(&page.block, guard, sheet_url, referrers);
                record_image_referrers(
                    page.rules.read_with(guard).0.as_slice(),
                    guard,
                    sheet_url,
                    referrers,
                );
            }
            // A `@keyframes` step can reference an image, and the animation
            // fetches it when the step is reached
            // (<https://drafts.csswg.org/css-animations/#keyframes>).
            CssRule::Keyframes(keyframes) => {
                for frame in &keyframes.read_with(guard).keyframes {
                    visit_block(&frame.read_with(guard).block, guard, sheet_url, referrers);
                }
            }
            // `@import` sheets arrive as their own `Stylesheet` and record
            // their own URLs; `@font-face` fonts are threaded through the font
            // loader; `@namespace` names no fetchable image.
            _ => {}
        }
    }
}

fn visit_block(
    block: &style::shared_lock::Locked<style::properties::PropertyDeclarationBlock>,
    guard: &SharedRwLockReadGuard,
    sheet_url: &url::Url,
    referrers: &mut Referrers,
) {
    for declaration in block.read_with(guard).declarations() {
        visit_declaration(declaration, sheet_url, referrers);
    }
}

/// Records the images one declaration can fetch.
///
/// This covers every property whose value can name a fetchable image resource:
/// the CSS image properties, `content`, the SVG paint properties and paint
/// servers, and the clip/motion/mask references. `filter: url()` and the marker
/// properties are not generated longhands in this style engine, so no
/// declaration can name an image through them; `image-set()` is not walked
/// either, since no `url()` form reaches it here.
fn visit_declaration(
    declaration: &PropertyDeclaration,
    sheet_url: &url::Url,
    referrers: &mut Referrers,
) {
    match declaration {
        PropertyDeclaration::BackgroundImage(images) => {
            record_images(&images.0, sheet_url, referrers)
        }
        PropertyDeclaration::MaskImage(images) => record_images(&images.0, sheet_url, referrers),
        PropertyDeclaration::ListStyleImage(image)
        | PropertyDeclaration::BorderImageSource(image) => {
            record_images(std::slice::from_ref(image), sheet_url, referrers);
        }
        PropertyDeclaration::Content(content) => {
            if let Content::Items(items) = content {
                // Items past `alt_start` are alternative text for assistive
                // technology, never rendered and never fetched
                // (<https://drafts.csswg.org/css-content/#content-property>).
                for item in &items.items[..items.alt_start] {
                    if let ContentItem::Image(image) = item {
                        record_images(std::slice::from_ref(image), sheet_url, referrers);
                    }
                }
            }
        }
        PropertyDeclaration::Fill(paint) | PropertyDeclaration::Stroke(paint) => {
            if let SVGPaintKind::PaintServer(url) = &paint.kind {
                record_url(url, sheet_url, referrers);
            }
        }
        PropertyDeclaration::ClipPath(ClipPath::Url(url)) => {
            record_url(url, sheet_url, referrers);
        }
        PropertyDeclaration::Cursor(cursor) => {
            record_cursor(cursor, sheet_url, referrers);
        }
        PropertyDeclaration::OffsetPath(OffsetPath::OffsetPath { path, .. }) => {
            if let style::values::generics::motion::OffsetPathFunction::Url(url) = &**path {
                record_url(url, sheet_url, referrers);
            }
        }
        _ => {}
    }
}

fn record_images(images: &[SpecifiedImage], sheet_url: &url::Url, referrers: &mut Referrers) {
    for image in images {
        if let SpecifiedImage::Url(url) = image {
            record_url(url, sheet_url, referrers);
        }
    }
}

fn record_cursor(cursor: &Cursor, sheet_url: &url::Url, referrers: &mut Referrers) {
    let images: &[_] = &cursor.images;
    for image in images {
        record_images(std::slice::from_ref(&image.image), sheet_url, referrers);
    }
}

fn record_url(url: &SpecifiedUrl, sheet_url: &url::Url, referrers: &mut Referrers) {
    let Some(url) = url.url() else {
        return;
    };
    referrers.insert(url.as_str().to_owned(), sheet_url.clone());
}

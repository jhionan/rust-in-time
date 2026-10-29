//! Small, paginated invoice layout using a real embedded Unicode TrueType font.
use super::{Document, expand_home};
use anyhow::{Context, Result, ensure};
use pdf_writer::types::{CidFontType, FontFlags, SystemInfo, UnicodeCmap};
use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref, Str};
use std::collections::BTreeMap;
use ttf_parser::{Face, GlyphId};

const FONT: &[u8] = include_bytes!("../../assets/fonts/Ubuntu-Light.ttf");
const LEFT: f32 = 56.0;
const RIGHT: f32 = 539.0;
const TOP: f32 = 786.0;
const BOTTOM: f32 = 56.0;
const INK: [f32; 3] = [0.12, 0.17, 0.21];
const GRAY: [f32; 3] = [0.40, 0.46, 0.51];
const ACCENT: [f32; 3] = [0.0, 0.36, 0.43];

struct Layout {
    face: Face<'static>,
    pages: Vec<Content>,
    glyphs: BTreeMap<u16, char>,
    y: f32,
    number: String,
}
impl Layout {
    fn new(number: &str) -> Result<Self> {
        Ok(Self {
            face: Face::parse(FONT, 0).context("read embedded invoice font")?,
            pages: vec![Content::new()],
            glyphs: BTreeMap::new(),
            y: TOP,
            number: number.to_owned(),
        })
    }
    fn page(&mut self) -> &mut Content {
        self.pages.last_mut().expect("layout always has a page")
    }
    fn width(&self, text: &str, size: f32) -> Result<f32> {
        let mut width = 0_u64;
        for c in text.chars() {
            let glyph = self.face.glyph_index(c).with_context(|| {
                format!(
                    "invoice font cannot display {c:?} (U+{:04X}); use a supported spelling",
                    c as u32
                )
            })?;
            width += u64::from(self.face.glyph_hor_advance(glyph).unwrap_or(0));
        }
        Ok(width as f32 / f32::from(self.face.units_per_em()) * size)
    }
    fn text(&mut self, text: &str, x: f32, y: f32, size: f32, color: [f32; 3]) -> Result<()> {
        let mut encoded = Vec::with_capacity(text.len() * 2);
        for c in text.chars() {
            let glyph = self.face.glyph_index(c).with_context(|| {
                format!(
                    "invoice font cannot display {c:?} (U+{:04X}); use a supported spelling",
                    c as u32
                )
            })?;
            self.glyphs.entry(glyph.0).or_insert(c);
            encoded.extend_from_slice(&glyph.0.to_be_bytes());
        }
        self.page().set_fill_rgb(color[0], color[1], color[2]);
        self.page()
            .begin_text()
            .set_font(Name(b"F1"), size)
            .next_line(x, y)
            .show(Str(&encoded))
            .end_text();
        Ok(())
    }
    fn right(&mut self, text: &str, x: f32, y: f32, size: f32, color: [f32; 3]) -> Result<()> {
        let width = self.width(text, size)?;
        self.text(text, x - width, y, size, color)
    }
    fn ensure(&mut self, height: f32) -> Result<()> {
        if self.y - height < BOTTOM {
            self.pages.push(Content::new());
            self.y = TOP;
            self.text(
                &format!("Invoice {} - continued", self.number),
                LEFT,
                self.y,
                11.0,
                GRAY,
            )?;
            self.y -= 32.0;
        }
        Ok(())
    }
    fn line(&mut self) -> Result<()> {
        self.ensure(18.0)?;
        let y = self.y;
        self.page()
            .set_stroke_rgb(0.79, 0.83, 0.85)
            .set_line_width(0.6)
            .move_to(LEFT, y)
            .line_to(RIGHT, y)
            .stroke();
        self.y -= 18.0;
        Ok(())
    }
    fn wrap(&self, text: &str, width: f32, size: f32) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        for paragraph in text.split('\n') {
            let mut line = String::new();
            for word in paragraph.split_whitespace() {
                let candidate = if line.is_empty() {
                    word.to_owned()
                } else {
                    format!("{line} {word}")
                };
                if self.width(&candidate, size)? <= width {
                    line = candidate;
                    continue;
                }
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                for c in word.chars() {
                    let mut next = line.clone();
                    next.push(c);
                    if self.width(&next, size)? > width && !line.is_empty() {
                        lines.push(std::mem::take(&mut line));
                    }
                    line.push(c);
                }
            }
            lines.push(line);
        }
        Ok(lines)
    }
    fn paragraph(&mut self, text: &str, size: f32, color: [f32; 3]) -> Result<()> {
        for line in self.wrap(text, RIGHT - LEFT, size)? {
            self.ensure(size + 5.0)?;
            self.text(&line, LEFT, self.y, size, color)?;
            self.y -= size + 5.0;
        }
        Ok(())
    }
    fn columns(&mut self, left: &[String], right: &[String]) -> Result<()> {
        let wrap_column = |items: &[String]| -> Result<Vec<String>> {
            let mut lines = Vec::new();
            for item in items {
                lines.extend(self.wrap(item, 224.0, 10.0)?);
            }
            Ok(lines)
        };
        let l = wrap_column(left)?;
        let r = wrap_column(right)?;
        for i in 0..l.len().max(r.len()) {
            self.ensure(15.0)?;
            if let Some(text) = l.get(i) {
                self.text(text, LEFT, self.y, 10.0, INK)?;
            }
            if let Some(text) = r.get(i) {
                self.text(text, 310.0, self.y, 10.0, INK)?;
            }
            self.y -= 15.0;
        }
        Ok(())
    }
    fn total(&mut self, label: &str, value: &str, large: bool) -> Result<()> {
        let size = if large { 13.0 } else { 10.0 };
        let values = self.wrap(value, 155.0, size)?;
        let labels = self.wrap(label, 175.0, size)?;
        let height = values.len().max(labels.len()) as f32 * (size + 5.0);
        self.ensure(height + 4.0)?;
        for (i, line) in labels.iter().enumerate() {
            self.right(
                line,
                RIGHT - 165.0,
                self.y - i as f32 * (size + 5.0),
                size,
                if large { ACCENT } else { GRAY },
            )?;
        }
        for (i, line) in values.iter().enumerate() {
            self.right(
                line,
                RIGHT,
                self.y - i as f32 * (size + 5.0),
                size,
                if large { ACCENT } else { INK },
            )?;
        }
        self.y -= height + 4.0;
        Ok(())
    }
}
fn money(cents: i64, currency: &str) -> String {
    let magnitude = cents.unsigned_abs();
    let major = (magnitude / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in major.chars().enumerate() {
        if i > 0 && (major.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!(
        "{}{grouped}.{:02} {currency}",
        if cents < 0 { "-" } else { "" },
        magnitude % 100
    )
}
fn quantity(value: i64) -> String {
    if value % 100 == 0 {
        format!("{}", value / 100)
    } else if value % 10 == 0 {
        format!("{}.{:01}", value / 100, value % 100 / 10)
    } else {
        format!("{}.{:02}", value / 100, value % 100)
    }
}
fn tax_line(label: &str, value: &str) -> String {
    if label.is_empty() {
        value.to_owned()
    } else {
        format!("{label} {value}")
    }
}

pub(super) fn render(doc: &Document) -> Result<Vec<u8>> {
    let mut layout = Layout::new(&doc.number)?;
    // As in Go, an absent logo file is optional; a corrupt existing logo is an error.
    let logo = if doc.sender.logo_path.is_empty() {
        None
    } else {
        let path = expand_home(&doc.sender.logo_path)?;
        if path.exists() {
            let decoded = image::ImageReader::open(&path)
                .with_context(|| format!("read logo {}", path.display()))?
                .decode()
                .context("decode invoice logo (PNG or JPEG required)")?;
            ensure!(
                u64::from(decoded.width()) * u64::from(decoded.height()) <= 16_000_000,
                "invoice logo is too large"
            );
            Some(decoded.to_rgba8())
        } else {
            None
        }
    };
    let title_x = if let Some(image) = &logo {
        let scale = (40.0 / image.width() as f32).min(40.0 / image.height() as f32);
        let width = image.width() as f32 * scale;
        let height = image.height() as f32 * scale;
        layout
            .page()
            .save_state()
            .transform([width, 0.0, 0.0, height, LEFT, TOP - 10.0])
            .x_object(Name(b"Logo"))
            .restore_state();
        LEFT + 52.0
    } else {
        LEFT
    };
    layout.text("Invoice", title_x, TOP, 28.0, ACCENT)?;
    layout.right(&doc.number, RIGHT, TOP, 15.0, INK)?;
    layout.right(
        &format!("Issued {}", doc.issue.format("%B %-d, %Y")),
        RIGHT,
        TOP - 20.0,
        10.0,
        GRAY,
    )?;
    layout.y -= 51.0;
    layout.line()?;
    layout.text("Billed to", LEFT, layout.y, 10.0, GRAY)?;
    layout.text("Issued by", 310.0, layout.y, 10.0, GRAY)?;
    layout.y -= 20.0;
    let client = &doc.client;
    let mut left = vec![if client.legal_name.is_empty() {
        doc.company.clone()
    } else {
        client.legal_name.clone()
    }];
    if !client.tax_id.is_empty() {
        left.push(tax_line(&client.tax_id_label, &client.tax_id));
    }
    if !client.email.is_empty() {
        left.push(client.email.clone());
    }
    left.extend(client.address_lines.iter().cloned());
    let mut right = vec![
        doc.sender.legal_name.clone(),
        tax_line(&doc.sender.tax_id_label, &doc.sender.tax_id),
    ];
    right.extend(doc.sender.address_lines.iter().cloned());
    layout.columns(&left, &right)?;
    layout.y -= 25.0;
    layout.ensure(86.0)?;
    layout.text("Product or service", LEFT, layout.y, 10.0, GRAY)?;
    layout.right("Qty", 318.0, layout.y, 10.0, GRAY)?;
    layout.right("Unit price", 403.0, layout.y, 10.0, GRAY)?;
    layout.right("Tax", 438.0, layout.y, 10.0, GRAY)?;
    layout.right("Total", RIGHT, layout.y, 10.0, GRAY)?;
    layout.y -= 12.0;
    layout.line()?;
    let labels = layout.wrap(&doc.label, 205.0, 10.0)?;
    layout.ensure(labels.len() as f32 * 15.0 + 45.0)?;
    layout.right(&quantity(doc.quantity), 318.0, layout.y, 10.0, INK)?;
    layout.right(&money(doc.rate, &doc.currency), 403.0, layout.y, 9.0, INK)?;
    layout.right("-", 438.0, layout.y, 10.0, INK)?;
    layout.right(
        &money(doc.subtotal, &doc.currency),
        RIGHT,
        layout.y,
        10.0,
        INK,
    )?;
    for line in labels {
        layout.text(&line, LEFT, layout.y, 10.0, INK)?;
        layout.y -= 15.0;
    }
    let shown_end = doc
        .to
        .checked_sub_days(chrono::Days::new(1))
        .context("invoice period outside supported range")?;
    let period = format!(
        "{} - {}",
        doc.from.format("%b %-d, %Y"),
        shown_end.format("%b %-d, %Y")
    );
    for line in layout.wrap(&period, 210.0, 8.0)? {
        layout.ensure(12.0)?;
        layout.text(&line, LEFT, layout.y, 8.0, GRAY)?;
        layout.y -= 12.0;
    }
    if doc.advance {
        layout.paragraph("Advance payment", 9.0, GRAY)?;
    }
    layout.y -= 22.0;
    if doc.discount > 0 || doc.applied > 0 {
        layout.total("Subtotal", &money(doc.subtotal, &doc.currency), false)?;
    }
    if doc.applied > 0 {
        let label = if doc.credit_ref.is_empty() {
            "Advance applied".to_owned()
        } else {
            format!("Advance applied ({})", doc.credit_ref)
        };
        layout.total(&label, &money(-doc.applied, &doc.currency), false)?;
    }
    if doc.discount > 0 {
        layout.total("Discount", &money(-doc.discount, &doc.currency), false)?;
    }
    layout.total(
        "Total excluding tax",
        &money(doc.total(), &doc.currency),
        false,
    )?;
    layout.total("Total tax", &money(0, &doc.currency), false)?;
    layout.total("Amount Due", &money(doc.total(), &doc.currency), true)?;
    layout.total("Due by", &doc.due.format("%B %-d, %Y").to_string(), false)?;
    layout.y -= 16.0;
    layout.line()?;
    layout.paragraph("Ways to pay", 14.0, ACCENT)?;
    layout.paragraph(&doc.bank.title, 11.0, INK)?;
    if !doc.bank.subtitle.is_empty() {
        layout.paragraph(&doc.bank.subtitle, 9.0, GRAY)?;
    }
    layout.y -= 8.0;
    for (label, value) in std::iter::once(("Reference", doc.number.as_str())).chain(
        doc.bank
            .fields
            .iter()
            .map(|f| (f.label.as_str(), f.value.as_str())),
    ) {
        let label_lines = layout.wrap(label, 110.0, 9.0)?;
        let value_lines = layout.wrap(value, RIGHT - LEFT - 130.0, 10.0)?;
        for i in 0..label_lines.len().max(value_lines.len()) {
            layout.ensure(15.0)?;
            if let Some(text) = label_lines.get(i) {
                layout.text(text, LEFT, layout.y, 9.0, GRAY)?;
            }
            if let Some(text) = value_lines.get(i) {
                layout.text(text, LEFT + 130.0, layout.y, 10.0, INK)?;
            }
            layout.y -= 15.0;
        }
        layout.y -= 7.0;
    }
    assemble(layout, logo.as_ref())
}

fn assemble(layout: Layout, logo: Option<&image::RgbaImage>) -> Result<Vec<u8>> {
    let mut pdf = Pdf::new();
    let catalog = Ref::new(1);
    let tree = Ref::new(2);
    let font = Ref::new(3);
    let cid = Ref::new(4);
    let descriptor = Ref::new(5);
    let font_file = Ref::new(6);
    let cmap_ref = Ref::new(7);
    let logo_ref = Ref::new(8);
    let mask_ref = Ref::new(9);
    pdf.catalog(catalog).pages(tree);
    let page_refs: Vec<_> = (0..layout.pages.len())
        .map(|i| Ref::new(10 + i as i32 * 2))
        .collect();
    pdf.pages(tree)
        .kids(page_refs.iter().copied())
        .count(i32::try_from(page_refs.len())?);
    pdf.type0_font(font)
        .base_font(Name(b"Ubuntu-Light"))
        .encoding_predefined(Name(b"Identity-H"))
        .descendant_font(cid)
        .to_unicode(cmap_ref);
    let system = SystemInfo {
        registry: Str(b"Adobe"),
        ordering: Str(b"Identity"),
        supplement: 0,
    };
    let mut cid_writer = pdf.cid_font(cid);
    cid_writer
        .subtype(CidFontType::Type2)
        .base_font(Name(b"Ubuntu-Light"))
        .system_info(system)
        .font_descriptor(descriptor)
        .cid_to_gid_map_predefined(Name(b"Identity"));
    let scale = 1000.0 / f32::from(layout.face.units_per_em());
    let mut widths = cid_writer.widths();
    for &glyph in layout.glyphs.keys() {
        widths.consecutive(
            glyph,
            [f32::from(layout.face.glyph_hor_advance(GlyphId(glyph)).unwrap_or(0)) * scale],
        );
    }
    widths.finish();
    cid_writer.finish();
    let bbox = layout.face.global_bounding_box();
    pdf.font_descriptor(descriptor)
        .name(Name(b"Ubuntu-Light"))
        .flags(FontFlags::NON_SYMBOLIC)
        .bbox(Rect::new(
            f32::from(bbox.x_min) * scale,
            f32::from(bbox.y_min) * scale,
            f32::from(bbox.x_max) * scale,
            f32::from(bbox.y_max) * scale,
        ))
        .italic_angle(0.0)
        .ascent(f32::from(layout.face.ascender()) * scale)
        .descent(f32::from(layout.face.descender()) * scale)
        .cap_height(
            f32::from(
                layout
                    .face
                    .capital_height()
                    .unwrap_or(layout.face.ascender()),
            ) * scale,
        )
        .stem_v(80.0)
        .font_file2(font_file);
    pdf.stream(font_file, FONT)
        .pair(Name(b"Length1"), i32::try_from(FONT.len())?);
    let mut cmap = UnicodeCmap::new(Name(b"InvoiceUnicode"), system);
    for (&glyph, &ch) in &layout.glyphs {
        cmap.pair(glyph, ch);
    }
    pdf.stream(cmap_ref, &cmap.finish());
    if let Some(image) = logo {
        let rgb: Vec<u8> = image
            .pixels()
            .flat_map(|p| [p.0[0], p.0[1], p.0[2]])
            .collect();
        let mask: Vec<u8> = image.pixels().map(|p| p.0[3]).collect();
        let mut x = pdf.image_xobject(logo_ref, &rgb);
        x.width(image.width() as i32)
            .height(image.height() as i32)
            .bits_per_component(8)
            .s_mask(mask_ref);
        x.color_space().device_rgb();
        x.finish();
        let mut m = pdf.image_xobject(mask_ref, &mask);
        m.width(image.width() as i32)
            .height(image.height() as i32)
            .bits_per_component(8);
        m.color_space().device_gray();
    }
    for (i, content) in layout.pages.into_iter().enumerate() {
        let page_id = page_refs[i];
        let content_id = Ref::new(page_id.get() + 1);
        let mut page = pdf.page(page_id);
        page.media_box(Rect::new(0.0, 0.0, 595.0, 842.0))
            .parent(tree)
            .contents(content_id);
        let mut resources = page.resources();
        resources.fonts().pair(Name(b"F1"), font);
        if logo.is_some() {
            resources.x_objects().pair(Name(b"Logo"), logo_ref);
        }
        resources.finish();
        page.finish();
        pdf.stream(content_id, &content.finish());
    }
    Ok(pdf.finish())
}

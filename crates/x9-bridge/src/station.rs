//! Next-station proposals: where the agent asks the person to set up next,
//! drawn on the current station's preview so they recognise the spot. A
//! marker ("set up here") on the ground, labels and boxes for what was seen,
//! what is hidden and the landmarks the agent's prose refers to. The bridge
//! draws the picture itself, so the chat, the web UI and a report all show the
//! same one. Coordinates are preview pixels, as for aiming.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use serde::Deserialize;
use serde_json::{json, Value};
use trimble_tzf::GridPoint;

static FONT: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Covered by this station (green).
    Seen,
    /// Not covered: what the next station is for (red).
    Hidden,
    /// Something to walk by (white).
    #[default]
    Landmark,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Marker {
    pub x: i64,
    pub y: i64,
    /// Under "SET UP HERE", e.g. "on the path, ~3 m past the corner".
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub x: i64,
    pub y: i64,
    pub text: String,
    #[serde(default)]
    pub kind: Kind,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Zone {
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub kind: Kind,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Proposal {
    pub marker: Marker,
    /// The agent's directions in prose, by landmarks (shown with the picture).
    pub note: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub boxes: Vec<Zone>,
}

const MAX_ITEMS: usize = 3;
const MAX_TEXT: usize = 80;
/// A landmark's label sits on the picture beside the tripod.
const MAX_LANDMARK: usize = 30;
/// Words that change meaning as the person turns or that need a north the
/// scan doesn't have.
const SIDE_WORDS: &[&str] = &["left", "right", "north", "south", "east", "west", "clockwise", "anticlockwise", "counterclockwise"];

fn side_word(note: &str) -> Option<&'static str> {
    note.split(|c: char| !c.is_alphabetic())
        .find_map(|w| SIDE_WORDS.iter().find(|s| w.eq_ignore_ascii_case(s)).copied())
}

/// "setup", "station 2": a name for the spot, not where it is.
fn only_names_the_setup(label: &str) -> bool {
    label
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .all(|w| w.chars().all(|c| c.is_ascii_digit()) || ["set", "up", "setup", "here", "station", "next", "tripod", "spot"].iter().any(|s| w.eq_ignore_ascii_case(s)))
}

fn text_width(font: &FontRef, size: f32, s: &str) -> f32 {
    let sf = font.as_scaled(PxScale::from(size));
    s.chars().map(|ch| sf.h_advance(font.glyph_id(ch))).sum()
}

/// Where a label's text goes on a canvas `canvas_w` wide: beside its dot at
/// `dot_x`, running away from the tripod at `marker_x` (`(x, anchor_x)`), or
/// `None` when it doesn't fit that side and goes above the dot instead.
fn beside(dot_x: f32, marker_x: f32, width: f32, gap: f32, canvas_w: f32) -> Option<(f32, f32)> {
    if dot_x < marker_x {
        (dot_x - gap - width >= 0.0).then_some((dot_x - gap, 1.0))
    } else {
        (dot_x + gap + width <= canvas_w).then_some((dot_x + gap, 0.0))
    }
}

impl Proposal {
    /// Everything inside a `w × h` preview, short texts, a note.
    pub fn check(&self, w: usize, h: usize) -> Result<(), String> {
        let inside = |x: i64, y: i64| (0..w as i64).contains(&x) && (0..h as i64).contains(&y);
        if !inside(self.marker.x, self.marker.y) {
            return Err(format!("the marker must lie within the {w}x{h} preview"));
        }
        if self.note.trim().is_empty() || self.note.chars().count() > 600 {
            return Err("note: the directions in prose, 1–600 characters".into());
        }
        if self.labels.len() + self.boxes.len() > MAX_ITEMS {
            return Err(format!("at most {MAX_ITEMS} labels and boxes together"));
        }
        if self.marker.label.chars().count() > MAX_TEXT {
            return Err(format!("marker.label: at most {MAX_TEXT} characters"));
        }
        if !self.marker.label.trim().is_empty() && only_names_the_setup(&self.marker.label) {
            return Err("marker.label: the picture already says SET UP HERE; say where on the ground (\"on the pavement, ~5 m past the lamp post\") or leave it empty".into());
        }
        for l in &self.labels {
            if !inside(l.x, l.y) || l.text.trim().is_empty() || l.text.chars().count() > MAX_TEXT {
                return Err(format!("label {:?}: inside the preview, 1–{MAX_TEXT} characters", l.text));
            }
        }
        for b in &self.boxes {
            if !inside(b.x0, b.y0) || !inside(b.x1, b.y1) || b.x0 >= b.x1 || b.y0 >= b.y1 || b.text.chars().count() > MAX_TEXT {
                return Err(format!("box {:?}: x0 < x1 and y0 < y1 inside the preview, text up to {MAX_TEXT} characters", b.text));
            }
        }
        // One view: the tripod and what the person walks past, side by side.
        let reach = w as i64 / 4;
        let near = |x: i64| (x - self.marker.x).abs() <= reach;
        for l in &self.labels {
            if !near(l.x) {
                return Err(format!("label {:?}: within {reach} px of the marker, so the person sees it and the tripod in one view", l.text));
            }
        }
        for b in &self.boxes {
            if !near(b.x0) || !near(b.x1) {
                return Err(format!("box {:?}: within {reach} px of the marker, so the person sees it and the tripod in one view", b.text));
            }
        }
        let landmarks: Vec<&str> = self.labels.iter().filter(|l| l.kind == Kind::Landmark).map(|l| l.text.trim()).collect();
        if landmarks.is_empty() {
            return Err("labels: name one landmark (kind landmark), the thing the person walks past to reach the marker".into());
        }
        if let Some(t) = landmarks.iter().find(|t| t.chars().count() > MAX_LANDMARK) {
            return Err(format!("label {t:?}: name the landmark in a few words (up to {MAX_LANDMARK} characters, \"lamp post\"); where it stands goes in the note"));
        }
        let note = self.note.to_lowercase();
        if !landmarks.iter().any(|t| note.contains(&t.to_lowercase())) {
            return Err(format!("note: say how to reach the marker by the landmark, in the words it is labelled with ({})", landmarks.join(", ")));
        }
        if let Some(word) = side_word(&self.note) {
            return Err(format!("note: {word:?} depends on which way the person faces; direct them by the landmark instead"));
        }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        let kind = |k: Kind| match k {
            Kind::Seen => "seen",
            Kind::Hidden => "hidden",
            Kind::Landmark => "landmark",
        };
        json!({
            "marker": {"x": self.marker.x, "y": self.marker.y, "label": self.marker.label},
            "note": self.note,
            "labels": self.labels.iter().map(|l| json!({"x": l.x, "y": l.y, "text": l.text, "kind": kind(l.kind)})).collect::<Vec<_>>(),
            "boxes": self.boxes.iter().map(|b| json!({"x0": b.x0, "y0": b.y0, "x1": b.x1, "y1": b.y1, "text": b.text, "kind": kind(b.kind)})).collect::<Vec<_>>(),
        })
    }
}

const GREEN: [u8; 3] = [74, 222, 128];
const RED: [u8; 3] = [248, 113, 113];
const AMBER: [u8; 3] = [251, 191, 36];
const WHITE: [u8; 3] = [241, 245, 249];
const PANEL: [u8; 3] = [15, 23, 42];

fn colour(k: Kind) -> [u8; 3] {
    match k {
        Kind::Seen => GREEN,
        Kind::Hidden => RED,
        Kind::Landmark => WHITE,
    }
}

/// An RGB canvas with the few shapes the picture needs.
pub struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

/// The embedded label font.
pub fn font() -> FontRef<'static> {
    FontRef::try_from_slice(FONT).expect("embedded font")
}

impl Canvas {
    pub fn new(w: usize, h: usize, bg: [u8; 3]) -> Canvas {
        Canvas { w, h, px: bg.iter().copied().cycle().take(w * h * 3).collect() }
    }
    pub fn png(&self) -> Vec<u8> {
        crate::measure::png_rgb(self.w, self.h, &self.px)
    }
    pub fn put(&mut self, x: i64, y: i64, c: [u8; 3], a: f32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 3;
        for (k, ck) in c.iter().enumerate() {
            self.px[i + k] = (self.px[i + k] as f32 * (1.0 - a) + *ck as f32 * a).round() as u8;
        }
    }
    fn fill(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, c: [u8; 3]) {
        for y in y0..=y1 {
            for x in x0..=x1 {
                self.put(x, y, c, 1.0);
            }
        }
    }
    pub fn line(&mut self, (x0, y0): (f32, f32), (x1, y1): (f32, f32), c: [u8; 3], width: f32) {
        let n = ((x1 - x0).abs().max((y1 - y0).abs()) as usize).max(1);
        let r = (width / 2.0).max(0.5);
        for i in 0..=n {
            let t = i as f32 / n as f32;
            let (cx, cy) = (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
            for dy in -(r.ceil() as i64)..=(r.ceil() as i64) {
                for dx in -(r.ceil() as i64)..=(r.ceil() as i64) {
                    if ((dx * dx + dy * dy) as f32) <= r * r {
                        self.put(cx.round() as i64 + dx, cy.round() as i64 + dy, c, 1.0);
                    }
                }
            }
        }
    }
    fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: [u8; 3], width: f32) {
        self.line((x0, y0), (x1, y0), c, width);
        self.line((x1, y0), (x1, y1), c, width);
        self.line((x1, y1), (x0, y1), c, width);
        self.line((x0, y1), (x0, y0), c, width);
    }
    pub fn ellipse(&mut self, (cx, cy): (f32, f32), (rx, ry): (f32, f32), c: [u8; 3], width: f32, filled: bool) {
        let steps = 180;
        let mut prev = (cx + rx, cy);
        for i in 1..=steps {
            let a = i as f32 / steps as f32 * std::f32::consts::TAU;
            let p = (cx + rx * a.cos(), cy + ry * a.sin());
            self.line(prev, p, c, width);
            prev = p;
        }
        if filled {
            for y in (cy - ry) as i64..=(cy + ry) as i64 {
                for x in (cx - rx) as i64..=(cx + rx) as i64 {
                    let (u, v) = ((x as f32 - cx) / rx, (y as f32 - cy) / ry);
                    if u * u + v * v <= 1.0 {
                        self.put(x, y, c, 1.0);
                    }
                }
            }
        }
    }
    /// Text on a dark panel; `anchor_x`: 0 left, 0.5 middle, 1 right of `x`.
    pub fn text(&mut self, font: &FontRef, size: f32, (x, y): (f32, f32), anchor_x: f32, s: &str, c: [u8; 3]) {
        let sf = font.as_scaled(PxScale::from(size));
        let width = text_width(font, size, s);
        let (ascent, descent) = (sf.ascent(), sf.descent());
        let pad = (size * 0.3).round();
        // Slide inwards rather than run off an edge.
        let left = (x - width * anchor_x).min(self.w as f32 - width - pad - 2.0).max(pad + 2.0);
        self.fill((left - pad) as i64, (y - pad * 0.6) as i64, (left + width + pad) as i64, (y + ascent - descent + pad * 0.6) as i64, PANEL);
        let mut pen = left;
        for ch in s.chars() {
            let id = font.glyph_id(ch);
            let g = id.with_scale_and_position(PxScale::from(size), ab_glyph::point(pen, y + ascent));
            if let Some(o) = font.outline_glyph(g) {
                let b = o.px_bounds();
                o.draw(|gx, gy, cov| self.put(b.min.x as i64 + gx as i64, b.min.y as i64 + gy as i64, c, cov));
            }
            pen += sf.h_advance(id);
        }
    }
}

/// The preview (grey, `pw × ph`) with the proposal drawn on it: cropped to the
/// annotated part (padded) and enlarged so text reads well. With the `ground`
/// under the marker, the tripod is drawn its real size there and casts a
/// shadow on the scene. PNG bytes.
pub fn render(grey: &[u8], pw: usize, ph: usize, p: &Proposal, ground: Option<Ground>) -> Vec<u8> {
    draw(grey, pw, ph, p, ground).0.png()
}

/// The canvas `render` encodes, its enlargement over the preview, and where
/// on it the marker is.
fn draw(grey: &[u8], pw: usize, ph: usize, p: &Proposal, ground: Option<Ground>) -> (Canvas, f32, (f32, f32)) {
    let font = FontRef::try_from_slice(FONT).expect("embedded font");
    // The tripod's top, in preview pixels above the marker.
    let rise = ground.map_or(60, |g| {
        let t = Tripod { g, origin: (0.0, 0.0), scale: 1.0 };
        (-t.at(0.0, 0.0, TRIPOD_M).1).clamp(60.0, ph as f32) as i64
    });
    // The region the proposal is about.
    let (mut x0, mut y0, mut x1, mut y1) = (p.marker.x, p.marker.y - rise, p.marker.x, p.marker.y + 40);
    for l in &p.labels {
        x0 = x0.min(l.x);
        x1 = x1.max(l.x);
        y0 = y0.min(l.y);
        y1 = y1.max(l.y);
    }
    for b in &p.boxes {
        x0 = x0.min(b.x0);
        x1 = x1.max(b.x1);
        y0 = y0.min(b.y0);
        y1 = y1.max(b.y1);
    }
    let pad = 120;
    let (x0, x1) = ((x0 - pad).max(0), (x1 + pad).min(pw as i64 - 1));
    let (y0, y1) = ((y0 - pad).max(0), (y1 + pad).min(ph as i64 - 1));
    let (cw, ch) = ((x1 - x0 + 1) as usize, (y1 - y0 + 1) as usize);
    let s = (2000.0 / cw as f32).clamp(1.0, 2.0);
    let (w, h) = ((cw as f32 * s) as usize, (ch as f32 * s) as usize);
    // Nearest-neighbour enlargement of the grey crop.
    let mut c = Canvas { w, h, px: vec![0; w * h * 3] };
    for y in 0..h {
        let sy = (y0 as usize + (y as f32 / s) as usize).min(ph - 1);
        for x in 0..w {
            let sx = (x0 as usize + (x as f32 / s) as usize).min(pw - 1);
            let g = grey[sy * pw + sx];
            let i = (y * w + x) * 3;
            c.px[i..i + 3].copy_from_slice(&[g, g, g]);
        }
    }
    let at = |x: i64, y: i64| ((x - x0) as f32 * s, (y - y0) as f32 * s);
    let tripod = ground.map(|g| Tripod { g, origin: at(p.marker.x, p.marker.y), scale: s });
    if let Some(t) = &tripod {
        cast(t, w, h).onto(&mut c);
    }
    let size = (14.0 * s).clamp(16.0, 28.0);
    let stroke = (2.0 * s).max(2.0);
    for b in &p.boxes {
        let ((ax, ay), (bx, by)) = (at(b.x0, b.y0), at(b.x1, b.y1));
        c.rect(ax, ay, bx, by, colour(b.kind), stroke);
        if !b.text.is_empty() {
            c.text(&font, size, (ax, ay - size * 1.6), 0.0, &b.text, colour(b.kind));
        }
    }
    for l in &p.labels {
        let (lx, ly) = at(l.x, l.y);
        c.ellipse((lx, ly), (stroke * 1.5, stroke * 1.5), colour(l.kind), 1.0, true);
        let (gap, pad) = (stroke * 3.0, size * 0.3 + 2.0);
        match beside(lx, at(p.marker.x, 0).0, text_width(&font, size, &l.text) + pad * 2.0, gap, w as f32) {
            Some((x, a)) => c.text(&font, size, (x - pad * (a * 2.0 - 1.0), ly - size * 0.6), a, &l.text, colour(l.kind)),
            None => {
                let a = if l.x < p.marker.x { 1.0 } else { 0.0 };
                c.text(&font, size, (lx, ly - gap - size * 1.8), a, &l.text, colour(l.kind))
            }
        }
    }
    let (mx, my) = at(p.marker.x, p.marker.y);
    let below = match &tripod {
        // A tripod its real size where it will stand, on a ring on the ground.
        Some(t) => {
            let ring: Vec<(f32, f32)> = (0..=72).map(|k| t.ground_ring(RING_M, k as f64 / 72.0)).collect();
            for (a, b) in ring.iter().zip(&ring[1..]) {
                c.line(*a, *b, AMBER, 1.5);
            }
            let (head, top) = (t.at(0.0, 0.0, HEAD_M), t.at(0.0, 0.0, TRIPOD_M));
            let leg = t.across_px(0.04).max(1.5);
            for k in 0..3 {
                let foot = t.foot(k);
                c.line(foot, head, PANEL, leg + 2.0);
                c.line(foot, head, AMBER, leg);
            }
            // The scanner: an upright box on the head.
            let half = t.across_px(0.11).max(2.0);
            let (y0, y1) = (top.1.min(head.1), top.1.max(head.1).max(top.1.min(head.1) + 2.0 * half));
            c.fill((head.0 - half - 1.0) as i64, (y0 - 1.0) as i64, (head.0 + half + 1.0) as i64, (y1 + 1.0) as i64, PANEL);
            c.fill((head.0 - half) as i64, y0 as i64, (head.0 + half) as i64, y1 as i64, AMBER);
            let lowest = ring.iter().chain(&[t.shadow(0.0, 0.0, TRIPOD_M)]).map(|p| p.1).fold(my, f32::max);
            lowest - my + size * 0.9
        }
        // The scene isn't known: a symbol for a tripod.
        None => {
            let r = 20.0 * s.max(1.2);
            c.ellipse((mx, my), (r, r * 0.42), AMBER, stroke * 1.4, false);
            for dx in [-0.65f32, 0.0, 0.65] {
                c.line((mx, my - r * 1.75), (mx + dx * r, my - 1.0), AMBER, stroke * 1.2);
            }
            c.ellipse((mx, my - r * 2.05), (r * 0.28, r * 0.28), AMBER, 1.0, true);
            r * 0.75
        }
    };
    c.text(&font, size * 1.25, (mx, my + below), 0.5, "SET UP HERE", AMBER);
    if !p.marker.label.is_empty() {
        c.text(&font, size, (mx, my + below + size * 2.3), 0.5, &p.marker.label, WHITE);
    }
    (c, s, (mx, my))
}

/// Ground to the top of the scanner on its tripod (m), to the tripod's head,
/// the legs' spread from the centre, and the ring drawn round it.
const TRIPOD_M: f64 = 1.65;
const HEAD_M: f64 = 1.3;
const LEG_SPREAD_M: f64 = 0.45;
const RING_M: f64 = 0.7;
/// The shadow's offset on the ground per metre of height: along the view
/// (away from the person looking) and across it, a sun ~40° up and off to
/// one side. Across, because ground seen from afar is foreshortened along
/// the view: a shadow cast away would flatten to nothing.
const SUN: (f64, f64) = (0.35, 1.1);
/// How dark a full shadow makes the ground.
const SHADOW: f32 = 0.6;

/// Where the marker stands in the scan: the ground there (metres out from
/// the scanner, and below it) and how many preview pixels a radian spans
/// there, across (azimuth) and down the picture (elevation; negative when up
/// the scan is up the picture).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ground {
    pub out_m: f64,
    pub below_m: f64,
    pub px_per_rad: (f64, f64),
}

/// The ground under preview pixel `at`, from the scan's points (`grid` its
/// columns and rows, `preview` its preview's size): `None` where the scan saw
/// nothing there or nothing below the scanner.
pub fn ground_at(pts: &[GridPoint], grid: (u32, u32), preview: (usize, usize), at: (i64, i64)) -> Option<Ground> {
    let (pw, ph) = (preview.0 as f64, preview.1 as f64);
    if pw == 0.0 || ph == 0.0 || grid.0 == 0 || grid.1 == 0 {
        return None;
    }
    let (fx, fy) = (pw / grid.0 as f64, ph / grid.1 as f64);
    let wrap = |d: f64, w: f64| (d + w / 2.0).rem_euclid(w) - w / 2.0;
    let pi = std::f64::consts::PI;
    // (dx, dy) from the marker in preview pixels; (azimuth, elevation, out, z).
    type Near = ((f64, f64), (f64, f64, f64, f64));
    let near: Vec<Near> = pts
        .iter()
        .filter_map(|g| {
            let (x, y, z) = (g.point.x as f64, g.point.y as f64, g.point.z as f64);
            let out = x.hypot(y);
            let d = (wrap(g.col as f64 * fx - at.0 as f64, pw), g.row as f64 * fy - at.1 as f64);
            (out > 0.0 && d.0.abs() <= 12.0 && d.1.abs() <= 12.0).then(|| (d, (y.atan2(x), z.atan2(out), out, z)))
        })
        .collect();
    let mut here: Vec<(f64, f64)> = near.iter().filter(|(d, _)| d.0.hypot(d.1) <= 2.5).map(|(_, p)| (p.2, p.3)).collect();
    if here.len() < 3 || near.len() < 12 {
        return None;
    }
    let mid = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    };
    let (out_m, z) = (mid(&mut here.iter().map(|h| h.0).collect()), mid(&mut here.iter_mut().map(|h| h.1).collect()));
    if z >= 0.0 {
        return None;
    }
    let az0 = near[0].1 .0;
    // Pixels per radian: the least-squares slope of pixel offset on angle.
    let slope = |pairs: &mut dyn Iterator<Item = (f64, f64)>| {
        let v: Vec<(f64, f64)> = pairs.collect();
        let n = v.len() as f64;
        let (ma, mp) = (v.iter().map(|p| p.0).sum::<f64>() / n, v.iter().map(|p| p.1).sum::<f64>() / n);
        let (sap, saa) = v.iter().fold((0.0, 0.0), |(sap, saa), &(a, p)| (sap + (a - ma) * (p - mp), saa + (a - ma) * (a - ma)));
        (saa > 1e-12).then(|| sap / saa)
    };
    let kx = slope(&mut near.iter().map(|(d, p)| (wrap(p.0 - az0, 2.0 * pi), d.0)))?;
    let ky = slope(&mut near.iter().map(|(d, p)| (p.1, d.1)))?;
    (kx.abs() > 1.0 && ky.abs() > 1.0).then_some(Ground { out_m, below_m: -z, px_per_rad: (kx, ky) })
}

/// Places on the ground round the marker, and above it, on the canvas.
struct Tripod {
    g: Ground,
    origin: (f32, f32),
    scale: f32,
}

impl Tripod {
    /// `along` metres further out than the marker, `across` to the side, `up`
    /// above the ground.
    fn at(&self, along: f64, across: f64, up: f64) -> (f32, f32) {
        let (out, z) = (self.g.out_m + along, -self.g.below_m + up);
        let daz = across.atan2(out);
        let del = z.atan2(out.hypot(across)) - (-self.g.below_m).atan2(self.g.out_m);
        (self.origin.0 + (self.g.px_per_rad.0 * daz) as f32 * self.scale, self.origin.1 + (self.g.px_per_rad.1 * del) as f32 * self.scale)
    }
    fn ground_ring(&self, r: f64, t: f64) -> (f32, f32) {
        let a = t * std::f64::consts::TAU;
        self.at(r * a.cos(), r * a.sin(), 0.0)
    }
    fn foot(&self, k: usize) -> (f32, f32) {
        self.ground_ring(LEG_SPREAD_M, 0.25 + k as f64 / 3.0)
    }
    /// Canvas pixels `m` metres across at the marker.
    fn across_px(&self, m: f64) -> f32 {
        (self.g.px_per_rad.0.abs() * m / self.g.out_m) as f32 * self.scale
    }
    /// Where something `up` metres above `(along, across)` casts its shadow.
    fn shadow(&self, along: f64, across: f64, up: f64) -> (f32, f32) {
        self.at(along + up * SUN.0, across + up * SUN.1, 0.0)
    }
}

/// How much of the ground each canvas pixel's shadow covers, 0 to 1.
struct Shade {
    w: usize,
    h: usize,
    a: Vec<f32>,
}

impl Shade {
    fn disc(&mut self, (cx, cy): (f32, f32), (rx, ry): (f32, f32), v: f32) {
        let (rx, ry) = (rx.max(0.5), ry.max(0.5));
        for y in (cy - ry).floor() as i64..=(cy + ry).ceil() as i64 {
            for x in (cx - rx).floor() as i64..=(cx + rx).ceil() as i64 {
                let (u, w) = ((x as f32 - cx) / rx, (y as f32 - cy) / ry);
                if u * u + w * w <= 1.0 && x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
                    let i = y as usize * self.w + x as usize;
                    self.a[i] = self.a[i].max(v);
                }
            }
        }
    }
    fn line(&mut self, a: (f32, f32), b: (f32, f32), width: f32, v: f32) {
        let n = ((b.0 - a.0).abs().max((b.1 - a.1).abs()).ceil() as usize).max(1);
        for k in 0..=n {
            let t = k as f32 / n as f32;
            self.disc((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t), (width / 2.0, width / 2.0), v);
        }
    }
    /// Soften the edges: two box blurs, near enough a Gaussian (a third
    /// fades a shadow seen edge-on, a pixel or two tall, to nothing).
    fn blur(&mut self, r: usize) {
        for _ in 0..2 {
            for (len, stride, lines, step) in [(self.w, 1, self.h, self.w), (self.h, self.w, self.w, 1)] {
                let mut line = vec![0f32; len];
                for l in 0..lines {
                    let base = l * step;
                    let mut sum = 0f32;
                    let get = |a: &[f32], i: i64| if i < 0 || i as usize >= len { 0.0 } else { a[base + i as usize * stride] };
                    for i in -(r as i64)..=(r as i64) {
                        sum += get(&self.a, i);
                    }
                    for (i, out) in line.iter_mut().enumerate() {
                        *out = sum / (2 * r + 1) as f32;
                        sum += get(&self.a, i as i64 + r as i64 + 1) - get(&self.a, i as i64 - r as i64);
                    }
                    for (i, v) in line.iter().enumerate() {
                        self.a[base + i * stride] = *v;
                    }
                }
            }
        }
    }
    /// Darken the picture under the shadow, so it falls on the real ground.
    fn onto(&self, c: &mut Canvas) {
        for (i, a) in self.a.iter().enumerate() {
            let k = 1.0 - SHADOW * a.clamp(0.0, 1.0);
            for p in &mut c.px[i * 3..i * 3 + 3] {
                *p = (*p as f32 * k).round() as u8;
            }
        }
    }
}

/// The tripod's shadow on the ground round it: dark under its feet, the legs
/// and scanner cast away from the sun, soft at the edges.
fn cast(t: &Tripod, w: usize, h: usize) -> Shade {
    let mut sh = Shade { w, h, a: vec![0.0; w * h] };
    let (o, rx, ry) = (t.at(0.0, 0.0, 0.0), t.at(0.0, LEG_SPREAD_M, 0.0), t.at(LEG_SPREAD_M, 0.0, 0.0));
    // Seen from afar the ground is nearly edge-on; the patch under the feet
    // stays a few pixels tall so it still grounds the tripod.
    sh.disc(o, ((rx.0 - o.0).abs() * 1.6, ((ry.1 - o.1).abs() * 1.6).max(3.0)), 0.8);
    let head = t.shadow(0.0, 0.0, HEAD_M);
    let leg = t.across_px(0.06).max(3.0);
    for k in 0..3 {
        sh.line(t.foot(k), head, leg, 1.0);
    }
    sh.line(head, t.shadow(0.0, 0.0, TRIPOD_M), t.across_px(0.24).max(3.0), 1.0);
    let tall = (t.at(0.0, 0.0, TRIPOD_M).1 - o.1).abs();
    sh.blur(((tall * 0.04) as usize).clamp(1, 12));
    sh
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal() -> Proposal {
        serde_json::from_value(json!({
            "marker": {"x": 300, "y": 200, "label": "on the path, ~3 m past the shed"},
            "note": "Carry the tripod past the shed door, onto the path.",
            "labels": [{"x": 250, "y": 120, "text": "Shed door", "kind": "landmark"}],
            "boxes": [{"x0": 260, "y0": 80, "x1": 330, "y1": 140, "text": "Hidden: the far wall", "kind": "hidden"}]
        }))
        .unwrap()
    }

    #[test]
    fn checks_keep_it_inside_and_short() {
        let p = proposal();
        assert!(p.check(400, 300).is_ok());
        assert!(p.check(250, 300).unwrap_err().contains("marker"));
        let mut q = proposal();
        q.note = " ".into();
        assert!(q.check(400, 300).unwrap_err().contains("note"));
        let mut q = proposal();
        q.boxes[0].x1 = 240;
        assert!(q.check(400, 300).unwrap_err().contains("x0 < x1"));
    }

    #[test]
    fn checks_keep_the_tripod_and_its_landmark_in_one_view() {
        let mut q = proposal();
        q.labels[0].x = 100;
        assert!(q.check(400, 300).unwrap_err().contains("one view"));
        let mut q = proposal();
        q.labels[0].kind = Kind::Seen;
        assert!(q.check(400, 300).unwrap_err().contains("name one landmark"));
        let mut q = proposal();
        q.note = "Carry the tripod onto the path.".into();
        assert!(q.check(400, 300).unwrap_err().contains("Shed door"));
        let mut q = proposal();
        q.note = "Carry the tripod past the shed door and turn left onto the path.".into();
        assert!(q.check(400, 300).unwrap_err().contains("\"left\""));
        let mut q = proposal();
        q.note = "Carry the tripod past the shed door; it's alright on the path.".into();
        assert!(q.check(400, 300).is_ok(), "a side word inside another word is fine");
        let mut q = proposal();
        for t in ["Gate", "Bench", "Hose"] {
            q.labels.push(Label { x: 290, y: 150, text: t.into(), kind: Kind::Landmark });
        }
        assert!(q.check(400, 300).unwrap_err().contains("at most 3"));
    }

    #[test]
    fn the_marker_label_says_where_not_what() {
        for name in ["setup", "Station 2", "SET UP HERE", "next station", "tripod #2"] {
            let mut q = proposal();
            q.marker.label = name.into();
            assert!(q.check(400, 300).unwrap_err().contains("SET UP HERE"), "{name}");
        }
        let mut q = proposal();
        q.marker.label = "on the pavement, ~5 m past the lamp post".into();
        assert!(q.check(400, 300).is_ok());
        q.marker.label.clear();
        assert!(q.check(400, 300).is_ok());
    }

    #[test]
    fn a_landmark_is_named_in_a_few_words() {
        let mut q = proposal();
        q.labels[0].text = "lamp post in front of the evergreen".into();
        q.note = "Carry the tripod past the lamp post in front of the evergreen.".into();
        assert!(q.check(400, 300).unwrap_err().contains("a few words"));
        q.labels[0].text = "lamp post".into();
        assert!(q.check(400, 300).is_ok());
        q.labels[0].text = "x".repeat(MAX_LANDMARK);
        q.note = format!("Carry the tripod past the {}.", q.labels[0].text);
        assert!(q.check(400, 300).is_ok());
    }

    #[test]
    fn a_label_beside_the_tripod_reads_away_from_it() {
        // Left of the tripod with room: ends just left of the dot.
        assert_eq!(beside(250.0, 300.0, 100.0, 6.0, 600.0), Some((244.0, 1.0)));
        // Right of (or on) the tripod with room: starts just right of the dot.
        assert_eq!(beside(350.0, 300.0, 100.0, 6.0, 600.0), Some((356.0, 0.0)));
        assert_eq!(beside(300.0, 300.0, 100.0, 6.0, 600.0), Some((306.0, 0.0)));
        // No room on the side away from the tripod: above the dot instead.
        assert_eq!(beside(90.0, 300.0, 100.0, 6.0, 600.0), None);
        assert_eq!(beside(520.0, 300.0, 100.0, 6.0, 600.0), None);
        assert_eq!(beside(106.0, 300.0, 100.0, 6.0, 600.0), Some((100.0, 1.0)));
        assert_eq!(beside(494.0, 300.0, 100.0, 6.0, 600.0), Some((500.0, 0.0)));
    }

    /// Flat ground 1.6 m below the scanner, seen on a 4000 × 1000 grid: 360°
    /// across, +70° to -70° down, a 2000 × 500 preview.
    fn flat_dome() -> (Vec<GridPoint>, (u32, u32), (usize, usize)) {
        let (gc, gr) = (4000u32, 1000u32);
        let mut v = Vec::new();
        for row in (0..gr).step_by(2) {
            let el = (70.0 - 140.0 * row as f64 / gr as f64).to_radians();
            if el >= -0.01 {
                continue;
            }
            let out = (1.6 / (-el).tan()).min(120.0);
            for col in (0..gc).step_by(2) {
                let az = col as f64 / gc as f64 * std::f64::consts::TAU;
                let point = trimble_tzf::Point { x: (out * az.cos()) as f32, y: (out * az.sin()) as f32, z: -1.6, intensity: 0.0 };
                v.push(GridPoint { point, row, col });
            }
        }
        (v, (gc, gr), (2000, 500))
    }

    /// The preview pixel of the ground `out` metres away at azimuth `az`.
    fn ground_px(out: f64, az: f64) -> (i64, i64) {
        let el = (-1.6f64).atan2(out).to_degrees();
        ((az / std::f64::consts::TAU * 2000.0).round() as i64, ((70.0 - el) / 140.0 * 500.0).round() as i64)
    }

    #[test]
    fn the_ground_under_the_marker_comes_from_the_scan() {
        let (pts, grid, preview) = flat_dome();
        for out in [6.0, 20.0, 38.0] {
            let g = ground_at(&pts, grid, preview, ground_px(out, 1.0)).expect("ground");
            assert!((g.out_m - out).abs() < 0.08 * out, "{g:?} at {out} m");
            assert!((g.below_m - 1.6).abs() < 0.01, "{g:?}");
            let (kx, ky) = (2000.0 / std::f64::consts::TAU, -500.0 / 140f64.to_radians());
            assert!((g.px_per_rad.0 / kx - 1.0).abs() < 0.03 && (g.px_per_rad.1 / ky - 1.0).abs() < 0.03, "{g:?}");
        }
        // The sky: nothing there.
        assert_eq!(ground_at(&pts, grid, preview, (500, 40)), None);
        // Across the seam the fit still holds.
        let g = ground_at(&pts, grid, preview, ground_px(15.0, 0.0)).expect("seam");
        assert!((g.px_per_rad.0 / (2000.0 / std::f64::consts::TAU) - 1.0).abs() < 0.03, "{g:?}");
    }

    /// How wide the tripod (amber, above the marker's row) stands on the
    /// drawn canvas, and how far above the marker it reaches.
    fn amber_extent(c: &Canvas, marker: (f32, f32)) -> (usize, usize) {
        let (mut x0, mut x1, mut top) = (usize::MAX, 0, usize::MAX);
        for y in 0..(marker.1 as usize).min(c.h) {
            for x in 0..c.w {
                let i = (y * c.w + x) * 3;
                if c.px[i..i + 3] == AMBER {
                    (x0, x1, top) = (x0.min(x), x1.max(x), top.min(y));
                }
            }
        }
        (x1.saturating_sub(x0), (marker.1 as usize).saturating_sub(top))
    }

    fn bare(x: i64, y: i64) -> Proposal {
        serde_json::from_value(json!({"marker": {"x": x, "y": y}, "note": "n", "labels": [], "boxes": []})).unwrap()
    }

    #[test]
    fn a_tripod_further_off_is_drawn_smaller() {
        let (pw, ph) = (2000usize, 500usize);
        let grey = vec![120u8; pw * ph];
        let (kx, ky) = (pw as f64 / std::f64::consts::TAU, -(ph as f64) / 140f64.to_radians());
        let at = |out: f64| {
            let (x, y) = ground_px(out, 1.0);
            let g = Ground { out_m: out, below_m: 1.6, px_per_rad: (kx, ky) };
            let (c, _, m) = draw(&grey, pw, ph, &bare(x, y), Some(g));
            amber_extent(&c, m)
        };
        let (near, far) = (at(10.0), at(40.0));
        // The tripod's height in the picture goes as one over the distance.
        let ratio = near.1 as f64 / far.1 as f64;
        assert!((2.8..=5.0).contains(&ratio), "near {near:?}, far {far:?}");
        // And it is about its real size: 1.65 m at 10 m is ~0.165 rad.
        let s = draw(&grey, pw, ph, &bare(0, 0), None).1;
        let want = 0.165 * ky.abs() * s as f64;
        assert!((near.1 as f64 / want - 1.0).abs() < 0.25, "{near:?} vs {want}");
    }

    #[test]
    fn the_tripod_casts_a_soft_shadow_on_the_ground() {
        let (pw, ph) = (2000usize, 500usize);
        let grey = vec![120u8; pw * ph];
        let (kx, ky) = (pw as f64 / std::f64::consts::TAU, -(ph as f64) / 140f64.to_radians());
        let (x, y) = ground_px(8.0, 1.0);
        let (c, _, _) = draw(&grey, pw, ph, &bare(x, y), Some(Ground { out_m: 8.0, below_m: 1.6, px_per_rad: (kx, ky) }));
        let grey_px: Vec<u8> = c.px.chunks(3).filter(|p| p[0] == p[1] && p[1] == p[2]).map(|p| p[0]).collect();
        let dark = grey_px.iter().filter(|&&g| g < 120).count();
        let darkest = *grey_px.iter().min().unwrap();
        assert!(dark > 200, "{dark} shadowed pixels");
        assert!((40..110).contains(&darkest), "a shadow darkens, never blacks out: {darkest}");
        // Soft: shades in between, not one flat tone.
        let tones: std::collections::BTreeSet<u8> = grey_px.iter().copied().filter(|&g| g < 120 && g > darkest).collect();
        assert!(tones.len() >= 8, "{} tones", tones.len());
        // Without the scene there is no shadow to cast.
        let (c, _, _) = draw(&grey, pw, ph, &bare(x, y), None);
        // (Text edges on their dark panels can come out grey too, far darker.)
        let odd: std::collections::BTreeSet<u8> = c.px.chunks(3).filter(|p| p[0] == p[1] && p[1] == p[2] && (60..120).contains(&p[0])).map(|p| p[0]).collect();
        assert!(odd.is_empty(), "{odd:?}");
    }

    /// Draws a proposal on a real scan: X9_DRAW_TZF, X9_DRAW_JPG and
    /// X9_DRAW_JSON (the proposal) to X9_DRAW_OUT (PNG).
    #[test]
    #[ignore]
    fn draws_a_real_proposal() {
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let t = trimble_tzf::Tzf::open(std::path::Path::new(&var("X9_DRAW_TZF"))).unwrap();
        let meta = t.scan_meta().unwrap();
        let pts = t.grid_points().unwrap();
        let (pw, ph, grey) = crate::aim::jpeg_luma(&std::fs::read(var("X9_DRAW_JPG")).unwrap()).unwrap();
        let p: Proposal = serde_json::from_slice(&std::fs::read(var("X9_DRAW_JSON")).unwrap()).unwrap();
        let g = ground_at(&pts, (meta.grid_cols, meta.grid_rows), (pw, ph), (p.marker.x, p.marker.y));
        eprintln!("{g:?}");
        std::fs::write(var("X9_DRAW_OUT"), render(&grey, pw, ph, &p, g)).unwrap();
        if let Ok(flat) = std::env::var("X9_DRAW_FLAT") {
            std::fs::write(flat, render(&vec![150u8; pw * ph], pw, ph, &p, g)).unwrap();
        }
    }

    #[test]
    fn odd_scenes_still_draw() {
        let (pw, ph) = (400usize, 300usize);
        let grey = vec![90u8; pw * ph];
        for g in [
            Ground { out_m: 0.4, below_m: 1.6, px_per_rad: (60.0, -120.0) },
            Ground { out_m: 300.0, below_m: 1.6, px_per_rad: (60.0, -120.0) },
            Ground { out_m: 12.0, below_m: 0.1, px_per_rad: (-60.0, 120.0) },
        ] {
            assert_eq!(&draw(&grey, pw, ph, &proposal(), Some(g)).0.png()[1..4], b"PNG", "{g:?}");
        }
    }

    #[test]
    fn draws_a_png_with_the_marker_on_it() {
        let (pw, ph) = (400usize, 300usize);
        let grey = vec![90u8; pw * ph];
        let png = render(&grey, pw, ph, &proposal(), None);
        assert_eq!(&png[1..4], b"PNG");
        // A label at the far right edge still draws (slid inwards), without panicking.
        let mut edge = proposal();
        edge.labels.push(Label { x: 399, y: 10, text: "Hidden: everything past this corner".into(), kind: Kind::Hidden });
        assert_eq!(&render(&grey, pw, ph, &edge, None)[1..4], b"PNG");
        let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
        assert!(w >= 400, "{w}");
    }
}

//! Exports: decode a scan with `trimble-tzf` into levelled points, add
//! intensity, write PLY, LAS or COPC, and run the automatic checks.

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Ply,
    Las,
    /// Cloud Optimized Point Cloud: LAZ-compressed LAS 1.4 in an octree, so
    /// viewers can stream it level by level.
    Copc,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Intensity {
    #[default]
    Auto,
    Scan,
    Preview,
    None,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExportRequest {
    pub format: Format,
    #[serde(default)]
    pub intensity: Intensity,
}

impl Format {
    /// The format's name, as requests and exports spell it.
    pub fn name(self) -> &'static str {
        match self {
            Format::Ply => "ply",
            Format::Las => "las",
            Format::Copc => "copc",
        }
    }

    /// The file extension.
    pub fn ext(self) -> &'static str {
        match self {
            Format::Copc => "copc.laz",
            f => f.name(),
        }
    }
}

/// Why an export stopped.
#[derive(Debug)]
pub enum Failure {
    /// The scan file or preview could not be read (`decode_failed`).
    Decode(String),
    /// The output file could not be written (`cache_error`).
    Write(std::io::Error),
    /// A cancel was asked for.
    Cancelled,
}

/// Points written between checks for a cancel.
const STOP_EVERY: usize = 1 << 16;

fn stopped() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "export cancelled")
}

fn check(name: &str, ok: bool, value: Option<f64>, unit: Option<&str>, detail: String) -> Value {
    let mut v = json!({"name": name, "ok": ok, "detail": detail});
    if let Some(x) = value {
        v["value"] = json!(x);
    }
    if let Some(u) = unit {
        v["unit"] = json!(u);
    }
    v
}

/// Run one export (blocking: call from `spawn_blocking`). Returns the
/// `Export` fields other than ids. `stop` is polled between stages and while
/// writing; once it says yes the export ends with `Failure::Cancelled`.
pub fn run(tzf: &Path, preview: Option<&[u8]>, req: &ExportRequest, out: &Path, stop: &dyn Fn() -> bool) -> Result<Value, Failure> {
    let bad = Failure::Decode;
    let t = trimble_tzf::Tzf::open(tzf).map_err(|e| bad(format!("open: {e}")))?;
    let meta = t.scan_meta().map_err(|e| bad(format!("metadata: {e}")))?;
    if stop() {
        return Err(Failure::Cancelled);
    }
    let pts = t.grid_points().map_err(|e| bad(format!("decode: {e}")))?;
    if stop() {
        return Err(Failure::Cancelled);
    }

    // Intensity. The scan's own intensity image (component 4) is not decoded
    // yet, so `auto` and `scan` use the grid-aligned preview when there is one.
    let (src, gray) = match (req.intensity, preview) {
        (Intensity::None, _) => ("none", None),
        (_, Some(jpg)) => {
            let (w, h, px) = crate::aim::jpeg_luma(jpg).map_err(bad)?;
            ("preview", Some((w, h, px)))
        }
        (Intensity::Preview, None) => return Err(bad("no preview available for intensity".into())),
        (_, None) => ("none", None),
    };
    let grey = Grey::new(&meta, gray);
    let g = |p: &trimble_tzf::GridPoint| grey.of(p);

    match req.format {
        Format::Ply => write_ply(out, &pts, &g, stop),
        Format::Las => write_las(out, &pts, &g, stop),
        Format::Copc => write_copc(out, &pts, &g, LOCAL_WKT, stop),
    }
    .map_err(|e| if e.kind() == std::io::ErrorKind::Interrupted && stop() { Failure::Cancelled } else { Failure::Write(e) })?;

    // Checks.
    let mut lo = [f64::MAX; 3];
    let mut hi = [f64::MIN; 3];
    let mut max_r = 0f64;
    let mut finite = true;
    for p in &pts {
        let q = [p.point.x as f64, p.point.y as f64, p.point.z as f64];
        finite &= q.iter().all(|v| v.is_finite());
        for k in 0..3 {
            lo[k] = lo[k].min(q[k]);
            hi[k] = hi[k].max(q[k]);
        }
        max_r = max_r.max((q[0] * q[0] + q[1] * q[1] + q[2] * q[2]).sqrt());
    }
    let tr = meta.tilt[0][0] + meta.tilt[1][1] + meta.tilt[2][2];
    let tilt_deg = ((tr - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees();
    let declared = meta.n_points as usize;
    let checks = vec![
        check("point_count", pts.len() == declared, Some(pts.len() as f64), Some("points"),
              format!("{} decoded, {declared} declared by the file", pts.len())),
        check("levelled", tilt_deg < 5.0, Some((tilt_deg * 1000.0).round() / 1000.0), Some("deg"),
              format!("tilt correction of {tilt_deg:.3}° applied (the compensator's range is a few degrees)")),
        check("coordinates", finite && max_r < 400.0, Some((max_r * 1000.0).round() / 1000.0), Some("m"),
              "all coordinates finite and within the scanner's range".into()),
        check("intensity", true, None, None,
              match src {
                  "preview" => "from the scanner preview (grid-aligned)".to_string(),
                  _ => "none; points are mid-grey".to_string(),
              }),
    ];
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok(json!({
        "format": req.format.name(),
        "point_count": pts.len(),
        "bytes": bytes,
        "bounds_m": {"min": lo, "max": hi},
        "checks": checks,
    }))
}

/// The project frame's CRS: the frame station's levelled scanner frame.
pub fn project_wkt(frame_station: u16) -> String {
    format!(r#"LOCAL_CS["X9 project frame (station {frame_station}, levelled)",LOCAL_DATUM["station {frame_station} scanner origin",0],UNIT["metre",1],AXIS["X",EAST],AXIS["Y",NORTH],AXIS["Z",UP]]"#)
}

/// One registered station as COPC: its points moved by its pose into the
/// project frame (`wkt`), grey from the preview when there is one. Returns
/// the point count, bytes and bounds.
pub fn save_station(tzf: &Path, preview: Option<&[u8]>, pose: &crate::register::Pose, wkt: &str, out: &Path, stop: &dyn Fn() -> bool) -> Result<Value, Failure> {
    let bad = Failure::Decode;
    let t = trimble_tzf::Tzf::open(tzf).map_err(|e| bad(format!("open: {e}")))?;
    let meta = t.scan_meta().map_err(|e| bad(format!("metadata: {e}")))?;
    let mut pts = t.grid_points().map_err(|e| bad(format!("decode: {e}")))?;
    if stop() {
        return Err(Failure::Cancelled);
    }
    let img = preview.map(crate::aim::jpeg_luma).transpose().map_err(bad)?;
    let grey = Grey::new(&meta, img);
    let (mut lo, mut hi) = ([f64::MAX; 3], [f64::MIN; 3]);
    for p in &mut pts {
        let q = pose.apply([p.point.x, p.point.y, p.point.z]);
        (p.point.x, p.point.y, p.point.z) = (q[0], q[1], q[2]);
        for k in 0..3 {
            lo[k] = lo[k].min(q[k] as f64);
            hi[k] = hi[k].max(q[k] as f64);
        }
    }
    write_copc(out, &pts, &|p| grey.of(p), wkt, stop)
        .map_err(|e| if e.kind() == std::io::ErrorKind::Interrupted && stop() { Failure::Cancelled } else { Failure::Write(e) })?;
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok(json!({"point_count": pts.len(), "bytes": bytes, "bounds_m": {"min": lo, "max": hi}}))
}

/// Grey level of a grid point, from a grid-aligned preview (decoded with
/// `aim::jpeg_luma`); mid-grey without one.
pub struct Grey {
    img: Option<(usize, usize, Vec<u8>)>,
    rows: usize,
    cols: usize,
}

impl Grey {
    pub fn new(meta: &trimble_tzf::ScanMeta, img: Option<(usize, usize, Vec<u8>)>) -> Self {
        Grey { img, rows: meta.grid_rows as usize, cols: meta.grid_cols as usize }
    }

    pub fn of(&self, p: &trimble_tzf::GridPoint) -> u8 {
        match &self.img {
            Some((w, h, px)) => {
                let x = (p.col as usize * w / self.cols.max(1)).min(w - 1);
                let y = (p.row as usize * h / self.rows.max(1)).min(h - 1);
                px[y * w + x]
            }
            None => 128,
        }
    }
}

fn write_ply(path: &Path, pts: &[trimble_tzf::GridPoint], gray: &dyn Fn(&trimble_tzf::GridPoint) -> u8, stop: &dyn Fn() -> bool) -> std::io::Result<()> {
    let mut w = BufWriter::new(std::fs::File::create(path)?);
    write!(
        w,
        "ply\nformat binary_little_endian 1.0\ncomment x9-bridge export\nelement vertex {}\n\
         property float x\nproperty float y\nproperty float z\n\
         property uchar red\nproperty uchar green\nproperty uchar blue\nend_header\n",
        pts.len()
    )?;
    for (i, p) in pts.iter().enumerate() {
        if i % STOP_EVERY == 0 && stop() {
            return Err(stopped());
        }
        let v = gray(p);
        for f in [p.point.x, p.point.y, p.point.z] {
            w.write_all(&f.to_le_bytes())?;
        }
        w.write_all(&[v, v, v])?;
    }
    w.flush()
}

/// LAS 1.2, point data format 0 (20-byte records), 0.1 mm scale.
fn write_las(path: &Path, pts: &[trimble_tzf::GridPoint], gray: &dyn Fn(&trimble_tzf::GridPoint) -> u8, stop: &dyn Fn() -> bool) -> std::io::Result<()> {
    const SCALE: f64 = 0.0001;
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for p in pts {
        for (k, v) in [p.point.x, p.point.y, p.point.z].into_iter().enumerate() {
            min[k] = min[k].min(v as f64);
            max[k] = max[k].max(v as f64);
        }
    }
    if pts.is_empty() {
        min = [0.0; 3];
        max = [0.0; 3];
    }
    let pad = |s: &[u8]| {
        let mut b = [0u8; 32];
        b[..s.len()].copy_from_slice(s);
        b
    };
    let mut h = Vec::with_capacity(227);
    h.extend_from_slice(b"LASF");
    h.extend_from_slice(&[0u8; 4]);
    h.extend_from_slice(&[0u8; 16]);
    h.extend_from_slice(&[1, 2]);
    h.extend_from_slice(&pad(b"x9-bridge"));
    h.extend_from_slice(&pad(b"x9-bridge export"));
    h.extend_from_slice(&[0u8; 4]);
    h.extend_from_slice(&227u16.to_le_bytes());
    h.extend_from_slice(&227u32.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes());
    h.push(0);
    h.extend_from_slice(&20u16.to_le_bytes());
    h.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    h.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    h.extend_from_slice(&[0u8; 16]);
    for _ in 0..3 {
        h.extend_from_slice(&SCALE.to_le_bytes());
    }
    for m in min {
        h.extend_from_slice(&m.to_le_bytes());
    }
    for k in 0..3 {
        h.extend_from_slice(&max[k].to_le_bytes());
        h.extend_from_slice(&min[k].to_le_bytes());
    }
    let mut w = BufWriter::new(std::fs::File::create(path)?);
    w.write_all(&h)?;
    for (i, p) in pts.iter().enumerate() {
        if i % STOP_EVERY == 0 && stop() {
            return Err(stopped());
        }
        for (k, v) in [p.point.x, p.point.y, p.point.z].into_iter().enumerate() {
            w.write_all(&((((v as f64) - min[k]) / SCALE).round() as i32).to_le_bytes())?;
        }
        w.write_all(&(gray(p) as u16 * 257).to_le_bytes())?;
        w.write_all(&[0b0000_1001, 0, 0, 0])?;
        w.write_all(&0u16.to_le_bytes())?;
    }
    w.flush()
}

/// The scanner's levelled frame (metres, z up, origin at the instrument):
/// there is no map projection until stations are registered, so COPC files
/// carry this local CRS.
const LOCAL_WKT: &str = r#"LOCAL_CS["X9 scanner frame, levelled",LOCAL_DATUM["scanner origin",0],UNIT["metre",1],AXIS["X",EAST],AXIS["Y",NORTH],AXIS["Z",UP]]"#;

/// COPC 1.0: LAS 1.4, point format 6, LAZ chunks in an octree, 0.1 mm scale.
fn write_copc(path: &Path, pts: &[trimble_tzf::GridPoint], gray: &dyn Fn(&trimble_tzf::GridPoint) -> u8, wkt: &str, stop: &dyn Fn() -> bool) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    const SCALE: f64 = 0.0001;
    let err = |e: &dyn std::fmt::Display| Error::other(format!("COPC: {e}"));
    if pts.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "COPC: the scan has no points"));
    }
    let mut min = [f64::MAX; 3];
    for p in pts {
        for (k, v) in [p.point.x, p.point.y, p.point.z].into_iter().enumerate() {
            min[k] = min[k].min(v as f64);
        }
    }
    // Snap to the file's grid first so the header's bounds hold every point
    // exactly as stored.
    let q = |v: f32, k: usize| min[k] + (((v as f64) - min[k]) / SCALE).round() * SCALE;
    let point = |p: &trimble_tzf::GridPoint| las::Point {
        x: q(p.point.x, 0),
        y: q(p.point.y, 1),
        z: q(p.point.z, 2),
        intensity: gray(p) as u16 * 257,
        return_number: 1,
        number_of_returns: 1,
        gps_time: Some(0.0),
        ..Default::default()
    };
    let mut b = las::Builder::from((1, 4));
    b.point_format = las::point::Format::new(6).map_err(|e| err(&e))?;
    b.generating_software = "x9-bridge".into();
    b.system_identifier = "Trimble X9 via x9-bridge".into();
    let t = |k: usize| las::Transform { scale: SCALE, offset: min[k] };
    b.transforms = las::Vector { x: t(0), y: t(1), z: t(2) };
    b.has_wkt_crs = true;
    b.vlrs.push(las::Vlr { user_id: "LASF_Projection".into(), record_id: 2112, description: "OGC WKT coordinate system".into(), data: wkt.as_bytes().to_vec() });
    let mut header = b.into_header().map_err(|e| err(&e))?;
    for p in pts {
        header.add_point(&point(p));
    }
    let file = BufWriter::new(std::fs::File::create(path)?);
    let mut w = copc_rs::CopcWriter::new(file, header, -1, -1).map_err(|e| err(&e))?;
    let asked = std::cell::Cell::new(false);
    let points = pts.iter().enumerate().map_while(|(i, p)| {
        if i % STOP_EVERY == 0 && stop() {
            asked.set(true);
            return None;
        }
        Some(point(p))
    });
    let res = w.write(points, pts.len().min(i32::MAX as usize) as i32);
    drop(w);
    if asked.get() {
        return Err(stopped());
    }
    res.map_err(|e| err(&e))?;
    fill_hierarchy(path).map(|_| ())
}

/// copc-rs 0.5 leaves a node out of the hierarchy when it holds no points,
/// even if its children do; readers walk the hierarchy down from the root
/// (PDAL, laspy) and never reach those children, losing their points. Add
/// every missing ancestor as an empty entry (the COPC spec allows point
/// count 0). The hierarchy page is the file's last record, so it grows in
/// place. Returns how many entries were added.
pub fn fill_hierarchy(path: &Path) -> std::io::Result<usize> {
    use std::collections::HashSet;
    use std::io::{Error, Read, Seek, SeekFrom};
    // LAS 1.4 header (375 bytes), then the COPC info VLR: a 54-byte VLR
    // header and 160 bytes of info, with the hierarchy's offset and size at
    // 40 and 48.
    const INFO: u64 = 375 + 54;
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut info = [0u8; 160];
    f.seek(SeekFrom::Start(INFO))?;
    f.read_exact(&mut info)?;
    let off = u64::from_le_bytes(info[40..48].try_into().unwrap());
    let size = u64::from_le_bytes(info[48..56].try_into().unwrap());
    if off + size != f.metadata()?.len() || size % 32 != 0 {
        return Err(Error::other("COPC: the hierarchy is not the last record; can't complete it"));
    }
    let mut page = vec![0u8; size as usize];
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(&mut page)?;
    let key = |e: &[u8]| -> [i32; 4] { std::array::from_fn(|k| i32::from_le_bytes(e[k * 4..k * 4 + 4].try_into().unwrap())) };
    let have: HashSet<[i32; 4]> = page.chunks(32).map(key).collect();
    let mut add: Vec<[i32; 4]> = Vec::new();
    let mut seen = have.clone();
    for k in &have {
        let mut k = *k;
        while k[0] > 0 {
            k = [k[0] - 1, k[1] >> 1, k[2] >> 1, k[3] >> 1];
            if !seen.insert(k) {
                break;
            }
            add.push(k);
        }
    }
    if add.is_empty() {
        return Ok(0);
    }
    add.sort();
    let mut extra = Vec::with_capacity(add.len() * 32);
    for k in &add {
        for v in k {
            extra.extend_from_slice(&v.to_le_bytes());
        }
        extra.extend_from_slice(&[0u8; 16]); // offset 0, byte size 0, point count 0
    }
    f.seek(SeekFrom::End(0))?;
    f.write_all(&extra)?;
    let new_size = size + extra.len() as u64;
    // The EVLR header's record length (at 20 of its 60 bytes), and the info's size.
    f.seek(SeekFrom::Start(off - 60 + 20))?;
    f.write_all(&new_size.to_le_bytes())?;
    f.seek(SeekFrom::Start(INFO + 48))?;
    f.write_all(&new_size.to_le_bytes())?;
    f.flush()?;
    Ok(add.len())
}

/// Hierarchy entries whose parent is missing (for tests).
#[cfg(test)]
fn orphans(path: &Path) -> usize {
    let b = std::fs::read(path).unwrap();
    let off = u64::from_le_bytes(b[375 + 54 + 40..375 + 54 + 48].try_into().unwrap()) as usize;
    let size = u64::from_le_bytes(b[375 + 54 + 48..375 + 54 + 56].try_into().unwrap()) as usize;
    let keys: std::collections::HashSet<[i32; 4]> = b[off..off + size]
        .chunks(32)
        .map(|e| std::array::from_fn(|k| i32::from_le_bytes(e[k * 4..k * 4 + 4].try_into().unwrap())))
        .collect();
    keys.iter().filter(|k| k[0] > 0 && !keys.contains(&[k[0] - 1, k[1] >> 1, k[2] >> 1, k[3] >> 1])).count()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;
    use trimble_tzf::{GridPoint, Point};

    fn pts(n: usize) -> Vec<GridPoint> {
        (0..n).map(|i| GridPoint { point: Point { x: i as f32, y: 1.0 + (i % 3) as f32 * 0.01, z: 2.0 + (i % 5) as f32 * 0.01, intensity: 0.0 }, row: 0, col: i as u32 }).collect()
    }

    // ------------------------------------------------ synthetic scan files ---
    // Shared with the other modules' tests: scan files the decoder reads,
    // built from a ray cast, so nothing depends on a recorded (private) scan.

    /// A 16 × 8 grey JPEG (a left-to-right ramp), as a scanner preview.
    pub(crate) const GREY_JPEG_HEX: &str = "ffd8ffc0000b080008001001011100ffc4001f0000010501010101010100000000000000000102030405060708090a0bffc400b5100002010303020403050504040000017d01020300041105122131410613516107227114328191a1082342b1c11552d1f02433627282090a161718191a25262728292a3435363738393a434445464748494a535455565758595a636465666768696a737475767778797a838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e9eaf1f2f3f4f5f6f7f8f9faffdb00430001010101010102010102030202020304030303030405040404040405060505050505050606060606060606070707070707080808080809090909090909090909ffdd00040002ffda0008010100003f00fe6fff00e09b7ff2e1ff0001aff400ff00826dff00cb87fc06bfffd9";

    pub(crate) fn grey_jpeg() -> Vec<u8> {
        (0..GREY_JPEG_HEX.len()).step_by(2).map(|i| u8::from_str_radix(&GREY_JPEG_HEX[i..i + 2], 16).unwrap()).collect()
    }

    /// A `.tzf` of `rows` × `cols` cells, `step` radians apart from vertical
    /// angle `va0` (from the zenith) and horizontal angle `ha0`: `hit(va, ha)`
    /// is the range (m) the ray meets, if any. One tile, DEFLATE planes,
    /// 0.1 mm range steps, no tilt.
    pub(crate) fn tzf_bytes(rows: usize, cols: usize, va0: f64, ha0: f64, step: f64, hit: impl Fn(f64, f64) -> Option<f64>) -> Vec<u8> {
        let n = rows * cols;
        let (mut range, mut va, mut ha) = (vec![0u32; n], vec![0u32; n], vec![0u32; n]);
        let mut count = 0u32;
        for line in 0..cols {
            let (mut pv, mut ph) = (0i64, 0i64);
            for k in 0..rows {
                let i = line * rows + k;
                let (a, b) = (va0 + k as f64 * step, ha0 + line as f64 * step);
                let (ua, ub) = ((a * 1e6).round() as i64, (b * 1e6).round() as i64);
                va[i] = (ua - pv) as i32 as u32;
                ha[i] = (ub - ph) as i32 as u32;
                (pv, ph) = (ua, ub);
                if let Some(r) = hit(a, b).filter(|r| r.is_finite() && *r > 0.0) {
                    range[i] = (r * 10_000.0).round() as u32;
                    count += 1;
                }
            }
        }
        let mut buf = vec![0u8; trimble_tzf::FIRST_DESCRIPTOR];
        buf[..8].copy_from_slice(&trimble_tzf::MAGIC);
        for (component, scale, plane) in [(1u32, 10.0f32, &range), (2, 1.0, &va), (3, 1.0, &ha)] {
            let raw: Vec<u8> = plane.iter().flat_map(|c| c.to_le_bytes()).collect();
            let mut payload = (raw.len() as u32).to_le_bytes().to_vec();
            payload.extend_from_slice(&[0x78, 0x01]);
            payload.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(&raw, 1));
            buf.extend_from_slice(&0u64.to_le_bytes());
            for v in [200u32, 16, payload.len() as u32 + 28, component, 0, 0, 100, cols as u32, rows as u32, raw.len() as u32] {
                buf.extend_from_slice(&v.to_le_bytes());
            }
            buf.extend_from_slice(&scale.to_le_bytes());
            buf.extend_from_slice(&0u64.to_le_bytes());
            buf.extend_from_slice(&payload);
        }
        let m = buf.len();
        buf[0x10..0x18].copy_from_slice(&(m as u64).to_le_bytes());
        buf.extend_from_slice(&[0u8; 0x44]);
        for (off, v) in [(0x10, step), (0x18, step), (0x20, va0), (0x28, -ha0)] {
            buf[m + off..m + off + 8].copy_from_slice(&v.to_le_bytes());
        }
        for (off, v) in [(0x30, rows as u32), (0x34, cols as u32), (0x38, rows.max(cols) as u32), (0x3c, count)] {
            buf[m + off..m + off + 4].copy_from_slice(&v.to_le_bytes());
        }
        for (k, v) in [1.0f64, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0].iter().enumerate() {
            buf.extend_from_slice(&v.to_le_bytes());
            let _ = k;
        }
        buf
    }

    /// Range along the ray (`va` from the zenith, `ha`) from a scanner at the
    /// origin to the inside of an open-topped box `lo`..`hi` (m): walls and a
    /// floor, no ceiling.
    pub(crate) fn room(lo: [f64; 3], hi: [f64; 3]) -> impl Fn(f64, f64) -> Option<f64> {
        move |va: f64, ha: f64| {
            let d = [va.sin() * ha.cos(), va.sin() * ha.sin(), va.cos()];
            if d[2] > 0.0 {
                // Up: only walls below the top.
                let t = (0..2).map(|k| if d[k] > 0.0 { hi[k] / d[k] } else if d[k] < 0.0 { lo[k] / d[k] } else { f64::MAX }).fold(f64::MAX, f64::min);
                return (t * d[2] <= hi[2]).then_some(t);
            }
            Some((0..3).map(|k| if d[k] > 0.0 { hi[k] / d[k] } else if d[k] < 0.0 { lo[k] / d[k] } else { f64::MAX }).fold(f64::MAX, f64::min))
        }
    }

    /// A full dome of [`room`]: `cols` columns round, rows from 0.4 rad off
    /// the zenith to near the nadir.
    pub(crate) fn room_dome(cols: usize, lo: [f64; 3], hi: [f64; 3]) -> Vec<u8> {
        let step = std::f64::consts::TAU / cols as f64;
        let rows = ((std::f64::consts::PI - 0.45) / step) as usize;
        tzf_bytes(rows, cols, 0.4, 0.0, step, room(lo, hi))
    }

    /// A fresh directory for one test's files.
    pub(crate) fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(crate::util::new_id(tag));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_synthetic_scan_decodes_to_the_room_it_was_cast_from() {
        let t = trimble_tzf::Tzf::from_bytes(room_dome(180, [-3.0, -2.0, -1.5], [4.0, 5.0, 1.0])).unwrap();
        let meta = t.scan_meta().unwrap();
        let pts = t.grid_points().unwrap();
        assert_eq!(pts.len(), meta.n_points as usize);
        assert!(pts.len() > 1000);
        for p in &pts {
            let q = p.point;
            let on = |v: f32, w: f64| (v as f64 - w).abs() < 0.01;
            assert!(on(q.x, -3.0) || on(q.x, 4.0) || on(q.y, -2.0) || on(q.y, 5.0) || on(q.z, -1.5), "{q:?} is on no surface");
            assert!(q.z <= 1.01);
        }
        assert_eq!(grey_jpeg()[..2], [0xff, 0xd8]);
        assert_eq!(crate::aim::jpeg_size(&grey_jpeg()).unwrap(), (16, 8));
    }

    fn write_scan(dir: &Path) -> std::path::PathBuf {
        let f = dir.join("dome.tzf");
        std::fs::write(&f, room_dome(240, [-3.0, -2.0, -1.5], [4.0, 5.0, 1.0])).unwrap();
        f
    }

    #[test]
    fn an_export_writes_each_format_with_its_checks() {
        let dir = scratch("x9-export-run");
        let f = write_scan(&dir);
        let jpg = grey_jpeg();
        for (format, intensity, preview, source) in [
            (Format::Ply, Intensity::Auto, Some(&jpg[..]), "preview"),
            (Format::Las, Intensity::Scan, None, "none"),
            (Format::Copc, Intensity::None, Some(&jpg[..]), "none"),
            (Format::Ply, Intensity::Preview, Some(&jpg[..]), "preview"),
        ] {
            let out = dir.join(format!("out.{}", format.ext()));
            let req = ExportRequest { format, intensity };
            let v = run(&f, preview, &req, &out, &|| false).unwrap_or_else(|e| panic!("{format:?}: {e:?}"));
            assert_eq!(v["format"], format.name());
            assert!(v["point_count"].as_u64().unwrap() > 1000);
            assert_eq!(v["bytes"].as_u64().unwrap(), std::fs::metadata(&out).unwrap().len());
            let checks = v["checks"].as_array().unwrap();
            let names: Vec<&str> = checks.iter().map(|c| c["name"].as_str().unwrap()).collect();
            assert_eq!(names, ["point_count", "levelled", "coordinates", "intensity"]);
            assert!(checks[..3].iter().all(|c| c["ok"] == true), "{format:?}: {checks:?}");
            assert_eq!(checks[1]["unit"], "deg");
            assert!(checks[3].get("value").is_none());
            assert_eq!(checks[3]["detail"].as_str().unwrap().starts_with("from the scanner preview"), source == "preview");
            assert!((v["bounds_m"]["min"][2].as_f64().unwrap() + 1.5).abs() < 0.01, "{}", v["bounds_m"]);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_export_without_its_inputs_or_cancelled_stops_with_the_reason() {
        let dir = scratch("x9-export-fail");
        let f = write_scan(&dir);
        let out = dir.join("x.ply");
        let req = |intensity| ExportRequest { format: Format::Ply, intensity };
        // Preview intensity needs a preview; a preview must be a JPEG.
        assert!(matches!(run(&f, None, &req(Intensity::Preview), &out, &|| false), Err(Failure::Decode(e)) if e.contains("no preview")));
        assert!(matches!(run(&f, Some(b"not a jpeg"), &req(Intensity::Auto), &out, &|| false), Err(Failure::Decode(_))));
        // A file that isn't a scan.
        let junk = dir.join("junk.tzf");
        std::fs::write(&junk, vec![0u8; 256]).unwrap();
        assert!(matches!(run(&junk, None, &req(Intensity::None), &out, &|| false), Err(Failure::Decode(e)) if e.starts_with("open")));
        // Cancelled before decoding, after decoding, and while writing.
        assert!(matches!(run(&f, None, &req(Intensity::None), &out, &|| true), Err(Failure::Cancelled)));
        for after in [1, 2] {
            let polls = Cell::new(0);
            let stop = || {
                polls.set(polls.get() + 1);
                polls.get() > after
            };
            assert!(matches!(run(&f, None, &req(Intensity::None), &out, &stop), Err(Failure::Cancelled)), "after {after}");
        }
        // An output that can't be written.
        let nowhere = dir.join("missing").join("x.ply");
        assert!(matches!(run(&f, None, &req(Intensity::None), &nowhere, &|| false), Err(Failure::Write(_))));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn formats_have_their_names_and_extensions() {
        assert_eq!([Format::Ply.name(), Format::Las.name(), Format::Copc.name()], ["ply", "las", "copc"]);
        assert_eq!([Format::Ply.ext(), Format::Las.ext(), Format::Copc.ext()], ["ply", "las", "copc.laz"]);
        let r: ExportRequest = serde_json::from_value(json!({"format": "copc"})).unwrap();
        assert_eq!((r.format, r.intensity), (Format::Copc, Intensity::Auto));
        assert!(serde_json::from_value::<ExportRequest>(json!({"format": "e57"})).is_err());
    }

    #[test]
    fn grey_comes_from_the_preview_pixel_over_the_cell() {
        let meta = trimble_tzf::Tzf::from_bytes(room_dome(160, [-3.0, -2.0, -1.5], [4.0, 5.0, 1.0])).unwrap().scan_meta().unwrap();
        let at = |row, col| GridPoint { point: Point { x: 0.0, y: 0.0, z: 0.0, intensity: 0.0 }, row, col };
        assert_eq!(Grey::new(&meta, None).of(&at(3, 3)), 128);
        let (w, h, px) = crate::aim::jpeg_luma(&grey_jpeg()).unwrap();
        let g = Grey::new(&meta, Some((w, h, px.clone())));
        // The ramp brightens to the right; past the last column stays on the image.
        assert!(g.of(&at(0, 0)) < g.of(&at(0, meta.grid_cols - 1)));
        assert_eq!(g.of(&at(meta.grid_rows + 5, meta.grid_cols + 5)), px[(h - 1) * w + w - 1]);
    }

    #[test]
    fn an_empty_scan_writes_an_empty_las_and_no_copc() {
        let dir = scratch("x9-export-empty");
        write_las(&dir.join("e.las"), &[], &|_| 0, &|| false).unwrap();
        assert_eq!(std::fs::metadata(dir.join("e.las")).unwrap().len(), 227);
        let e = write_copc(&dir.join("e.copc.laz"), &[], &|_| 0, LOCAL_WKT, &|| false).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        // A file that isn't COPC can't have its hierarchy completed.
        std::fs::write(dir.join("short.laz"), b"LASF").unwrap();
        assert!(fill_hierarchy(&dir.join("short.laz")).is_err());
        std::fs::write(dir.join("zeros.laz"), vec![0u8; 700]).unwrap();
        assert!(fill_hierarchy(&dir.join("zeros.laz")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writers_stop_when_asked() {
        let dir = std::env::temp_dir().join(crate::util::new_id("x9-export"));
        std::fs::create_dir_all(&dir).unwrap();
        let p = pts(3 * STOP_EVERY);
        let grey = |_: &GridPoint| 128u8;
        for (name, w) in [("a.ply", write_ply as fn(&Path, &[GridPoint], &dyn Fn(&GridPoint) -> u8, &dyn Fn() -> bool) -> std::io::Result<()>), ("a.las", write_las), ("a.copc.laz", |p, pts, g, stop| write_copc(p, pts, g, LOCAL_WKT, stop))] {
            let out = dir.join(name);
            // Asked to stop after the first batch.
            let polls = Cell::new(0);
            let stop = || {
                polls.set(polls.get() + 1);
                polls.get() > 1
            };
            let e = w(&out, &p, &grey, &stop).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::Interrupted, "{name}");
            assert_eq!(polls.get(), 2, "{name}");
            // Never asked: every point is written.
            w(&out, &p, &grey, &|| false).unwrap();
            let len = std::fs::metadata(&out).unwrap().len() as usize;
            // COPC is compressed; the others are 15+ bytes a point.
            assert!(len > if name.ends_with("laz") { 1000 } else { p.len() * 15 }, "{name}: {len}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copc_reads_back() {
        let dir = std::env::temp_dir().join(crate::util::new_id("x9-copc"));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("a.copc.laz");
        let p: Vec<GridPoint> = (0..50_000)
            .map(|i| GridPoint { point: Point { x: (i % 250) as f32 * 0.01, y: (i / 250) as f32 * 0.01, z: 1.5 + (i % 7) as f32 * 0.001, intensity: 0.0 }, row: i / 250, col: i % 250 })
            .collect();
        write_copc(&out, &p, &|g| (g.col % 256) as u8, LOCAL_WKT, &|| false).unwrap();
        let mut r = copc_rs::CopcReader::from_path(&out).unwrap();
        assert!(r.num_entries() > 0);
        let got: Vec<las::Point> = r.points(copc_rs::LodSelection::All, copc_rs::BoundsSelection::All).unwrap().collect();
        assert_eq!(got.len(), p.len());
        assert!(r.header().has_wkt_crs());
        let top = got.iter().map(|q| q.x).fold(f64::MIN, f64::max);
        assert!((top - 2.49).abs() < 1e-6, "{top}");
        // Intensity and coordinates survive, to the file's 0.1 mm grid.
        let a = got.iter().find(|q| (q.x - 1.23).abs() < 1e-6 && (q.y - 0.5).abs() < 1e-6).expect("point (1.23, 0.5)");
        assert_eq!(a.intensity, 123 * 257);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn every_copc_node_is_reachable_from_the_root() {
        let dir = std::env::temp_dir().join(crate::util::new_id("x9-copc2"));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("b.copc.laz");
        // What a scanner sees: rays on an angular grid from the origin to the
        // walls of an off-centre box room, so density falls off with range
        // and the far corners are sparse (where copc-rs left nodes orphaned).
        let (lo, hi) = ([-3.0f64, -2.0, -1.3], [7.0f64, 4.0, 1.2]);
        let mut p = Vec::new();
        let (rows, cols) = (700u32, 2100u32);
        for r in 0..rows {
            let va = 0.05 + (r as f64 + 0.5) / rows as f64 * (std::f64::consts::PI - 0.1);
            for c in 0..cols {
                let ha = c as f64 / cols as f64 * std::f64::consts::TAU;
                let d = [va.sin() * ha.cos(), va.sin() * ha.sin(), va.cos()];
                let t = (0..3).map(|k| if d[k] > 0.0 { hi[k] / d[k] } else if d[k] < 0.0 { lo[k] / d[k] } else { f64::MAX }).fold(f64::MAX, f64::min);
                p.push(GridPoint { point: Point { x: (d[0] * t) as f32, y: (d[1] * t) as f32, z: (d[2] * t) as f32, intensity: 0.0 }, row: r, col: c });
            }
        }
        write_copc(&out, &p, &|_| 0, LOCAL_WKT, &|| false).unwrap();
        assert_eq!(orphans(&out), 0);
        let mut r = copc_rs::CopcReader::from_path(&out).unwrap();
        let n = r.points(copc_rs::LodSelection::All, copc_rs::BoundsSelection::All).unwrap().count();
        assert_eq!(n, p.len());
        // Completing it again adds nothing.
        assert_eq!(fill_hierarchy(&out).unwrap(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_a_decode_failure() {
        let req = ExportRequest { format: Format::Ply, intensity: Intensity::None };
        let out = std::env::temp_dir().join(crate::util::new_id("x9-none")).join("x.ply");
        assert!(matches!(run(Path::new("/nonexistent.tzf"), None, &req, &out, &|| true), Err(Failure::Decode(_))));
    }
}

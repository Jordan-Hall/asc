//! Exploitation intelligence for advisories: CISA's Known Exploited Vulnerabilities catalog (KEV) and
//! the EPSS exploit prediction scores.
//!
//! This module is pure: bytes in, values out. It reads no network, no config and no files. Both feeds
//! may arrive as plain text or gzip (found by the magic bytes `1f 8b`); gzip is inflated under a hard
//! cap so a small file cannot grow without limit. A feed that was never loaded is **unknown**, which is
//! different from a feed that was loaded and does not list the CVE; [`Intel::lookup`] keeps the two apart.
//! A feed that parses but holds too few usable rows is refused, so a wrong or emptied file can never
//! make every CVE read as "not listed".

use std::collections::{BTreeSet, HashMap};
use std::io::Read;

use serde::Deserialize;
use serde::de::{DeserializeSeed, Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor};

/// Most entries read from a KEV catalog.
pub const MAX_KEV_ENTRIES: usize = 200_000;
/// Most score rows read from an EPSS file.
pub const MAX_EPSS_ROWS: usize = 2_000_000;
/// Longest line accepted in an EPSS file, in bytes.
pub const MAX_EPSS_LINE: usize = 1024;
/// Largest KEV catalog after decompression, in bytes.
pub const MAX_KEV_BYTES: u64 = 64 * 1024 * 1024;
/// Largest EPSS file after decompression, in bytes.
pub const MAX_EPSS_BYTES: u64 = 256 * 1024 * 1024;
/// Fewest usable entries a real KEV catalog has; the fetch step passes this as `min_rows`.
pub const KEV_MIN_ROWS: usize = 1_000;
/// Fewest usable rows a real EPSS file has; the fetch step passes this as `min_rows`.
pub const EPSS_MIN_ROWS: usize = 100_000;

/// Why a feed could not be read.
#[derive(Debug, thiserror::Error)]
pub enum IntelError {
    /// The gzip stream is damaged or cut short.
    #[error("the feed is not valid gzip: {0}")]
    Gzip(String),
    /// The feed is larger than its bound once decompressed.
    #[error("the feed is larger than its limit of {0} bytes")]
    TooLarge(u64),
    /// The KEV catalog is not the JSON shape CISA publishes.
    #[error("the KEV catalog is not valid JSON of the expected shape: {0}")]
    BadKev(String),
    /// The KEV catalog lists more entries than the bound.
    #[error("the KEV catalog has more than {0} entries")]
    TooManyEntries(usize),
    /// The EPSS file is not UTF-8 text.
    #[error("the EPSS file is not UTF-8 text")]
    NotUtf8,
    /// The EPSS file has no `cve,epss,percentile` header line.
    #[error("the EPSS file has no cve,epss,percentile header line")]
    NoHeader,
    /// A line of the EPSS file is longer than the bound.
    #[error("line {line} of the EPSS file is longer than {max} bytes")]
    LineTooLong {
        /// One-based line number.
        line: usize,
        /// The bound that was crossed.
        max: usize,
    },
    /// The EPSS file has more rows than the bound.
    #[error("the EPSS file has more than {0} rows")]
    TooManyRows(usize),
    /// The feed parsed but has fewer usable rows than a real feed has.
    #[error("the feed has only {found} usable rows, fewer than the {min} required")]
    TooFewRows {
        /// Usable rows found.
        found: usize,
        /// Rows required.
        min: usize,
    },
    /// Too many rows of the feed were unusable, so the file is probably not the expected one.
    #[error("{skipped} rows of the feed were unusable against {usable} usable rows")]
    TooManySkipped {
        /// Rows skipped.
        skipped: usize,
        /// Usable rows found.
        usable: usize,
    },
}

/// Returns `bytes` as they are, or inflated when they start with the gzip magic bytes (every gzip member
/// is read). Output beyond `max` bytes is refused.
fn unpack(bytes: &[u8], max: u64) -> Result<std::borrow::Cow<'_, [u8]>, IntelError> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::MultiGzDecoder::new(bytes)
            .take(max.saturating_add(1))
            .read_to_end(&mut out)
            .map_err(|e| IntelError::Gzip(e.to_string()))?;
        if out.len() as u64 > max {
            return Err(IntelError::TooLarge(max));
        }
        return Ok(std::borrow::Cow::Owned(out));
    }
    if bytes.len() as u64 > max {
        return Err(IntelError::TooLarge(max));
    }
    Ok(std::borrow::Cow::Borrowed(bytes))
}

/// Refuses a feed with too few usable rows (fewer than `min_rows`, and never fewer than one) or too many
/// skipped ones (at least 10 and more than 1% of the usable rows).
fn check_rows(usable: usize, skipped: usize, min_rows: usize) -> Result<(), IntelError> {
    let min = min_rows.max(1);
    if usable < min {
        return Err(IntelError::TooFewRows { found: usable, min });
    }
    if skipped >= 10 && skipped.saturating_mul(100) > usable {
        return Err(IntelError::TooManySkipped { skipped, usable });
    }
    Ok(())
}

/// Upper-cases `id` and returns it when it has the shape `CVE-YYYY-NNNN...`: a year from 1999 to 2100,
/// then four or more digits, with no leading zero when there are more than four. Any other text gives
/// `None`.
pub fn normalize_cve(id: &str) -> Option<String> {
    let up = id.trim().to_ascii_uppercase();
    let rest = up.strip_prefix("CVE-")?;
    let (year, seq) = rest.split_once('-')?;
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if year.len() != 4 || !digits(year) || seq.len() < 4 || seq.len() > 19 || !digits(seq) {
        return None;
    }
    if !(1999..=2100).contains(&year.parse::<u32>().ok()?) {
        return None;
    }
    if seq.len() > 4 && seq.starts_with('0') {
        return None;
    }
    Some(up)
}

/// `s` when it is exactly `YYYY-MM-DD` (ASCII digits and dashes).
fn ymd(s: String) -> Option<String> {
    let ok = s.len() == 10
        && s.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    ok.then_some(s)
}

/// `s` when it is a short timestamp: at most 40 ASCII letters, digits and `-:+.`.
fn stamp(s: String) -> Option<String> {
    let ok = !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b':' | b'+' | b'.'));
    ok.then_some(s)
}

/// `s` when it is at most 64 bytes of printable ASCII.
fn label(s: String) -> Option<String> {
    let ok = !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| (0x20..=0x7e).contains(&b));
    ok.then_some(s)
}

/// The earlier of two optional `YYYY-MM-DD` dates; a missing date loses to a present one.
fn earliest(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// What the KEV catalog says about one CVE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KevEntry {
    /// The day CISA added the CVE (`YYYY-MM-DD`), when given and well formed.
    pub date_added: Option<String>,
    /// The day federal agencies must have fixed it by (`YYYY-MM-DD`), when given and well formed.
    pub due_date: Option<String>,
    /// True when CISA marks the CVE as used in ransomware campaigns.
    pub ransomware: bool,
}

impl KevEntry {
    /// Joins two entries: ransomware flags are OR-ed and the earliest dates are kept.
    fn merge(self, other: KevEntry) -> KevEntry {
        KevEntry {
            date_added: earliest(self.date_added, other.date_added),
            due_date: earliest(self.due_date, other.due_date),
            ransomware: self.ransomware || other.ransomware,
        }
    }
}

/// A parsed KEV catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kev {
    /// The catalog version string, when given and well formed.
    pub catalog_version: Option<String>,
    /// The catalog release date, when given and well formed.
    pub date_released: Option<String>,
    /// Entries skipped because they had no usable CVE id.
    pub skipped: usize,
    entries: HashMap<String, KevEntry>,
}

impl Kev {
    /// The entry for an already normalised CVE id.
    pub fn get(&self, cve: &str) -> Option<&KevEntry> {
        self.entries.get(cve)
    }

    /// How many CVEs the catalog lists.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the catalog lists no CVE.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Deserialize)]
struct RawKevEntry {
    #[serde(rename = "cveID")]
    cve_id: Option<String>,
    #[serde(rename = "dateAdded")]
    date_added: Option<String>,
    #[serde(rename = "dueDate")]
    due_date: Option<String>,
    #[serde(rename = "knownRansomwareCampaignUse")]
    ransomware: Option<String>,
}

/// The catalog while it is read: entries go straight into the map, so memory stays bounded by the cap.
struct KevState {
    kev: Kev,
    cap: usize,
    seen: usize,
    over: bool,
}

impl KevState {
    fn add(&mut self, e: RawKevEntry) {
        let Some(id) = e.cve_id.as_deref().and_then(normalize_cve) else {
            self.kev.skipped += 1;
            return;
        };
        let entry = KevEntry {
            date_added: e.date_added.and_then(ymd),
            due_date: e.due_date.and_then(ymd),
            ransomware: e
                .ransomware
                .as_deref()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("known")),
        };
        match self.kev.entries.remove(&id) {
            Some(old) => self.kev.entries.insert(id, old.merge(entry)),
            None => self.kev.entries.insert(id, entry),
        };
    }
}

struct DocSeed<'a>(&'a mut KevState);
struct DocVisitor<'a>(&'a mut KevState);
struct EntriesSeed<'a>(&'a mut KevState);
struct EntriesVisitor<'a>(&'a mut KevState);

impl<'de> DeserializeSeed<'de> for DocSeed<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(DocVisitor(self.0))
    }
}

impl<'de> Visitor<'de> for DocVisitor<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a KEV catalog object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let state = self.0;
        let mut found = false;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "catalogVersion" => {
                    let v: Option<String> = map.next_value()?;
                    state.kev.catalog_version = v.and_then(label);
                }
                "dateReleased" => {
                    let v: Option<String> = map.next_value()?;
                    state.kev.date_released = v.and_then(stamp);
                }
                "vulnerabilities" => {
                    map.next_value_seed(EntriesSeed(state))?;
                    found = true;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        if found {
            Ok(())
        } else {
            Err(A::Error::missing_field("vulnerabilities"))
        }
    }
}

impl<'de> DeserializeSeed<'de> for EntriesSeed<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(EntriesVisitor(self.0))
    }
}

impl<'de> Visitor<'de> for EntriesVisitor<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a list of KEV entries")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let state = self.0;
        while let Some(e) = seq.next_element::<RawKevEntry>()? {
            state.seen += 1;
            if state.seen > state.cap {
                state.over = true;
                return Err(A::Error::custom("too many entries"));
            }
            state.add(e);
        }
        Ok(())
    }
}

/// Reads a KEV catalog (JSON, plain or gzip). Entries without a usable `cveID` are skipped and counted;
/// more than [`MAX_KEV_ENTRIES`] entries is an error, found while reading, before the rest is stored. The
/// catalog must have a `vulnerabilities` list with at least `min_rows` usable entries (and never zero),
/// and not too many skipped ones.
pub fn parse_kev(bytes: &[u8], min_rows: usize) -> Result<Kev, IntelError> {
    parse_kev_capped(bytes, min_rows, MAX_KEV_ENTRIES)
}

fn parse_kev_capped(bytes: &[u8], min_rows: usize, cap: usize) -> Result<Kev, IntelError> {
    let data = unpack(bytes, MAX_KEV_BYTES)?;
    let mut state = KevState {
        kev: Kev::default(),
        cap,
        seen: 0,
        over: false,
    };
    let mut de = serde_json::Deserializer::from_slice(&data);
    let res = DocSeed(&mut state)
        .deserialize(&mut de)
        .and_then(|()| de.end());
    if state.over {
        return Err(IntelError::TooManyEntries(cap));
    }
    res.map_err(|e| IntelError::BadKev(e.to_string()))?;
    check_rows(state.kev.entries.len(), state.kev.skipped, min_rows)?;
    Ok(state.kev)
}

/// One EPSS score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EpssScore {
    /// Probability of exploitation in the next 30 days, from 0 to 1.
    pub score: f64,
    /// Share of CVEs scored at or below this one, from 0 to 1.
    pub percentile: f64,
}

/// A parsed EPSS file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Epss {
    /// The model version from the leading comment, when present and well formed.
    pub model_version: Option<String>,
    /// The score date from the leading comment, when present and well formed.
    pub score_date: Option<String>,
    /// Rows skipped because the id, a number or the range was bad.
    pub skipped: usize,
    scores: HashMap<String, EpssScore>,
}

impl Epss {
    /// The score for an already normalised CVE id.
    pub fn get(&self, cve: &str) -> Option<EpssScore> {
        self.scores.get(cve).copied()
    }

    /// How many CVEs have a score.
    pub fn len(&self) -> usize {
        self.scores.len()
    }

    /// True when no CVE has a score.
    pub fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }
}

fn unit_interval(s: &str) -> Option<f64> {
    let v: f64 = s.trim().parse().ok()?;
    (v.is_finite() && (0.0..=1.0).contains(&v)).then_some(v)
}

/// Reads an EPSS scores file (CSV, plain or gzip). A leading `#model_version:...,score_date:...` comment
/// is read when present. A bad row is skipped and counted; more than [`MAX_EPSS_ROWS`] rows or a line
/// longer than [`MAX_EPSS_LINE`] bytes is an error. The file must have at least `min_rows` usable rows
/// (and never zero), and not too many skipped ones. A repeated CVE keeps the higher score.
pub fn parse_epss(bytes: &[u8], min_rows: usize) -> Result<Epss, IntelError> {
    let data = unpack(bytes, MAX_EPSS_BYTES)?;
    let text = std::str::from_utf8(&data).map_err(|_| IntelError::NotUtf8)?;
    let mut epss = Epss::default();
    let mut header = false;
    let mut rows = 0usize;
    for (n, line) in text.lines().enumerate() {
        if line.len() > MAX_EPSS_LINE {
            return Err(IntelError::LineTooLong {
                line: n + 1,
                max: MAX_EPSS_LINE,
            });
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            if !header {
                for part in comment.split(',') {
                    match part.trim().split_once(':') {
                        Some(("model_version", v)) => {
                            epss.model_version = label(v.trim().into());
                        }
                        Some(("score_date", v)) => epss.score_date = stamp(v.trim().into()),
                        _ => {}
                    }
                }
            }
            continue;
        }
        if !header {
            let cols: Vec<String> = line.split(',').map(|c| c.trim().to_lowercase()).collect();
            if cols != ["cve", "epss", "percentile"] {
                return Err(IntelError::NoHeader);
            }
            header = true;
            continue;
        }
        rows += 1;
        if rows > MAX_EPSS_ROWS {
            return Err(IntelError::TooManyRows(MAX_EPSS_ROWS));
        }
        let mut cols = line.split(',');
        let parsed = match (cols.next(), cols.next(), cols.next(), cols.next()) {
            (Some(id), Some(s), Some(p), None) => normalize_cve(id)
                .zip(unit_interval(s))
                .zip(unit_interval(p)),
            _ => None,
        };
        match parsed {
            Some(((id, score), percentile)) => {
                let new = EpssScore { score, percentile };
                epss.scores
                    .entry(id)
                    .and_modify(|old| {
                        if new.score > old.score {
                            *old = new;
                        }
                    })
                    .or_insert(new);
            }
            None => epss.skipped += 1,
        }
    }
    if !header {
        return Err(IntelError::NoHeader);
    }
    check_rows(epss.scores.len(), epss.skipped, min_rows)?;
    Ok(epss)
}

/// What one feed says about one CVE. An absent feed is [`Feed::Unknown`], never [`Feed::NotListed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed<T> {
    /// The feed was not loaded, so nothing is known.
    Unknown,
    /// The feed was loaded and does not list the CVE (or the advisory has no CVE id).
    NotListed,
    /// The feed lists the CVE.
    Listed(T),
}

/// The answer of [`Intel::lookup`].
#[derive(Debug, Clone, PartialEq)]
pub struct IntelHit {
    /// Every normalised CVE id found among the advisory's id and aliases (sorted, no repeats).
    pub cve: Vec<String>,
    /// What KEV says: listed when any of the ids is listed.
    pub kev: Feed<KevEntry>,
    /// What EPSS says: the highest score among the ids.
    pub epss: Feed<EpssScore>,
}

/// The loaded feeds. A feed is `None` when it was never loaded or failed to load.
#[derive(Debug, Clone, Default)]
pub struct Intel {
    /// The KEV catalog, when loaded.
    pub kev: Option<Kev>,
    /// The EPSS scores, when loaded.
    pub epss: Option<Epss>,
}

impl Intel {
    /// Looks up an advisory by its id and aliases. Every id of CVE shape counts. KEV is listed when any
    /// is listed (ransomware flags are OR-ed, earliest dates kept); EPSS takes the highest score.
    pub fn lookup<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> IntelHit {
        let cve: Vec<String> = ids
            .into_iter()
            .filter_map(normalize_cve)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let kev = match &self.kev {
            None => Feed::Unknown,
            Some(k) => cve
                .iter()
                .filter_map(|c| k.get(c).cloned())
                .reduce(KevEntry::merge)
                .map_or(Feed::NotListed, Feed::Listed),
        };
        let epss = match &self.epss {
            None => Feed::Unknown,
            Some(e) => cve
                .iter()
                .filter_map(|c| e.get(c))
                .reduce(|a, b| if b.score > a.score { b } else { a })
                .map_or(Feed::NotListed, Feed::Listed),
        };
        IntelHit { cve, kev, epss }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const KEV: &str = r#"{
  "title": "CISA Catalog of Known Exploited Vulnerabilities",
  "catalogVersion": "2026.11.12",
  "dateReleased": "2026-11-12T15:00:00.000Z",
  "count": 3,
  "vulnerabilities": [
    {"cveID": "CVE-2021-44228", "vendorProject": "Apache", "product": "Log4j2",
     "vulnerabilityName": "Log4Shell", "dateAdded": "2021-12-10", "shortDescription": "x",
     "requiredAction": "Apply updates.", "dueDate": "2021-12-24",
     "knownRansomwareCampaignUse": "Known", "notes": "", "extra": [1, 2]},
    {"cveID": "cve-2023-1234", "vendorProject": "V", "product": "P", "vulnerabilityName": "N",
     "dateAdded": "2023-03-01", "dueDate": "2023-03-22", "knownRansomwareCampaignUse": "Unknown",
     "requiredAction": "Patch."},
    {"vendorProject": "V", "product": "P", "dateAdded": "2023-03-01"}
  ]
}"#;

    const EPSS: &str = "#model_version:v2025.03.14,score_date:2026-11-12T00:00:00+0000\n\
cve,epss,percentile\n\
CVE-2021-44228,0.97565,1.00000\n\
CVE-2023-1234,0.00043,0.08\n";

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn kev(text: &str) -> Result<Kev, IntelError> {
        parse_kev(text.as_bytes(), 1)
    }

    #[test]
    fn kev_sample_is_read() {
        let k = parse_kev(KEV.as_bytes(), 2).unwrap();
        assert_eq!(k.len(), 2);
        assert_eq!(k.skipped, 1);
        assert_eq!(k.catalog_version.as_deref(), Some("2026.11.12"));
        assert_eq!(k.date_released.as_deref(), Some("2026-11-12T15:00:00.000Z"));
        let e = k.get("CVE-2021-44228").unwrap();
        assert_eq!(e.date_added.as_deref(), Some("2021-12-10"));
        assert_eq!(e.due_date.as_deref(), Some("2021-12-24"));
        assert!(e.ransomware);
        assert!(!k.get("CVE-2023-1234").unwrap().ransomware);
    }

    #[test]
    fn epss_sample_is_read() {
        let e = parse_epss(EPSS.as_bytes(), 2).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e.skipped, 0);
        assert_eq!(e.model_version.as_deref(), Some("v2025.03.14"));
        assert_eq!(e.score_date.as_deref(), Some("2026-11-12T00:00:00+0000"));
        let s = e.get("CVE-2021-44228").unwrap();
        assert!((s.score - 0.97565).abs() < 1e-9);
        assert!((s.percentile - 1.0).abs() < 1e-9);
    }

    #[test]
    fn epss_without_comment_is_read() {
        let e = parse_epss(b"cve,epss,percentile\nCVE-2020-0001,0.5,0.5\n", 1).unwrap();
        assert_eq!(e.len(), 1);
        assert!(e.model_version.is_none() && e.score_date.is_none());
    }

    #[test]
    fn gzip_input_is_read() {
        assert_eq!(parse_kev(&gz(KEV.as_bytes()), 2).unwrap().len(), 2);
        assert_eq!(parse_epss(&gz(EPSS.as_bytes()), 2).unwrap().len(), 2);
    }

    #[test]
    fn concatenated_gzip_members_are_all_read() {
        let mut z = gz(b"cve,epss,percentile\nCVE-2020-0001,0.5,0.5\n");
        z.extend(gz(b"CVE-2020-0002,0.25,0.25\n"));
        let e = parse_epss(&z, 2).unwrap();
        assert_eq!(e.len(), 2);
        assert!(e.get("CVE-2020-0002").is_some());
    }

    #[test]
    fn truncated_json_is_an_error() {
        let cut = &KEV.as_bytes()[..KEV.len() / 2];
        assert!(matches!(parse_kev(cut, 1), Err(IntelError::BadKev(_))));
    }

    #[test]
    fn truncated_gzip_is_an_error() {
        let z = gz(KEV.as_bytes());
        assert!(matches!(
            parse_kev(&z[..z.len() / 2], 1),
            Err(IntelError::Gzip(_))
        ));
    }

    #[test]
    fn hostile_kev_files_are_refused() {
        assert!(matches!(kev("{}"), Err(IntelError::BadKev(_))));
        assert!(matches!(kev("[]"), Err(IntelError::BadKev(_))));
        assert!(matches!(
            kev(r#"{"vulnerabilities": []}"#),
            Err(IntelError::TooFewRows { found: 0, min: 1 })
        ));
        let wrong_case: Vec<String> = (0..50)
            .map(|i| format!(r#"{{"cveId": "CVE-2020-{:04}"}}"#, 1000 + i))
            .collect();
        let text = format!(r#"{{"vulnerabilities": [{}]}}"#, wrong_case.join(","));
        assert!(matches!(
            kev(&text),
            Err(IntelError::TooFewRows { found: 0, .. })
        ));
        assert!(matches!(
            parse_kev(KEV.as_bytes(), 3),
            Err(IntelError::TooFewRows { found: 2, min: 3 })
        ));
    }

    #[test]
    fn kev_with_many_skipped_entries_is_refused() {
        let mut items = vec![r#"{"cveID": "CVE-2020-1000"}"#.to_string()];
        items.extend((0..20).map(|_| "{}".to_string()));
        let text = format!(r#"{{"vulnerabilities": [{}]}}"#, items.join(","));
        assert!(matches!(
            kev(&text),
            Err(IntelError::TooManySkipped {
                skipped: 20,
                usable: 1
            })
        ));
    }

    #[test]
    fn kev_entry_cap_stops_reading() {
        let items = vec!["{}"; 101].join(",");
        let text = format!(r#"{{"vulnerabilities": [{items}]}}"#);
        assert!(matches!(
            parse_kev_capped(text.as_bytes(), 1, 100),
            Err(IntelError::TooManyEntries(100))
        ));
        let ok = format!(r#"{{"vulnerabilities": [{}]}}"#, vec!["{}"; 100].join(","));
        // At the cap the entries are read; all are unusable, so the row check refuses instead.
        assert!(matches!(
            parse_kev_capped(ok.as_bytes(), 1, 100),
            Err(IntelError::TooFewRows { .. })
        ));
    }

    #[test]
    fn epss_header_only_is_refused() {
        assert!(matches!(
            parse_epss(b"cve,epss,percentile\n", 1),
            Err(IntelError::TooFewRows { found: 0, min: 1 })
        ));
        assert!(matches!(
            parse_epss(b"cve,epss,percentile\n", 0),
            Err(IntelError::TooFewRows { .. })
        ));
        assert!(matches!(
            parse_epss(b"cve,epss,percentile\nCVE-2020-0001,0.5,0.5\n", 2),
            Err(IntelError::TooFewRows { found: 1, min: 2 })
        ));
    }

    #[test]
    fn bad_csv_rows_are_skipped_and_counted() {
        let csv = "cve,epss,percentile\n\
CVE-2020-0001,0.5,0.5\n\
CVE-2020-0002,1.5,0.5\n\
CVE-2020-0003,abc,0.5\n\
CVE-2020-0004,0.5\n\
not-a-cve,0.5,0.5\n\
CVE-2020-0005,0.5,-0.1\n\
CVE-2020-0006,NaN,0.5\n\
CVE-2020-0007,0.1,0.2,extra\n\
CVE-2020-0008,0.25,0.75\n";
        let e = parse_epss(csv.as_bytes(), 2).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e.skipped, 7);
        assert!(e.get("CVE-2020-0008").is_some());
    }

    #[test]
    fn epss_with_many_bad_rows_is_refused() {
        let mut csv = String::from("cve,epss,percentile\nCVE-2020-0001,0.5,0.5\n");
        for _ in 0..12 {
            csv.push_str("junk\n");
        }
        assert!(matches!(
            parse_epss(csv.as_bytes(), 1),
            Err(IntelError::TooManySkipped {
                skipped: 12,
                usable: 1
            })
        ));
    }

    #[test]
    fn epss_needs_a_header_and_bounds_lines() {
        assert!(matches!(
            parse_epss(b"CVE-2020-0001,0.5,0.5\n", 1),
            Err(IntelError::NoHeader)
        ));
        assert!(matches!(parse_epss(b"", 1), Err(IntelError::NoHeader)));
        let long = format!(
            "cve,epss,percentile\nCVE-2020-0001,0.5,{}\n",
            "1".repeat(2000)
        );
        assert!(matches!(
            parse_epss(long.as_bytes(), 1),
            Err(IntelError::LineTooLong { line: 2, .. })
        ));
    }

    #[test]
    fn oversized_decompression_is_refused() {
        let z = gz(&vec![b' '; 4096]);
        assert!(matches!(unpack(&z, 1024), Err(IntelError::TooLarge(1024))));
        assert!(unpack(&z, 4096).is_ok());
        assert!(matches!(
            unpack(&[b'a'; 10], 5),
            Err(IntelError::TooLarge(5))
        ));
        let mut two = gz(&vec![b' '; 600]);
        two.extend(gz(&vec![b' '; 600]));
        assert!(matches!(
            unpack(&two, 1000),
            Err(IntelError::TooLarge(1000))
        ));
    }

    #[test]
    fn cve_ids_are_normalised() {
        assert_eq!(
            normalize_cve(" cve-2021-44228 ").as_deref(),
            Some("CVE-2021-44228")
        );
        assert_eq!(
            normalize_cve("CVE-2021-0001").as_deref(),
            Some("CVE-2021-0001")
        );
        for bad in [
            "GHSA-aaaa-bbbb-cccc",
            "CVE-21-1234",
            "CVE-2021-123",
            "CVE-2021-12x4",
            "CVE-0000-0001",
            "CVE-1998-0001",
            "CVE-2101-0001",
            "CVE-2021-044228",
            "",
        ] {
            assert!(normalize_cve(bad).is_none(), "{bad}");
        }
        let k = parse_kev(KEV.as_bytes(), 1).unwrap();
        assert!(k.get("CVE-2023-1234").is_some());
    }

    #[test]
    fn odd_strings_are_dropped() {
        let text = r#"{"catalogVersion": "<script>alert(1)</script>\u0007",
            "dateReleased": "<script>",
            "vulnerabilities": [
              {"cveID": "CVE-2020-1000", "dateAdded": "<script>", "dueDate": "2020-1-1"},
              {"cveID": "CVE-2020-1001", "dateAdded": "2020-01-02", "dueDate": "2020-01-30"}]}"#;
        let k = kev(text).unwrap();
        assert!(k.catalog_version.is_none());
        assert!(k.date_released.is_none());
        let a = k.get("CVE-2020-1000").unwrap();
        assert!(a.date_added.is_none() && a.due_date.is_none());
        assert_eq!(
            k.get("CVE-2020-1001").unwrap().due_date.as_deref(),
            Some("2020-01-30")
        );
        let e = parse_epss(
            b"#model_version:v1\x07,score_date:<b>x</b>\ncve,epss,percentile\nCVE-2020-0001,0.5,0.5\n",
            1,
        )
        .unwrap();
        assert!(e.model_version.is_none() && e.score_date.is_none());
        assert!(label("x".repeat(65)).is_none());
        assert!(stamp("2".repeat(41)).is_none());
    }

    #[test]
    fn duplicate_kev_entries_merge() {
        let text = r#"{"vulnerabilities": [
          {"cveID": "CVE-2020-1000", "dateAdded": "2021-05-05", "dueDate": "2021-06-06",
           "knownRansomwareCampaignUse": "Unknown"},
          {"cveID": "cve-2020-1000", "dateAdded": "2020-05-05", "dueDate": "2022-06-06",
           "knownRansomwareCampaignUse": "Known"}]}"#;
        let k = kev(text).unwrap();
        assert_eq!(k.len(), 1);
        let e = k.get("CVE-2020-1000").unwrap();
        assert_eq!(e.date_added.as_deref(), Some("2020-05-05"));
        assert_eq!(e.due_date.as_deref(), Some("2021-06-06"));
        assert!(e.ransomware);
    }

    #[test]
    fn duplicate_epss_rows_keep_the_higher_score() {
        let csv = "cve,epss,percentile\nCVE-2020-0001,0.2,0.3\nCVE-2020-0001,0.9,0.95\n\
cve-2020-0001,0.5,0.6\n";
        let e = parse_epss(csv.as_bytes(), 1).unwrap();
        assert_eq!(e.len(), 1);
        let s = e.get("CVE-2020-0001").unwrap();
        assert!((s.score - 0.9).abs() < 1e-9 && (s.percentile - 0.95).abs() < 1e-9);
    }

    fn intel() -> Intel {
        Intel {
            kev: Some(parse_kev(KEV.as_bytes(), 1).unwrap()),
            epss: Some(parse_epss(EPSS.as_bytes(), 1).unwrap()),
        }
    }

    #[test]
    fn lookup_goes_through_an_alias() {
        let hit = intel().lookup(["GHSA-jfh8-c2jp-5v3q", "cve-2021-44228"]);
        assert_eq!(hit.cve, ["CVE-2021-44228"]);
        match hit.kev {
            Feed::Listed(e) => assert!(e.ransomware),
            other => panic!("{other:?}"),
        }
        match hit.epss {
            Feed::Listed(s) => assert!(s.score > 0.9),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn lookup_uses_every_cve_id() {
        let hit = intel().lookup(["CVE-2020-0001", "CVE-2021-44228", "cve-2020-0001"]);
        assert_eq!(hit.cve, ["CVE-2020-0001", "CVE-2021-44228"]);
        assert!(matches!(hit.kev, Feed::Listed(ref e) if e.ransomware));
        assert!(matches!(hit.epss, Feed::Listed(s) if s.score > 0.9));

        let both = intel().lookup(["CVE-2021-44228", "CVE-2023-1234"]);
        match both.kev {
            Feed::Listed(e) => {
                assert!(e.ransomware);
                assert_eq!(e.date_added.as_deref(), Some("2021-12-10"));
                assert_eq!(e.due_date.as_deref(), Some("2021-12-24"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn absent_feed_is_not_the_same_as_not_listed() {
        let none = Intel::default().lookup(["CVE-2021-44228"]);
        assert_eq!(none.kev, Feed::Unknown);
        assert_eq!(none.epss, Feed::Unknown);

        let present = intel().lookup(["CVE-1999-0001"]);
        assert_eq!(present.kev, Feed::NotListed);
        assert_eq!(present.epss, Feed::NotListed);

        let no_cve = intel().lookup(["GHSA-jfh8-c2jp-5v3q"]);
        assert!(no_cve.cve.is_empty());
        assert_eq!(no_cve.kev, Feed::NotListed);

        let half = Intel {
            kev: None,
            epss: intel().epss,
        }
        .lookup(["CVE-2021-44228"]);
        assert_eq!(half.kev, Feed::Unknown);
        assert!(matches!(half.epss, Feed::Listed(_)));
    }
}

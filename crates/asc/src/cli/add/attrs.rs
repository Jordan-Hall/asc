//! Attribute lookup for ingest (SPEC §2.4.1): which line-ending normalisation a path gets, and
//! which attributes `add` cannot honour. Attributes come from, lowest precedence first,
//! `core.attributesFile`, each directory's `.ascattributes` else `.gitattributes` (root to leaf)
//! and `<common>/info/attributes`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use asc_core::path::RepoPath;
use asc_wc::SafeRoot;
use asc_wc::status::{Eol, EolRules};
use bstr::ByteSlice;
use gix_attributes::glob::pattern::Case;
use gix_attributes::search::{MetadataCollection, Outcome};
use gix_attributes::{Search, StateRef};

/// Attributes `add` reads.
const SELECTED: [&str; 6] = [
    "text",
    "crlf",
    "eol",
    "ident",
    "filter",
    "working-tree-encoding",
];

/// A path whose attributes `add` cannot honour.
#[derive(Debug, thiserror::Error)]
#[error("asc: attribute '{0}' is not supported")]
pub struct Unsupported(pub String);

/// What `add` does with one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathAttrs {
    pub eol: Eol,
    /// `filter=lfs`: ignored, the content is stored natively (SPEC §2.4.1).
    pub lfs: bool,
}

/// Per-directory attribute searches, built on demand and cached.
pub struct Attrs {
    root: PathBuf,
    gate: SafeRoot,
    info: Option<PathBuf>,
    collection: MetadataCollection,
    outcome: Outcome,
    /// The search for each directory (`""` is the root), `info/attributes` last.
    dirs: HashMap<Vec<u8>, Rc<Search>>,
    base: Search,
    buf: Vec<u8>,
}

impl Attrs {
    /// Reads the global files; per-directory files are read when a path below them is looked up.
    pub fn load(root: &Path, common_dir: &Path, global: Option<PathBuf>) -> io::Result<Self> {
        let mut collection = MetadataCollection::default();
        let mut buf = Vec::new();
        let base = Search::new_globals(global, &mut buf, &mut collection)?;
        let info = common_dir.join("info").join("attributes");
        Ok(Self {
            root: root.to_owned(),
            gate: SafeRoot::new(root),
            info: info.is_file().then_some(info),
            collection,
            outcome: Outcome::default(),
            dirs: HashMap::new(),
            base,
            buf,
        })
    }

    fn search(&mut self, dir: &[u8]) -> io::Result<Rc<Search>> {
        if let Some(found) = self.dirs.get(dir) {
            return Ok(found.clone());
        }
        let mut search = if dir.is_empty() {
            self.base.clone()
        } else {
            let parent = dir
                .iter()
                .rposition(|&c| c == b'/')
                .map_or(&[][..], |i| &dir[..i]);
            let mut inherited = (*self.search(parent)?).clone();
            if self.info.is_some() {
                inherited.pop_pattern_list(); // `info/attributes` goes last again below
            }
            inherited
        };
        // A directory behind a link is outside the work tree, and so is a linked attributes file.
        let regular = |name: &str| -> Option<PathBuf> {
            let rel = if dir.is_empty() {
                name.as_bytes().to_vec()
            } else {
                [dir, b"/", name.as_bytes()].concat()
            };
            let abs = self.gate.resolve(&RepoPath::new(rel).ok()?)?;
            std::fs::symlink_metadata(&abs)
                .is_ok_and(|m| m.is_file())
                .then_some(abs)
        };
        if let Some(file) = regular(".ascattributes").or_else(|| regular(".gitattributes")) {
            search.add_patterns_file(
                file,
                false,
                Some(&self.root),
                &mut self.buf,
                &mut self.collection,
                true,
            )?;
        }
        if let Some(info) = &self.info {
            search.add_patterns_file(
                info.clone(),
                false,
                None,
                &mut self.buf,
                &mut self.collection,
                true,
            )?;
        }
        let search = Rc::new(search);
        self.dirs.insert(dir.to_vec(), search.clone());
        Ok(search)
    }

    /// The attributes of `path`, or the first one `add` cannot honour.
    pub fn lookup(&mut self, path: &RepoPath) -> Result<PathAttrs, Unsupported> {
        let bytes = path.as_bytes();
        let dir = bytes
            .iter()
            .rposition(|&c| c == b'/')
            .map_or(&[][..], |i| &bytes[..i]);
        // An unreadable attributes file reads as absent, like git.
        let Ok(search) = self.search(dir) else {
            return Ok(PathAttrs {
                eol: Eol::Keep,
                lfs: false,
            });
        };
        self.outcome
            .initialize_with_selection(&self.collection, SELECTED);
        search.pattern_matching_relative_path(
            path.as_bytes().as_bstr(),
            Case::Sensitive,
            Some(false),
            &mut self.outcome,
        );
        let mut found: HashMap<String, StateRef<'_>> = HashMap::new();
        for m in self.outcome.iter_selected() {
            found.insert(m.assignment.name.as_str().to_owned(), m.assignment.state);
        }
        let value = |name: &str| match found.get(name) {
            Some(StateRef::Value(v)) => Some(v.as_bstr().to_str_lossy().into_owned()),
            _ => None,
        };
        if matches!(found.get("ident"), Some(StateRef::Set | StateRef::Value(_))) {
            return Err(Unsupported("ident".into()));
        }
        if value("working-tree-encoding").is_some() {
            return Err(Unsupported("working-tree-encoding".into()));
        }
        let filter = value("filter");
        if let Some(driver) = filter.as_deref().filter(|d| *d != "lfs") {
            return Err(Unsupported(format!("filter={driver}")));
        }
        // git's `text` attribute, else the legacy `crlf`; `eol=lf|crlf` makes an unspecified text
        // a text, and `-text` (so `binary`) wins over it.
        let from = |name: &str| match found.get(name) {
            Some(StateRef::Set) => Some(Eol::Text),
            Some(StateRef::Unset) => Some(Eol::Keep),
            Some(StateRef::Value(v)) if v.as_bstr() == "auto" => Some(Eol::Auto),
            _ => None,
        };
        let action = from("text").or_else(|| from("crlf"));
        let mut eol = action.unwrap_or(Eol::Keep);
        if action != Some(Eol::Keep)
            && matches!(value("eol").as_deref(), Some("lf" | "crlf"))
            && eol == Eol::Keep
        {
            eol = Eol::Text;
        }
        Ok(PathAttrs {
            eol,
            lfs: filter.is_some(),
        })
    }
}

/// [`EolRules`] over [`Attrs`] for the status hasher. A path with unsupported attributes is not
/// normalised; `add` is what refuses it.
pub struct AttrEol(RefCell<Attrs>);

impl AttrEol {
    pub fn new(attrs: Attrs) -> Self {
        Self(RefCell::new(attrs))
    }
}

impl EolRules for AttrEol {
    fn eol(&self, path: &RepoPath) -> Eol {
        self.0
            .borrow_mut()
            .lookup(path)
            .map_or(Eol::Keep, |a| a.eol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(files: &[(&str, &str)]) -> (tempfile::TempDir, Attrs) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let common = dir.path().join("common");
        std::fs::create_dir_all(common.join("info")).unwrap();
        let a = Attrs::load(dir.path(), &common, None).unwrap();
        (dir, a)
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn text_auto_text_and_binary() {
        let (_d, mut a) = attrs(&[(
            ".gitattributes",
            "*.txt text=auto\n*.sh text eol=lf\n*.dat binary\n*.crlf eol=crlf\n",
        )]);
        assert_eq!(a.lookup(&p("a.txt")).unwrap().eol, Eol::Auto);
        assert_eq!(a.lookup(&p("d/a.sh")).unwrap().eol, Eol::Text);
        assert_eq!(a.lookup(&p("a.dat")).unwrap().eol, Eol::Keep);
        assert_eq!(a.lookup(&p("a.crlf")).unwrap().eol, Eol::Text);
        assert_eq!(a.lookup(&p("plain")).unwrap().eol, Eol::Keep);
    }

    #[test]
    fn ascattributes_replaces_gitattributes_in_its_directory() {
        let (_d, mut a) = attrs(&[
            (".gitattributes", "* text\n"),
            ("sub/.gitattributes", "* text\n"),
            ("sub/.ascattributes", "* -text\n"),
        ]);
        assert_eq!(a.lookup(&p("x")).unwrap().eol, Eol::Text);
        assert_eq!(a.lookup(&p("sub/x")).unwrap().eol, Eol::Keep);
        assert_eq!(a.lookup(&p("sub/deeper/x")).unwrap().eol, Eol::Keep);
    }

    #[test]
    fn deeper_files_override_and_info_wins() {
        let (d, mut a) = attrs(&[
            (".gitattributes", "*.txt text\n"),
            ("sub/.gitattributes", "*.txt -text\n"),
        ]);
        std::fs::write(
            d.path().join("common/info/attributes"),
            "special.txt text=auto\n",
        )
        .unwrap();
        let mut a2 = Attrs::load(d.path(), &d.path().join("common"), None).unwrap();
        assert_eq!(a.lookup(&p("sub/a.txt")).unwrap().eol, Eol::Keep);
        assert_eq!(a2.lookup(&p("sub/a.txt")).unwrap().eol, Eol::Keep);
        assert_eq!(a2.lookup(&p("special.txt")).unwrap().eol, Eol::Auto);
        assert_eq!(
            a2.lookup(&p("sub/special.txt")).unwrap().eol,
            Eol::Auto,
            "info beats the leaf file"
        );
    }

    #[test]
    fn unsupported_attributes_and_lfs() {
        let (_d, mut a) = attrs(&[(
            ".gitattributes",
            "*.i ident\n*.e working-tree-encoding=UTF-16\n*.f filter=foo\n*.bin filter=lfs diff=lfs\n",
        )]);
        assert_eq!(a.lookup(&p("a.i")).unwrap_err().0, "ident");
        assert_eq!(a.lookup(&p("a.e")).unwrap_err().0, "working-tree-encoding");
        assert_eq!(a.lookup(&p("a.f")).unwrap_err().0, "filter=foo");
        let lfs = a.lookup(&p("a.bin")).unwrap();
        assert!(lfs.lfs);
        assert_eq!(lfs.eol, Eol::Keep);
    }
}

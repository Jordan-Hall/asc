//! The `diff` attribute of a path (SPEC §2.4.1): `diff` forces text, `-diff` (so `binary`)
//! forces binary, `diff=<driver>` names a driver. Attributes come from, lowest precedence
//! first, `core.attributesFile`, each directory's `.ascattributes` else `.gitattributes` (root
//! to leaf) and `<common>/info/attributes`. Drivers are named but never run: textconv is not
//! served, so a driver path diffs its raw content.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use asc_core::path::RepoPath;
use asc_engine::diff::{self, Attributes, DiffAttr};
use asc_wc::SafeRoot;
use async_trait::async_trait;
use bstr::ByteSlice;
use bytes::Bytes;
use gix_attributes::glob::pattern::Case;
use gix_attributes::search::{MetadataCollection, Outcome};
use gix_attributes::{Search, StateRef};

use crate::cli::common::{Ctx, WsCtx};

struct Inner {
    root: PathBuf,
    gate: SafeRoot,
    info: Option<PathBuf>,
    collection: MetadataCollection,
    outcome: Outcome,
    /// The search for each directory (`""` is the root), `info/attributes` last.
    dirs: HashMap<Vec<u8>, Search>,
    base: Search,
    buf: Vec<u8>,
}

/// [`Attributes`] over the work tree's attribute files, read when a path below them is first
/// asked about.
pub struct DiffAttrs(Mutex<Inner>);

impl DiffAttrs {
    pub fn load(ctx: &Ctx, ws: &WsCtx) -> io::Result<Self> {
        let global = ctx.config.path("core.attributesFile").or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::home_dir().map(|h| h.join(".config")))
                .map(|c| c.join("git").join("attributes"))
        });
        let (root, common): (&Path, &Path) = (ws.projection.toplevel(), ws.projection.common_dir());
        let mut collection = MetadataCollection::default();
        let mut buf = Vec::new();
        let base = Search::new_globals(global, &mut buf, &mut collection)?;
        let info = common.join("info").join("attributes");
        Ok(Self(Mutex::new(Inner {
            root: root.to_owned(),
            gate: SafeRoot::new(root),
            info: info.is_file().then_some(info),
            collection,
            outcome: Outcome::default(),
            dirs: HashMap::new(),
            base,
            buf,
        })))
    }
}

impl Inner {
    fn search(&mut self, dir: &[u8]) -> io::Result<Search> {
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
            let mut inherited = self.search(parent)?;
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
        self.dirs.insert(dir.to_vec(), search.clone());
        Ok(search)
    }

    fn lookup(&mut self, path: &RepoPath) -> DiffAttr {
        let bytes = path.as_bytes();
        let dir = bytes
            .iter()
            .rposition(|&c| c == b'/')
            .map_or(&[][..], |i| &bytes[..i]);
        // An unreadable attributes file reads as absent, like git.
        let Ok(search) = self.search(dir) else {
            return DiffAttr::Unspecified;
        };
        self.outcome
            .initialize_with_selection(&self.collection, ["diff"]);
        search.pattern_matching_relative_path(
            bytes.as_bstr(),
            Case::Sensitive,
            Some(false),
            &mut self.outcome,
        );
        match self
            .outcome
            .iter_selected()
            .next()
            .map(|m| m.assignment.state)
        {
            Some(StateRef::Set) => DiffAttr::Text,
            Some(StateRef::Unset) => DiffAttr::Binary,
            Some(StateRef::Value(v)) => DiffAttr::Driver(v.as_bstr().to_str_lossy().into_owned()),
            _ => DiffAttr::Unspecified,
        }
    }
}

#[async_trait]
impl Attributes for DiffAttrs {
    fn diff_attr(&self, path: &RepoPath) -> DiffAttr {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lookup(path)
    }

    async fn textconv(&self, _: &str, _: &RepoPath, _: &Bytes) -> diff::Result<Option<Bytes>> {
        Ok(None)
    }
}

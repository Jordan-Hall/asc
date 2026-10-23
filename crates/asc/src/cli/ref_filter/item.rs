//! One ref as the atoms see it: its name, the object it points at (and the one a tag peels to),
//! and the values that need the repository (`refname:short`, `upstream`, `describe`, …) loaded
//! before rendering, keyed by the atom's text.

use std::collections::HashMap;

use asc_core::id::{ChangesetId, GitOid, RefTarget, TagId};

/// What ref-filter can point at in an asc repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjKind {
    Commit,
    Tag,
}

impl ObjKind {
    /// git's name for the type.
    pub fn name(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Tag => "tag",
        }
    }
}

/// An object's id and (when an atom needs it) its raw git bytes.
#[derive(Clone, Debug)]
pub struct Obj {
    pub kind: ObjKind,
    pub oid: GitOid,
    /// Empty until a format reads fields beyond the id.
    pub raw: Vec<u8>,
}

/// One listed ref.
#[derive(Clone, Debug)]
pub struct Item {
    /// The full name, `refs/heads/master` (or `HEAD`).
    pub name: String,
    pub target: Option<RefTarget>,
    /// The commit the ref leads to, peeling a tag.
    pub commit: Option<ChangesetId>,
    /// The tag object, for an annotated tag.
    pub tag: Option<TagId>,
    pub obj: Option<Obj>,
    /// The peeled object of a tag.
    pub deref: Option<Obj>,
    /// Whether it is the checked-out branch.
    pub head: bool,
    /// The ref a symbolic ref names (only `HEAD`).
    pub symref: Option<String>,
    /// Loaded values by atom text.
    pub extra: HashMap<String, Vec<u8>>,
}

impl Item {
    /// A ref with nothing loaded.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            target: None,
            commit: None,
            tag: None,
            obj: None,
            deref: None,
            head: false,
            symref: None,
            extra: HashMap::new(),
        }
    }
}

//! `pubspec.lock`.

use super::Parsed;
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version};
use crate::deps::yaml::{self, Y};

const ECO: Ecosystem = Ecosystem::Pub;

pub(super) fn pubspec_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc = yaml::parse(text)?;
    if !matches!(doc, Y::Map(_)) {
        return Err("not a pubspec.lock".into());
    }
    let path = p.path.clone();
    let mut left_out = 0usize;
    for (name, pkg) in doc.get("packages").map_or(&[][..], Y::entries) {
        let hosted_on_pub = pkg.str_at("source") == Some("hosted")
            && pkg
                .get("description")
                .and_then(|d| d.str_at("url"))
                .is_some_and(|u| u.contains("pub.dev") || u.contains("pub.dartlang.org"));
        let version = pkg
            .str_at("version")
            .filter(|v| Version::parse(Scheme::Semver, v).is_some());
        let (true, Some(version)) = (hosted_on_pub, version) else {
            left_out += 1;
            continue;
        };
        let mut d = Dep::lock(ECO, name, version, &path);
        d.direct = pkg
            .str_at("dependency")
            .is_some_and(|k| k.starts_with("direct"));
        p.pin(d)?;
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} packages are SDK, git, path or private-server sources and are not checked"
        ));
    }
    Ok(())
}

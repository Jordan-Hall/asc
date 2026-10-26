//! `asc ci artifact upload|download|list` (SPEC §12.7, PLAN U75). An artifact is a manifest like a
//! cache entry, in the `artifact` namespace of the server's store. Names are immutable unless an
//! upload asks to overwrite; a download reads the caller's scope, then `trusted`, and works across
//! runs because the name is not tied to one.

use std::path::Path;

use asc_store::objects::Objects;
use bytes::Bytes;

use crate::cache::{
    ActionsManifest, Error, ListedKey, ManifestApi, RestoreStats, Result, StoreKind, UploadStats,
    check_key, collect, materialise, upload_items,
};

/// What `artifact upload` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Uploaded {
    /// The name is taken and `overwrite` was not given.
    Exists,
    /// Nothing matched the paths.
    Empty,
    Uploaded {
        entries: usize,
        stats: UploadStats,
    },
}

/// `asc ci artifact upload <name> <paths>`.
pub async fn upload(
    api: &dyn ManifestApi,
    objects: &dyn Objects,
    root: &Path,
    name: &str,
    specs: &[String],
    overwrite: bool,
) -> Result<Uploaded> {
    check_key(name)?;
    if specs.is_empty() {
        return Err(Error::Input("no paths given".into()));
    }
    let found = collect(root, specs)?;
    if found.items.is_empty() {
        return Ok(Uploaded::Empty);
    }
    let (entries, stats) = upload_items(objects, &found.items).await?;
    let manifest = ActionsManifest::new(entries)?;
    let body = Bytes::from(manifest.encode()?);
    if !api.put(StoreKind::Artifact, name, body, overwrite).await? {
        return Ok(Uploaded::Exists);
    }
    Ok(Uploaded::Uploaded {
        entries: manifest.entries.len(),
        stats,
    })
}

/// `asc ci artifact download <name> [--path dir]`: `None` when no artifact has that name.
pub async fn download(
    api: &dyn ManifestApi,
    objects: &dyn Objects,
    dest: &Path,
    name: &str,
) -> Result<Option<RestoreStats>> {
    check_key(name)?;
    let Some(body) = api.get(StoreKind::Artifact, name).await? else {
        return Ok(None);
    };
    let manifest = ActionsManifest::decode(&body)?;
    materialise(objects, dest, &manifest).await.map(Some)
}

/// `asc ci artifact list [<prefix>]`, newest first.
pub async fn list(api: &dyn ManifestApi, prefix: &str) -> Result<Vec<ListedKey>> {
    if !prefix.is_empty() {
        check_key(prefix)?;
    }
    api.list(StoreKind::Artifact, prefix).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::testutil::{Mem, MemApi};

    fn specs(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn upload_download_list_across_directories() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("out/sub")).unwrap();
        std::fs::write(src.path().join("out/sub/report.txt"), b"ok").unwrap();
        std::fs::write(src.path().join("out/b.bin"), vec![1u8; 5 * 1024 * 1024]).unwrap();
        let (api, objs) = (MemApi::default(), Mem::default());
        let up = upload(&api, &objs, src.path(), "build-1", &specs(&["out"]), false)
            .await
            .unwrap();
        assert!(
            matches!(up, Uploaded::Uploaded { entries: 2, .. }),
            "{up:?}"
        );
        upload(
            &api,
            &objs,
            src.path(),
            "build-2",
            &specs(&["out/sub"]),
            false,
        )
        .await
        .unwrap();

        let names: Vec<String> = list(&api, "build-")
            .await
            .unwrap()
            .into_iter()
            .map(|k| k.key)
            .collect();
        assert_eq!(names, ["build-2", "build-1"]);
        assert!(list(&api, "zzz").await.unwrap().is_empty());

        let dst = tempfile::tempdir().unwrap();
        let got = download(&api, &objs, &dst.path().join("fetched"), "build-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.files, 2);
        assert_eq!(
            std::fs::read(dst.path().join("fetched/out/sub/report.txt")).unwrap(),
            b"ok"
        );
        assert!(
            download(&api, &objs, dst.path(), "nope")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn names_are_immutable_unless_overwritten() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("f"), b"1").unwrap();
        let (api, objs) = (MemApi::default(), Mem::default());
        upload(&api, &objs, src.path(), "a", &specs(&["f"]), false)
            .await
            .unwrap();
        std::fs::write(src.path().join("f"), b"2").unwrap();
        assert_eq!(
            upload(&api, &objs, src.path(), "a", &specs(&["f"]), false)
                .await
                .unwrap(),
            Uploaded::Exists
        );
        assert!(matches!(
            upload(&api, &objs, src.path(), "a", &specs(&["f"]), true)
                .await
                .unwrap(),
            Uploaded::Uploaded { .. }
        ));
        let dst = tempfile::tempdir().unwrap();
        download(&api, &objs, dst.path(), "a").await.unwrap();
        assert_eq!(std::fs::read(dst.path().join("f")).unwrap(), b"2");
    }

    #[tokio::test]
    async fn bad_names_and_empty_matches() {
        let src = tempfile::tempdir().unwrap();
        let (api, objs) = (MemApi::default(), Mem::default());
        assert!(
            upload(&api, &objs, src.path(), "", &specs(&["f"]), false)
                .await
                .is_err()
        );
        assert!(
            upload(&api, &objs, src.path(), "a", &[], false)
                .await
                .is_err()
        );
        assert_eq!(
            upload(&api, &objs, src.path(), "a", &specs(&["missing"]), false)
                .await
                .unwrap(),
            Uploaded::Empty
        );
    }
}

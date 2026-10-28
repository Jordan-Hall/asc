use super::*;

const LOOKUP: &[u8] = include_bytes!("../fixtures/registry/sumdb-pkg-errors.txt");

/// The tiles of the real log the lookup's proof needs, by their path.
fn real_tiles(l: &Lookup) -> HashMap<TileId, Vec<u8>> {
    let files: [(&str, &[u8]); 7] = [
        (
            "tile/8/2/000",
            include_bytes!("../fixtures/registry/sumdb-tiles/2_000"),
        ),
        (
            "tile/8/1/010",
            include_bytes!("../fixtures/registry/sumdb-tiles/1_010"),
        ),
        (
            "tile/8/0/x002/760",
            include_bytes!("../fixtures/registry/sumdb-tiles/0_x002_760"),
        ),
        (
            "tile/8/3/000.p/4",
            include_bytes!("../fixtures/registry/sumdb-tiles/3_000.p_4"),
        ),
        (
            "tile/8/2/004.p/46",
            include_bytes!("../fixtures/registry/sumdb-tiles/2_004.p_46"),
        ),
        (
            "tile/8/1/x001/070.p/213",
            include_bytes!("../fixtures/registry/sumdb-tiles/1_x001_070.p_213"),
        ),
        (
            "tile/8/0/x274/133.p/94",
            include_bytes!("../fixtures/registry/sumdb-tiles/0_x274_133.p_94"),
        ),
    ];
    tiles_needed(l)
        .unwrap()
        .into_iter()
        .map(|t| {
            let path = t.path().join("/");
            let data = files
                .iter()
                .find(|(p, _)| *p == path)
                .unwrap_or_else(|| panic!("no fixture for {path}"))
                .1;
            (t, data.to_vec())
        })
        .collect()
}

#[test]
fn the_default_key_is_the_databases_and_a_malformed_key_is_refused() {
    let k = parse_key(DEFAULT_KEY).unwrap();
    assert_eq!(k.name, "sum.golang.org");
    for bad in [
        "",
        "sum.golang.org",
        "sum.golang.org+033de0ae",
        // The hash does not belong to the name and key.
        "sum.golang.org+033de0af+Ac4zctda0e5eza+HJyk9SxEdh+s3Ux18htTTAD8OuAn8",
        "other.example+033de0ae+Ac4zctda0e5eza+HJyk9SxEdh+s3Ux18htTTAD8OuAn8",
        // Not Ed25519.
        "sum.golang.org+033de0ae+Ac4z",
        "sum.golang.org+zzzzzzzz+Ac4zctda0e5eza+HJyk9SxEdh+s3Ux18htTTAD8OuAn8",
        "sum golang+033de0ae+Ac4zctda0e5eza+HJyk9SxEdh+s3Ux18htTTAD8OuAn8",
    ] {
        assert!(parse_key(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn the_real_lookup_verifies_its_signature_and_its_place_in_the_log() {
    let l = parse_lookup(LOOKUP).unwrap();
    assert_eq!((l.id, l.size), (706_716, 70_178_142));
    assert!(l.record.starts_with("github.com/pkg/errors v0.9.1 h1:"));
    verify_note(&l, &parse_key(DEFAULT_KEY).unwrap()).unwrap();
    assert_eq!(tiles_needed(&l).unwrap().len(), 7);
    verify_inclusion(&l, &real_tiles(&l)).unwrap();
}

#[test]
fn a_record_that_is_not_what_was_logged_is_not_in_the_tree() {
    let l = parse_lookup(LOOKUP).unwrap();
    let tiles = real_tiles(&l);
    // The signature is over the tree head, so a changed record keeps it valid and fails the proof.
    let text = std::str::from_utf8(LOOKUP)
        .unwrap()
        .replacen("FEBLx1zS214", "AAAAx1zS214", 1);
    let forged = parse_lookup(text.as_bytes()).unwrap();
    verify_note(&forged, &parse_key(DEFAULT_KEY).unwrap()).unwrap();
    let e = verify_inclusion(&forged, &tiles).unwrap_err();
    assert!(e.contains("not in the tree"), "{e}");
    // Another record number.
    let moved = Lookup {
        id: l.id + 1,
        ..l.clone()
    };
    assert!(verify_inclusion(&moved, &real_tiles(&moved)).is_err());
    // A tile of the log that was changed, or is the wrong size, or is missing.
    let mut bad = tiles.clone();
    let t = *bad.keys().next().unwrap();
    // Every byte, since which hashes of a tile the proof reads depends on the record.
    bad.get_mut(&t).unwrap().iter_mut().for_each(|b| *b ^= 0xff);
    assert!(verify_inclusion(&l, &bad).is_err());
    let mut short = tiles.clone();
    short.get_mut(&t).unwrap().truncate(32);
    assert!(verify_inclusion(&l, &short).unwrap_err().contains("size"));
    let mut missing = tiles;
    missing.remove(&t);
    assert!(
        verify_inclusion(&l, &missing)
            .unwrap_err()
            .contains("not fetched")
    );
}

fn fresh_key() -> (ring::signature::Ed25519KeyPair, [u8; 32]) {
    use ring::signature::KeyPair as _;
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(pair.public_key().as_ref());
    (pair, pubkey)
}

#[test]
fn a_bad_signature_a_wrong_key_or_a_truncated_note_is_refused() {
    let key = parse_key(DEFAULT_KEY).unwrap();
    let l = parse_lookup(LOOKUP).unwrap();
    // One bit of the signature.
    let mut flipped = l.clone();
    flipped.signatures[0].1[10] ^= 1;
    assert!(
        verify_note(&flipped, &key)
            .unwrap_err()
            .contains("does not verify")
    );
    // A tree head with another size, under the same signature.
    let mut other = l.clone();
    other.note = other.note.replacen("70178142", "70178143", 1);
    assert!(
        verify_note(&other, &key)
            .unwrap_err()
            .contains("does not verify")
    );
    // Signed by a key that is not the one trusted (same name, other key).
    let (pair, pubkey) = fresh_key();
    let mut sig = key_hash("sum.golang.org", &pubkey).to_vec();
    sig.extend_from_slice(pair.sign(l.note.as_bytes()).as_ref());
    let forged = Lookup {
        signatures: vec![("sum.golang.org".into(), sig)],
        ..l.clone()
    };
    assert!(verify_note(&forged, &key).is_err(), "signed by another key");
    // A note signed by nobody the key names.
    let stranger = Lookup {
        signatures: vec![("evil.example".into(), l.signatures[0].1.clone())],
        ..l.clone()
    };
    assert!(
        verify_note(&stranger, &key)
            .unwrap_err()
            .contains("not signed by")
    );
    // Truncated answers.
    let text = std::str::from_utf8(LOOKUP).unwrap();
    for cut in [0, 10, 100, 200, 300, text.len() - 60] {
        let part = &text.as_bytes()[..cut];
        let ok = parse_lookup(part).and_then(|l| verify_note(&l, &key));
        assert!(ok.is_err(), "a lookup cut at {cut} was accepted");
    }
    for bad in [
        text.replacen("go.sum database tree", "go.sum database free", 1),
        text.replacen("70178142", "x", 1),
        text.replace('\u{2014}', "-"),
        text.replacen("706716", "99999999999", 1),
    ] {
        let refused = match parse_lookup(bad.as_bytes()) {
            Err(_) => true,
            Ok(l) => verify_note(&l, &key).is_err(),
        };
        assert!(refused);
    }
}

#[test]
fn a_log_the_configured_key_does_not_sign_is_refused_and_one_it_does_is_accepted() {
    // A one-record log signed by a fresh key, to show the checks pass for what they should.
    let (pair, pubkey) = fresh_key();
    let name = "sumdb.example";
    let hash = key_hash(name, &pubkey);
    let mut enc = vec![1u8];
    enc.extend_from_slice(&pubkey);
    let key_text = format!(
        "{name}+{:08x}+{}",
        u32::from_be_bytes(hash),
        B64.encode(enc)
    );
    let key = parse_key(&key_text).unwrap();
    let record = "m v1.0.0 h1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n";
    let root = record_hash(record.as_bytes());
    let note = format!("go.sum database tree\n1\n{}\n", B64.encode(root));
    let mut sig = hash.to_vec();
    sig.extend_from_slice(pair.sign(note.as_bytes()).as_ref());
    let body = format!("0\n{record}\n{note}\n\u{2014} {name} {}\n", B64.encode(sig));
    let l = parse_lookup(body.as_bytes()).unwrap();
    verify_note(&l, &key).unwrap();
    assert!(tiles_needed(&l).unwrap().is_empty());
    verify_inclusion(&l, &HashMap::new()).unwrap();
    // The default key does not trust it.
    assert!(verify_note(&l, &parse_key(DEFAULT_KEY).unwrap()).is_err());
}

#[test]
fn tile_paths_follow_the_logs_layout() {
    let p = |level, n, width| TileId { level, n, width }.path().join("/");
    assert_eq!(p(0, 0, 256), "tile/8/0/000");
    assert_eq!(p(0, 999, 256), "tile/8/0/999");
    assert_eq!(p(0, 1000, 256), "tile/8/0/x001/000");
    assert_eq!(p(0, 274_133, 94), "tile/8/0/x274/133.p/94");
    assert_eq!(p(1, 1_234_567, 256), "tile/8/1/x001/x234/567");
    assert_eq!(p(3, 0, 4), "tile/8/3/000.p/4");
}

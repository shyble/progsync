use progsync_core::diff;
use progsync_core::gc::{check, gc};
use progsync_core::object::Object;
use progsync_core::{ConflictKind, Hash, Identity, MetaUpdate, Repo};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

fn id(agent: &str) -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some(agent.into()) }
}

fn msg(m: &str) -> MetaUpdate {
    MetaUpdate { intent: Some(m.into()), ..Default::default() }
}

/// Deterministic pseudo-random bytes.
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        })
        .collect()
}

fn chunks_of(repo: &Repo, path: &str) -> Vec<Hash> {
    let tree = repo.working().unwrap().1.tree;
    let (_, h) = diff::lookup(&repo.store, tree, path).unwrap().unwrap();
    match repo.store.get(&h).unwrap() {
        Object::Chunked(c) => c.chunks.into_iter().map(|(h, _)| h).collect(),
        _ => panic!("{path} is not chunked"),
    }
}

fn age_everything(root: &Path) {
    let old = SystemTime::now() - Duration::from_secs(2 * 86_400);
    for d in fs::read_dir(root.join(".psync/objects")).unwrap().flatten() {
        for f in fs::read_dir(d.path()).unwrap().flatten() {
            fs::OpenOptions::new().write(true).open(f.path()).unwrap().set_modified(old).unwrap();
        }
    }
}

#[test]
fn large_files_are_chunked_and_edits_store_only_changed_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    fs::create_dir(&root).unwrap();
    let mut big = noise(12 << 20, 7);
    fs::write(root.join("big.bin"), &big).unwrap();
    fs::write(root.join("small.txt"), "small\n").unwrap();
    let repo = Repo::init(&root, id("a")).unwrap();
    repo.commit(&msg("first")).unwrap();
    repo.land(None).unwrap();

    // Small files keep the plain blob format.
    let tree = repo.working().unwrap().1.tree;
    let (_, small) = diff::lookup(&repo.store, tree, "small.txt").unwrap().unwrap();
    assert_eq!(small, Hash::of(b"\x02bsmall\n"));
    let before = chunks_of(&repo, "big.bin");
    assert!(before.len() >= 4, "{} chunks", before.len());
    assert_eq!(repo.store.get_blob(&diff::lookup(&repo.store, tree, "big.bin").unwrap().unwrap().1).unwrap(), big);

    // An edit in the middle stores one or two new chunks.
    big.splice(6 << 20..(6 << 20) + 5, *b"an edit");
    fs::write(root.join("big.bin"), &big).unwrap();
    repo.snapshot().unwrap();
    let after = chunks_of(&repo, "big.bin");
    let new = after.iter().filter(|c| !before.contains(c)).count();
    assert!((1..=2).contains(&new), "{new} new chunks of {}", after.len());

    // Diffs show it as binary without reading it.
    let st = repo.status().unwrap();
    let c = st.changes.iter().find(|c| c.path == "big.bin").unwrap();
    assert!(diff::unified(&repo.store, c).unwrap().starts_with("Binary files"));
    assert!(diff::line_stat(&repo.store, c).unwrap().binary);
    repo.commit(&msg("edit")).unwrap();
    repo.land(None).unwrap();

    // Checkout writes it back (another workspace, then a clone).
    let ws = repo.workspace_add("other", None, None).unwrap().info.path.unwrap();
    assert_eq!(fs::read(ws.join("big.bin")).unwrap(), big);
    let dst = dir.path().join("dst");
    let clone = Repo::clone_from(root.to_str().unwrap(), &dst, id("b")).unwrap();
    assert_eq!(fs::read(dst.join("big.bin")).unwrap(), big);
    assert!(check(&clone).unwrap().problems.is_empty());

    // gc keeps every chunk of every kept version; packed, they still read.
    age_everything(&root);
    gc(&repo, None, false).unwrap();
    let r = check(&repo).unwrap();
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!(r.packs, 1);
    for h in before.iter().chain(&after) {
        assert!(repo.store.read_raw(h).is_ok());
    }
    let tree = repo.working().unwrap().1.tree;
    assert_eq!(repo.store.get_blob(&diff::lookup(&repo.store, tree, "big.bin").unwrap().unwrap().1).unwrap(), big);

    // A missing chunk is reported.
    let gone = after[0];
    drop(clone);
    let hex = gone.to_hex();
    fs::remove_file(dst.join(".psync/objects").join(&hex[..2]).join(&hex[2..])).unwrap();
    let c = check(&Repo::discover_as(&dst, id("b")).unwrap()).unwrap();
    assert!(c.problems.iter().any(|p| p.contains(&format!("chunk {}: missing", gone.short()))), "{:?}", c.problems);
}

#[test]
fn put_reader_streams_and_reports_the_old_whole_blob_hash() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path(), id("a")).unwrap();
    let data = noise(5 << 20, 3);
    let (h, whole) = repo.store.put_reader(&mut &data[..]).unwrap();
    assert_eq!(whole, Some(Hash::of(&[b"\x02b".as_slice(), &data].concat())));
    assert_eq!(repo.store.put_blob(&data).unwrap(), h, "same contents, same hash either way");
    assert_eq!(repo.store.get_blob(&h).unwrap(), data);
    let mut out = Vec::new();
    repo.store.write_file_to(&h, &mut out).unwrap();
    assert_eq!(out, data);
    assert!(repo.store.get_small_file(&h).unwrap().is_none());
    // At the threshold a file is still one blob.
    let edge = noise(progsync_core::chunk::LARGE, 4);
    let (h, whole) = repo.store.put_reader(&mut &edge[..]).unwrap();
    assert!(whole.is_none());
    assert_eq!(h, Hash::of(&[b"\x02b".as_slice(), &edge].concat()));
}

#[test]
fn concurrent_edits_of_a_large_file_conflict_as_binary() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("main");
    fs::create_dir(&root).unwrap();
    let big = noise(6 << 20, 9);
    fs::write(root.join("big.bin"), &big).unwrap();
    let repo = Repo::init(&root, id("a")).unwrap();
    repo.commit(&msg("first")).unwrap();
    repo.land(None).unwrap();
    let ws = repo.workspace_add("b", None, None).unwrap().info.path.unwrap();

    let mut ours = big.clone();
    ours[100] ^= 1;
    fs::write(root.join("big.bin"), &ours).unwrap();
    repo.snapshot().unwrap();
    repo.commit(&msg("ours")).unwrap();
    repo.land(None).unwrap();
    drop(repo);

    let mut theirs = big.clone();
    theirs[5 << 20] ^= 1;
    fs::write(ws.join("big.bin"), &theirs).unwrap();
    let b = Repo::discover_as(&ws, id("b")).unwrap();
    b.snapshot().unwrap();
    b.commit(&msg("theirs")).unwrap();
    b.sync().unwrap();
    let w = b.working().unwrap().1;
    let conflicts: Vec<_> = b.log(b.working().unwrap().0, 5).unwrap().into_iter().flat_map(|(_, r)| r.conflicts).chain(w.conflicts).collect();
    assert!(conflicts.iter().any(|c| c.path == "big.bin" && c.kind == ConflictKind::Binary), "{conflicts:?}");
}

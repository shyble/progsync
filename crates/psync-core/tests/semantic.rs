use psync_core::repo::LandResult;
use psync_core::{Identity, MetaUpdate, Repo};
use std::fs;
use std::path::Path;

fn id(agent: &str) -> Identity {
    Identity { name: "Ada".into(), email: "ada@example.com".into(), agent: Some(agent.into()) }
}

fn msg(m: &str) -> MetaUpdate {
    MetaUpdate { intent: Some(m.into()), ..Default::default() }
}

fn open(p: &Path, agent: &str) -> Repo {
    let r = Repo::discover_as(p, id(agent)).unwrap();
    r.snapshot().unwrap();
    r
}

const LIB: &str = "pub fn shared() -> u32 {\n    1\n}\n";

#[test]
fn two_agents_appending_functions_land_without_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("app");
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/lib.rs"), LIB).unwrap();
    fs::write(main.join("notes.txt"), "shared\n").unwrap();
    let r = Repo::init(&main, id("lead")).unwrap();
    r.commit(&msg("start")).unwrap();
    assert!(matches!(r.land(None).unwrap(), LandResult::Landed { .. }));
    let a = r.workspace_add("a", None, None).unwrap().info.path.unwrap();
    let b = r.workspace_add("b", None, None).unwrap().info.path.unwrap();
    drop(r);

    for (ws, name) in [(&a, "from_a"), (&b, "from_b")] {
        fs::write(ws.join("src/lib.rs"), format!("{LIB}\npub fn {name}() -> u32 {{\n    2\n}}\n")).unwrap();
        open(ws, name).commit(&msg(&format!("add {name}"))).unwrap();
    }
    assert!(matches!(open(&a, "a").land(None).unwrap(), LandResult::Landed { .. }));
    let rb = open(&b, "b");
    match rb.land(None).unwrap() {
        LandResult::Landed { .. } => {}
        LandResult::Conflicts(s) => panic!("structural merge should resolve: {:?}", s.conflicts),
        LandResult::CheckFailed { .. } => unreachable!(),
    }
    let merged = fs::read_to_string(b.join("src/lib.rs")).unwrap();
    assert_eq!(
        merged,
        format!("{LIB}\npub fn from_a() -> u32 {{\n    2\n}}\n\npub fn from_b() -> u32 {{\n    2\n}}\n")
    );
    drop(rb);

    // Plain text keeps line-based conflicts.
    for (ws, line) in [(&a, "a"), (&b, "b")] {
        fs::write(ws.join("notes.txt"), format!("shared\n{line}\n")).unwrap();
        let r = open(ws, line);
        r.sync().unwrap();
        r.commit(&msg(&format!("{line}'s note"))).unwrap();
    }
    assert!(matches!(open(&a, "a").land(None).unwrap(), LandResult::Landed { .. }));
    assert!(matches!(open(&b, "b").land(None).unwrap(), LandResult::Conflicts(_)), "text files still conflict");
}

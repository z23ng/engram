use mem_search::MemSearchEngine;
use mem_server::{authorize, Action, ControlPlane, Role, ServerError};

fn open(dir: &std::path::Path) -> (ControlPlane, bool) {
    ControlPlane::open(dir, Box::new(MemSearchEngine::new_bm25_only)).unwrap()
}

#[test]
fn role_action_matrix_matches_design_table() {
    use Action::*;
    let table = [
        (Role::Admin, [true, true, true, true]),
        (Role::Dev, [true, true, true, false]),
        (Role::Pm, [true, true, true, false]),
        (Role::Bot, [true, false, false, false]),
    ];
    for (role, expected) in table {
        let got = [Read, Write, Branch, Manage].map(|a| role.allows(a));
        assert_eq!(got, expected, "{role:?}");
    }
}

#[test]
fn open_reports_new_database_only_once() {
    let dir = tempfile::tempdir().unwrap();
    assert!(open(dir.path()).1);
    assert!(!open(dir.path()).1);
}

#[test]
fn invalid_project_slugs_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (cp, _) = open(dir.path());
    for bad in [
        "",
        "../etc",
        "a/b",
        "Upper",
        "-lead",
        "dot.name",
        &"x".repeat(64),
    ] {
        assert!(cp.create_project(bad).is_err(), "accepted {bad:?}");
    }
    cp.create_project("alpha-1").unwrap();
    assert!(cp.create_project("alpha-1").is_err(), "duplicate accepted");
    assert_eq!(cp.list_projects().unwrap(), vec!["alpha-1".to_string()]);
    assert!(dir.path().join("projects/alpha-1.db").exists());
}

#[test]
fn key_create_verify_revoke_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (cp, _) = open(dir.path());
    cp.create_project("alpha").unwrap();
    let (info, plain) = cp
        .create_key("ci-bot", Role::Bot, Some(vec!["alpha".into()]), None)
        .unwrap();
    assert!(plain.starts_with("egm_") && plain.len() > 40);
    let key = cp.verify(&plain).unwrap();
    assert_eq!((key.id, key.role), (info.id, Role::Bot));
    assert!(authorize(&key, "alpha", Action::Read).is_ok());
    assert!(matches!(
        authorize(&key, "alpha", Action::Write),
        Err(ServerError::Forbidden(_))
    ));
    assert!(matches!(
        authorize(&key, "beta", Action::Read),
        Err(ServerError::Forbidden(_))
    ));

    assert!(cp.revoke_key(info.id).unwrap());
    assert!(matches!(cp.verify(&plain), Err(ServerError::Unauthorized)));
    assert!(cp
        .list_keys()
        .unwrap()
        .iter()
        .any(|k| k.id == info.id && k.revoked));
    assert!(matches!(
        cp.verify("egm_unknown"),
        Err(ServerError::Unauthorized)
    ));
}

#[test]
fn expired_key_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (cp, _) = open(dir.path());
    let (_, plain) = cp.create_key("old", Role::Dev, None, Some(1)).unwrap();
    assert!(matches!(cp.verify(&plain), Err(ServerError::Unauthorized)));
}

#[test]
fn plaintext_key_is_never_stored() {
    let dir = tempfile::tempdir().unwrap();
    let (cp, _) = open(dir.path());
    let (_, plain) = cp.create_key("admin", Role::Admin, None, None).unwrap();
    cp.verify(&plain).unwrap();
    drop(cp);
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap_or_default();
        assert!(!bytes.windows(plain.len()).any(|w| w == plain.as_bytes()));
    }
}

//! Structural validation that the declarative hardware matrix (`tests/hardware/*.toml`,
//! `distro/hardware/test-matrix.md`) stays complete and internally consistent. This cannot run
//! the matrix itself — that needs real hardware (`lege doctor --hardware`) — but it can guarantee
//! every declared case is well-formed and that every profile only references cases that actually
//! exist, which is exactly the kind of drift that silently rots a "declarative matrix" over time.

use std::{collections::BTreeSet, fs, path::PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

#[derive(serde::Deserialize)]
struct MatrixCase {
    id: String,
    profiles: Vec<String>,
    steps: Vec<String>,
    expect: Vec<String>,
}

#[derive(serde::Deserialize)]
struct HardwareProfile {
    id: String,
    #[serde(default)]
    test_matrix: Vec<String>,
}

fn load_matrix_cases() -> Vec<MatrixCase> {
    let dir = repo_root().join("tests/hardware");
    let mut cases = Vec::new();
    for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "toml") {
            continue;
        }
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let case: MatrixCase =
            toml::from_str(&text).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
        cases.push(case);
    }
    cases
}

fn load_profiles() -> Vec<HardwareProfile> {
    let dir = repo_root().join("distro/hardware/profiles");
    let mut profiles = Vec::new();
    for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "toml") {
            continue;
        }
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let profile: HardwareProfile =
            toml::from_str(&text).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
        profiles.push(profile);
    }
    profiles
}

#[test]
fn every_matrix_case_is_well_formed() {
    let cases = load_matrix_cases();
    assert!(
        cases.len() >= 8,
        "expected at least the 8 cases planning doc 01 §4 lists, found {}",
        cases.len()
    );
    for case in &cases {
        assert!(!case.id.is_empty(), "a matrix case has an empty id");
        assert!(
            !case.profiles.is_empty(),
            "{}: profiles must not be empty",
            case.id
        );
        assert!(
            !case.steps.is_empty(),
            "{}: steps must not be empty",
            case.id
        );
        assert!(
            !case.expect.is_empty(),
            "{}: expect (pass criteria) must not be empty",
            case.id
        );
    }
    let ids: BTreeSet<&str> = cases.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids.len(),
        cases.len(),
        "duplicate case id in tests/hardware/*.toml"
    );
}

#[test]
fn every_profile_test_matrix_entry_has_a_matching_case() {
    let known_case_ids: BTreeSet<String> = load_matrix_cases().into_iter().map(|c| c.id).collect();
    for profile in load_profiles() {
        for case_id in &profile.test_matrix {
            assert!(
                known_case_ids.contains(case_id),
                "profile {:?} references matrix case {case_id:?}, but no tests/hardware/*.toml declares that id",
                profile.id
            );
        }
    }
}

#[test]
fn every_matrix_case_only_references_profiles_that_exist() {
    let known_profile_ids: BTreeSet<String> = load_profiles().into_iter().map(|p| p.id).collect();
    for case in load_matrix_cases() {
        for profile_id in &case.profiles {
            assert!(
                known_profile_ids.contains(profile_id),
                "matrix case {:?} references profile {profile_id:?}, but no distro/hardware/profiles/*.toml declares that id",
                case.id
            );
        }
    }
}

#[test]
fn the_reference_profile_declares_every_case_planning_doc_01_lists() {
    let profiles = load_profiles();
    let hybrid = profiles
        .iter()
        .find(|p| p.id == "hybrid-intel-nvidia")
        .expect("distro/hardware/profiles/hybrid-intel-nvidia.toml must exist");
    let expected = [
        "intel-desktop-nvidia-offload",
        "nvidia-desktop",
        "external-monitor-each-output-path",
        "suspend-resume",
        "kernel-update-and-rollback",
        "driver-update-and-rollback",
        "wayland-and-xwayland-apps",
        "cuda-and-vulkan-where-installed",
    ];
    for case_id in expected {
        assert!(
            hybrid.test_matrix.iter().any(|id| id == case_id),
            "hybrid-intel-nvidia.toml's test_matrix is missing {case_id:?} (planning doc 01 §4)"
        );
    }
}

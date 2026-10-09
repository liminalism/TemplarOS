//! Lossless journal path encoding. Existing UTF-8 strings retain their shipped JSON form;
//! non-UTF-8 paths use canonical native-byte base64. Display text never names a file.
#[cfg(test)]
mod tests {
    use crate::operations::{OperationId, OperationKind, Options, Step, Survey, plan};
    use std::{os::unix::ffi::OsStringExt, path::PathBuf};
    #[test]
    fn durable_kind_steps_and_undo_preserve_native_bytes() {
        let source = PathBuf::from(std::ffi::OsString::from_vec(
            b"/source/name-\xff.pdf".to_vec(),
        ));
        let destination = PathBuf::from("/dest");
        let kind = OperationKind::Move {
            sources: vec![source.clone()],
            destination_dir: destination.clone(),
        };
        let mut operation = plan(
            OperationId::new(),
            kind,
            &Survey::default(),
            Options::default(),
        );
        operation.steps = vec![Step::RenameInPlace {
            from: source.clone(),
            to: destination.join(source.file_name().unwrap()),
        }];
        operation.skipped.push(source);
        let encoded = serde_json::to_string(&operation).unwrap();
        assert!(encoded.contains("path_b64"));
        assert_eq!(
            serde_json::from_str::<crate::operations::Operation>(&encoded).unwrap(),
            operation
        );
        let legacy = r#"{"kind":"copy","sources":["/source/x"],"destination_dir":"/dest"}"#;
        assert_eq!(
            serde_json::to_string(&serde_json::from_str::<OperationKind>(legacy).unwrap()).unwrap(),
            legacy
        );
    }

    #[test]
    fn surveyed_raw_relative_names_and_keep_both_are_lossless() {
        use crate::operations::{ConflictPolicy, survey};
        let fixture = tempfile::tempdir().unwrap();
        let from = fixture.path().join("from");
        let to = fixture.path().join("to");
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        let name = std::ffi::OsString::from_vec(b"name-\xff.pdf".to_vec());
        let source = from.join(&name);
        std::fs::create_dir(&source).unwrap();
        std::fs::write(
            source.join(std::ffi::OsString::from_vec(b"child-\xfe".to_vec())),
            b"body",
        )
        .unwrap();
        std::fs::write(to.join(&name), b"existing").unwrap();
        let surveyed = survey(std::slice::from_ref(&source), Some(&to));
        assert_eq!(surveyed.destination_names, [name.clone()].into());
        let json = serde_json::to_string(&surveyed).unwrap();
        assert!(json.contains("name_b64"));
        assert_eq!(serde_json::from_str::<Survey>(&json).unwrap(), surveyed);
        let operation = plan(
            OperationId::new(),
            OperationKind::Copy {
                sources: vec![source],
                destination_dir: to.clone(),
            },
            &surveyed,
            Options {
                conflict: ConflictPolicy::KeepBoth,
                ..Options::default()
            },
        );
        assert_eq!(
            operation.transfers[0].destination,
            to.join(std::ffi::OsString::from_vec(b"name-\xff (2).pdf".to_vec()))
        );
    }

    #[test]
    fn native_collision_fail_does_not_overwrite_or_remove_the_source() {
        use crate::{
            execute::{self, Cancel, ExecutorConfig},
            operations::{OperationState, survey},
        };
        let fixture = tempfile::tempdir().unwrap();
        let from = fixture.path().join("from");
        let to = fixture.path().join("to");
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        let name = std::ffi::OsString::from_vec(b"collision-\xff".to_vec());
        let source = from.join(&name);
        let destination = to.join(&name);
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&destination, b"old").unwrap();
        let paths = vec![source.clone()];
        let mut operation = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: paths.clone(),
                destination_dir: to.clone(),
            },
            &survey(&paths, Some(&to)),
            Options::default(),
        );
        let config = ExecutorConfig {
            data_home: fixture.path().join("data"),
            reserve_percent: 0,
            ..ExecutorConfig::default()
        };
        assert!(execute::run(&mut operation, &config, &Cancel::new(), |_| {}).is_err());
        assert_eq!(operation.state, OperationState::Failed);
        assert_eq!(std::fs::read(&source).unwrap(), b"new");
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");
    }
}

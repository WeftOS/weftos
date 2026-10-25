//! `pack_cog` rejects bad `cog.toml` declarations (cog plan P1).

use super::*;

fn pack(toml: &str) -> Result<(), PackError> {
    let tmp = tempfile::tempdir().unwrap();
    let cog_dir = tmp.path().join("cog");
    std::fs::create_dir_all(&cog_dir).unwrap();
    std::fs::write(cog_dir.join("cog.toml"), toml).unwrap();
    std::fs::write(tmp.path().join("bin"), b"\x7fELF test").unwrap();
    let input = CogPackInput {
        cog_dir,
        binaries: vec![("aarch64".into(), tmp.path().join("bin"))],
        source: PackageSource {
            commit: Some("0000000".into()),
            ..Default::default()
        },
        cognitum_record: None,
        redistributable: false,
        provenance: None,
        allow_no_provenance: true,
    };
    pack_cog(&input, &tmp.path().join("out")).map(|_| ())
}

const HEAD: &str = "[cog]\nid = \"decl-test\"\nversion = \"0.1.0\"\n";

#[test]
fn declared_topics_and_interfaces_pack() {
    let toml = format!(
        "{HEAD}interfaces = [\"audio-in\"]\n[[outputs]]\ntopic = \"audio.pcm\"\n\
         schema = \"pcm.s16le.v1\"\nclass = \"stream\"\n"
    );
    pack(&toml).unwrap();
}

#[test]
fn bad_declarations_fail_at_pack_time_with_the_reason() {
    for (extra, want) in [
        ("interfaces = [\"pcie\"]\n", "unknown interface"),
        (
            "[[outputs]]\ntopic = \"x\"\nschema = \"nope.v1\"\nclass = \"reading\"\n",
            "unregistered schema",
        ),
        (
            "[[inputs]]\ntopic = \"Bad Topic\"\nschema = \"telemetry.v1\"\n",
            "bad topic",
        ),
    ] {
        match pack(&format!("{HEAD}{extra}")) {
            Err(PackError::Input(m)) => assert!(m.contains(want), "{m}"),
            other => panic!("expected Input error containing {want:?}, got {other:?}"),
        }
    }
}

use super::*;

const MESH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const ULID: &str = "01K6ZQ8N3T4V5W6X7Y8Z9A0B1C";

fn mesh() -> Authority {
    let mut id = [0u8; 32];
    hex::decode_to_slice(MESH, &mut id).unwrap();
    Authority(id)
}

#[test]
fn every_accepted_name_round_trips_byte_for_byte() {
    let names = [
        format!("weftos://{MESH}/projects/{ULID}"),
        format!("weftos://{MESH}/projects/{ULID}/repos/tools"),
        format!("weftos://{MESH}/projects/{ULID}/repos/a.b_c~d-e"),
        format!("weftos://{MESH}/nodes/87ee73837d4ba020e074790b42767a34/services/workload-host"),
        format!("weftos://{MESH}/cogs/weft-ecg-scope?rev=sha256:{MESH}"),
        format!("weftos://{MESH}/memory/notes-2026?rev=sha256:{MESH}&view=overview"),
        format!("weftos://{MESH}/companies/c1"),
        format!("weftos://{MESH}/teams/Core/members/alice"),
        format!("weftos://{MESH}/agents/steward?rev=sha256:{MESH}"),
        format!("weftos://{MESH}/instances/{ULID}"),
    ];
    for n in names {
        let u = WeftosUri::parse(&n).unwrap_or_else(|e| panic!("{n}: {e}"));
        assert_eq!(u.to_string(), n, "round trip");
    }
    let u = WeftosUri::parse(&format!("weftos://{MESH}/projects/{ULID}/repos/tools")).unwrap();
    assert_eq!(u.authority, mesh());
    assert_eq!(u.kind, Kind::Projects);
    assert_eq!(u.project_repo(), Some((ULID, "tools")));
    assert_eq!(WeftosUri::parse(&format!("weftos://{MESH}/projects/{ULID}")).unwrap().project_repo(), Some((ULID, ".")));
    assert_eq!(WeftosUri::for_project_repo(mesh(), ULID, ".").to_string(), format!("weftos://{MESH}/projects/{ULID}"));
    assert_eq!(WeftosUri::for_project_repo(mesh(), ULID, "tools").to_string(), format!("weftos://{MESH}/projects/{ULID}/repos/tools"));
    for k in Kind::ALL {
        assert_eq!(Kind::parse(k.1), Some(k.0));
    }
}

#[test]
fn the_reject_table() {
    let m = MESH;
    let cases: Vec<(String, UriError)> = vec![
        (format!("http://{m}/projects/x"), UriError::Scheme),
        (format!("WEFTOS://{m}/projects/x"), UriError::Scheme),
        (format!("Weftos://{m}/projects/x"), UriError::Scheme),
        (format!("weftos://{m}/projects/x%2Fy"), UriError::Characters),
        (format!("weftos://{m}/projects/x\u{e9}"), UriError::Characters),
        (format!("weftos://{m}/projects/x y"), UriError::Characters),
        (format!("weftos://{m}/projects/x#top"), UriError::Structure),
        (format!("weftos://user@{m}/projects/x"), UriError::Structure),
        (format!("weftos://{m}:9470/projects/x"), UriError::Structure),
        // Aliases are dashboard labels, never an authority (owner, 2026-10-09).
        ("weftos://weavelogic/projects/x".to_owned(), UriError::Authority),
        ("weftos://lab.local/projects/x".to_owned(), UriError::Authority),
        ("weftos://m/projects/x".to_owned(), UriError::Authority),
        (format!("weftos://{}/projects/x", m.to_ascii_uppercase()), UriError::Authority),
        (format!("weftos://{}/projects/x", &m[..63]), UriError::Authority),
        (format!("weftos://{m}0/projects/x"), UriError::Authority),
        ("weftos:///projects/x".to_owned(), UriError::Authority),
        (format!("weftos://{m}/Projects/x"), UriError::Kind),
        (format!("weftos://{m}/things/x"), UriError::Kind),
        (format!("weftos://{m}/instance/x"), UriError::Kind),
        (format!("weftos://{m}/Instances/x"), UriError::Kind),
        (format!("weftos://{m}/projects"), UriError::Segment),
        (format!("weftos://{m}/projects/"), UriError::Segment),
        (format!("weftos://{m}/projects/x/"), UriError::Segment),
        (format!("weftos://{m}/projects/x//y"), UriError::Segment),
        (format!("weftos://{m}/projects/./y"), UriError::Segment),
        (format!("weftos://{m}/projects/x/.."), UriError::Segment),
        (format!("weftos://{m}/projects/x/re$po"), UriError::Segment),
        (format!("weftos://{m}"), UriError::Segment),
        (format!("weftos://{m}/projects/x?view=content"), UriError::Query),
        (format!("weftos://{m}/projects/x?rev=sha256:abc"), UriError::Query),
        (format!("weftos://{m}/projects/x?rev=SHA256:{m}"), UriError::Query),
        (format!("weftos://{m}/projects/x?rev=sha256:{m}&rev=sha256:{m}"), UriError::Query),
        (format!("weftos://{m}/projects/x?rev=sha256:{m}&view=full"), UriError::Query),
        (format!("weftos://{m}/projects/x?rev=sha256:{}", m.to_ascii_uppercase()), UriError::Query),
        (format!("weftos://{m}/projects/x?view=content&rev=sha256:{m}"), UriError::Query),
        (format!("weftos://{m}/projects/x?x=1"), UriError::Query),
    ];
    for (input, want) in &cases {
        assert_eq!(WeftosUri::parse(input), Err(*want), "{input}");
    }
    // Size caps: 32 segments after the id and 2,048 bytes in all are the most.
    let thirty_two = format!("weftos://{m}/memory/store{}", "/s".repeat(32));
    assert!(WeftosUri::parse(&thirty_two).is_ok());
    assert_eq!(WeftosUri::parse(&format!("{thirty_two}/s")), Err(UriError::Segment));
    assert!(WeftosUri::parse(&format!("weftos://{m}/memory/{}", "a".repeat(128))).is_ok());
    assert_eq!(WeftosUri::parse(&format!("weftos://{m}/memory/{}", "a".repeat(129))), Err(UriError::Segment));
    let mut big = format!("weftos://{m}/memory/x");
    while big.len() + 129 <= 2048 {
        big.push_str(&format!("/{}", "b".repeat(128)));
    }
    let at_cap = format!("{big}/{}", "c".repeat(2048 - big.len() - 1));
    assert_eq!(at_cap.len(), 2048);
    assert!(WeftosUri::parse(&at_cap).is_ok(), "exactly 2048 bytes parses");
    assert_eq!(WeftosUri::parse(&format!("{at_cap}c")), Err(UriError::Segment), "2049 bytes does not");
    // The old three-segment node form is not a name.
    assert!(WeftosUri::parse(&format!("weftos://{MESH}/{ULID}/.")).is_err());
    assert!(WeftosUri::parse(&format!("weftos://{MESH}/{ULID}/tools")).is_err());
    // A project repository needs a ULID and `repos/<plain dir>`; other paths are not repositories.
    assert_eq!(WeftosUri::parse(&format!("weftos://{m}/projects/not-a-ulid")).unwrap().project_repo(), None);
    assert_eq!(WeftosUri::parse(&format!("weftos://{m}/projects/{ULID}/repos")).unwrap().project_repo(), None);
    assert_eq!(WeftosUri::parse(&format!("weftos://{m}/projects/{ULID}/repos/.hidden")).unwrap().project_repo(), None);
    assert_eq!(WeftosUri::parse(&format!("weftos://{m}/projects/{ULID}/files/x")).unwrap().project_repo(), None);
    assert_eq!(WeftosUri::parse(&format!("weftos://{m}/cogs/{ULID}")).unwrap().project_repo(), None);
}

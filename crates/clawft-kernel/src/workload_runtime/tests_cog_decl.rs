//! `cog.toml` interface and topic declarations (cog plan P1).

use clawft_types::cog_topics::{InputPolicy, TopicClass};

use super::cog_spec::CogSpec;

const BASE: &str = "[cog]\nid = \"mic\"\nversion = \"0.1.0\"\n";

fn parse(extra: &str) -> Result<CogSpec, String> {
    CogSpec::parse(format!("{BASE}{extra}").as_bytes()).map_err(|e| e.to_string())
}

fn err(extra: &str) -> String {
    parse(extra).expect_err("should be rejected")
}

#[test]
fn legacy_toml_parses_with_empty_declarations() {
    let s = parse(
        "name = \"T\"\nhardware_requirement = [\"pi-zero-2w\", \"v0-appliance\"]\n\
         [console]\nallowed_commands = [\"--once\"]\n",
    )
    .unwrap();
    assert!(s.interfaces.is_empty() && s.outputs.is_empty() && s.inputs.is_empty());
    assert_eq!(s.console.allowed_commands, ["--once"]);
}

#[test]
fn declarations_parse_into_typed_structs() {
    let s = parse(
        "interfaces = [\"audio-in\", \"i2c\"]\n\
         [[outputs]]\ntopic = \"audio.pcm\"\nschema = \"pcm.s16le.v1\"\nclass = \"stream\"\nrate = \"16000 Hz\"\n\
         [[outputs]]\ntopic = \"levels\"\nschema = \"telemetry.v1\"\nclass = \"reading\"\n\
         [[inputs]]\ntopic = \"audio.pcm\"\nschema = \"pcm.s16le.v1\"\npolicy = \"block\"\n\
         [[inputs]]\ntopic = \"cmd\"\nschema = \"senml.v1\"\n",
    )
    .unwrap();
    assert_eq!(s.interfaces, ["audio-in", "i2c"]);
    assert_eq!(s.outputs[0].class, TopicClass::Stream);
    assert_eq!(s.outputs[0].rate.as_deref(), Some("16000 Hz"));
    assert_eq!(s.outputs[1].rate, None);
    assert_eq!(s.inputs[0].policy, InputPolicy::Block);
    assert_eq!(s.inputs[1].policy, InputPolicy::DropOldest);
}

#[test]
fn interfaces_are_checked() {
    assert!(parse("interfaces = [\"none\"]").is_ok());
    assert!(err("interfaces = [\"pcie\"]").contains("unknown interface"));
    assert!(err("interfaces = [\"i2c\", \"i2c\"]").contains("twice"));
    assert!(err("interfaces = [\"none\", \"i2c\"]").contains("none"));
}

#[test]
fn output_errors_are_clear() {
    let o = |body: &str| format!("[[outputs]]\n{body}");
    let ok = "topic = \"a.b\"\nschema = \"telemetry.v1\"\nclass = \"reading\"\n";
    assert!(err(&o("topic = \"A\"\nschema = \"telemetry.v1\"\nclass = \"reading\"\n")).contains("bad topic"));
    assert!(err(&o("topic = \"a\"\nschema = \"nope.v1\"\nclass = \"reading\"\n")).contains("unregistered schema"));
    assert!(err(&o("topic = \"a\"\nschema = \"telemetry.v1\"\nclass = \"huge\"\n")).contains("class"));
    assert!(err(&o("topic = \"a\"\nschema = \"telemetry.v1\"\n")).contains("class"));
    let long = format!("{ok}rate = \"{}\"\n", "x".repeat(65));
    assert!(err(&o(&long)).contains("rate"));
    assert!(err(&o(&format!("{ok}extra = 1\n"))).contains("extra"));
    assert!(err(&format!("{}{}", o(ok), o(ok))).contains("duplicate"));
}

#[test]
fn input_errors_and_limits() {
    let i = "[[inputs]]\ntopic = \"a\"\nschema = \"telemetry.v1\"\npolicy = \"keep\"\n";
    assert!(err(i).contains("policy"));
    let out = "[[outputs]]\ntopic = \"t0\"\nschema = \"telemetry.v1\"\nclass = \"reading\"\n";
    let many_out: String = (0..17)
        .map(|n| out.replace("t0", &format!("t{n}")))
        .collect();
    assert!(err(&many_out).contains("more than 16"));
    let inp = "[[inputs]]\ntopic = \"a\"\nschema = \"telemetry.v1\"\n";
    assert!(err(&inp.repeat(17)).contains("more than 16"));
    // Duplicate input topics are allowed (only outputs are unique); 16 is the cap.
    assert!(parse(&inp.repeat(16)).is_ok());
}

#[test]
fn device_selectors_cannot_name_files_or_nodes_no_cog_may_own() {
    // A socket must live in a system runtime directory: no /etc/passwd, no relative or traversing path.
    for p in ["/etc/passwd", "/tmp/x.sock", "/run", "/run/../etc/passwd", "run/x.sock", "/runx/a.sock"] {
        assert!(err(&format!("[[devices]]\nsocket = \"{p}\"\n")).contains("socket"), "{p}");
    }
    // The container runtimes and systemd are not cog devices even under /run.
    for p in ["/run/docker.sock", "/var/run/docker.sock", "/run/containerd/containerd.sock", "/run/systemd/private", "/run/dbus/system_bus_socket"] {
        assert!(err(&format!("[[devices]]\nsocket = \"{p}\"\n")).contains("no cog may own"), "{p}");
    }
    assert!(parse("[[devices]]\nsocket = \"/run/arduino-router.sock\"\n").is_ok());
    // Memory, disks, terminals and loop devices, by name or through a glob that could name them.
    for p in ["/dev/mem", "/dev/kmem", "/dev/port", "/dev/sda", "/dev/sda*", "/dev/nvme0n1", "/dev/mmcblk0", "/dev/tty", "/dev/console", "/dev/loop0", "/dev/loop*", "/dev/dm-0", "/dev/ptmx"] {
        assert!(err(&format!("[[devices]]\npath = \"{p}\"\n")).contains("no cog may own"), "{p}");
    }
    // A wildcard that is too broad to be a selector.
    for p in ["/dev/*", "/dev/s*", "/dev/??"] {
        assert!(err(&format!("[[devices]]\npath = \"{p}\"\n")).contains("literal"), "{p}");
    }
    assert!(parse("[[devices]]\npath = \"/dev/video*\"\n[[devices]]\npath = \"/dev/snd/pcmC*D*c\"\n").is_ok());
}

#[test]
fn netdev_devices_parse_and_are_checked() {
    use super::cog_devices::DeviceSel;
    let s = parse("[[devices]]\nnetdev = \"can*\"\n[[devices]]\npath = \"/dev/video0\"\n").unwrap();
    assert_eq!(s.devices[0].sel, DeviceSel::Netdev("can*".into()));
    assert_eq!(s.devices[0].sel.label(), "net:can*");
    assert!(err("[[devices]]\nnetdev = \"CAN0\"\n").contains("netdev"));
    assert!(err("[[devices]]\nnetdev = \"a/b\"\n").contains("netdev"));
    assert!(err("[[devices]]\nnetdev = \"averyveryverylongname\"\n").contains("netdev"));
    assert!(err("[[devices]]\nnetdev = \"can0\"\npath = \"/dev/x\"\n").contains("exactly one"));
}

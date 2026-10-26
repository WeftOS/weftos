//! Read a running process's REAL environment, for tests of the child
//! supervisor (ADR-103 Phase 2 F; reused by package G).
//!
//! `std::env::remove_var` in a child does not hide anything from the
//! outside: `/proc/<pid>/environ` (Linux) and `ps eww` (macOS) show the block
//! the process was started with. The only way a child cannot leak a key is
//! to be spawned without it (`Command::env_clear()` plus an allow-list). A
//! supervisor test should spawn the child, then call [`process_environment`]
//! on its pid and assert with [`leaked`] that no secret appears.

use std::io;

/// The environment `pid` was started with, as text (`KEY=VALUE` per line on
/// Linux; the `ps eww` command-plus-environment line on macOS). Both are for
/// substring checks, not parsing. Only same-uid processes are readable.
pub fn process_environment(pid: u32) -> io::Result<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read(format!("/proc/{pid}/environ"))?;
        Ok(raw
            .split(|b| *b == 0)
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .collect::<Vec<_>>()
            .join("\n"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::process::Command;
        let out = Command::new("ps")
            .args(["eww", "-o", "command=", "-p", &pid.to_string()])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!("ps exited {}", out.status)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// The `needles` (secret values, or variable names) that appear in `env`.
pub fn leaked<'a>(env: &str, needles: &[&'a str]) -> Vec<&'a str> {
    needles.iter().copied().filter(|n| env.contains(n)).collect()
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    const MARK: &str = "WEFT_ENV_PROBE_MARK";
    const SECRET: &str = "sk-fake-probe-9f3a";

    /// The child half: when launched by the tests below it just sleeps so its
    /// environment can be read. A normal run returns at once.
    #[test]
    #[ignore = "helper process for env_probe tests"]
    fn probe_sleeper() {
        if std::env::var_os(MARK).is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    fn spawn(with_key: bool) -> std::process::Child {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "env_probe::tests::probe_sleeper", "--ignored"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env(MARK, "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if with_key {
            cmd.env("OPENAI_API_KEY", SECRET);
        }
        cmd.spawn().expect("spawn probe child")
    }

    fn read_when_started(child: &std::process::Child) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(env) = process_environment(child.id())
                && env.contains(MARK)
            {
                return env;
            }
            assert!(Instant::now() < deadline, "could not read the child's environment");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn the_probe_sees_a_key_that_was_passed_and_not_one_that_was_not() {
        let mut leaky = spawn(true);
        let mut clean = spawn(false);
        let leaky_env = read_when_started(&leaky);
        let clean_env = read_when_started(&clean);
        let _ = leaky.kill();
        let _ = clean.kill();
        let _ = leaky.wait();
        let _ = clean.wait();
        assert_eq!(
            leaked(&leaky_env, &[SECRET]),
            [SECRET],
            "probe must see a passed key"
        );
        assert!(
            leaked(&clean_env, &[SECRET, "OPENAI_API_KEY"]).is_empty(),
            "{clean_env}"
        );
    }
}

//! `smart-turn-compare` — replay WAV recordings through the Talk Mode turn gate
//! beside Smart Turn and report where they disagree. Offline; changes nothing
//! at runtime. Needs `--features onnx` and a Smart Turn ONNX file (the model
//! path is an argument; weights are never fetched).
//!
//! ```text
//! smart-turn-compare --model PATH [--threshold 0.5] [--short-silence-ms 500]
//!     [--max-silence-ms 2500] [--step-ms 100] [--late-margin-ms 300]
//!     [--out-json report.json] [--out-md report.md] <file.wav | dir>...
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clawft_voice_onnx::smart_turn::SmartTurnEndpoint;
use clawft_voice_onnx::turn_compare::{CompareConfig, compare_audio};
use clawft_voice_onnx::turn_compare_report::{to_json, to_markdown};
use clawft_voice_onnx::wav_input::load_wav_16k;

struct Args {
    model: PathBuf,
    cfg: CompareConfig,
    out_json: Option<PathBuf>,
    out_md: Option<PathBuf>,
    inputs: Vec<PathBuf>,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut cfg = CompareConfig::default();
    let (mut model, mut out_json, mut out_md, mut inputs) = (None, None, None, Vec::new());
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        let num = |s: String, name: &str| {
            s.parse::<f64>()
                .map_err(|_| format!("{name}: bad number {s:?}"))
        };
        match a.as_str() {
            "--model" => model = Some(PathBuf::from(val("--model")?)),
            "--out-json" => out_json = Some(PathBuf::from(val("--out-json")?)),
            "--out-md" => out_md = Some(PathBuf::from(val("--out-md")?)),
            "--threshold" => cfg.threshold = num(val("--threshold")?, "--threshold")? as f32,
            "--short-silence-ms" => {
                cfg.short_silence_ms = num(val("--short-silence-ms")?, a)? as u32
            }
            "--max-silence-ms" => cfg.max_silence_ms = num(val("--max-silence-ms")?, a)? as u32,
            "--step-ms" => cfg.step_ms = num(val("--step-ms")?, a)? as u32,
            "--late-margin-ms" => cfg.late_margin_ms = num(val("--late-margin-ms")?, a)? as u32,
            f if f.starts_with("--") => return Err(format!("unknown flag {f}")),
            path => inputs.push(PathBuf::from(path)),
        }
    }
    cfg.validate()?;
    let model = model.ok_or("--model PATH is required")?;
    if inputs.is_empty() {
        return Err("give at least one WAV file or directory".into());
    }
    Ok(Args {
        model,
        cfg,
        out_json,
        out_md,
        inputs,
    })
}

/// Expand files and directories (non-recursive, sorted) into WAV paths.
fn collect_wavs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for p in inputs {
        if p.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(p)
                .map_err(|e| format!("{}: {e}", p.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|f| f.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")))
                .collect();
            found.sort();
            if found.is_empty() {
                return Err(format!("{}: no .wav files", p.display()));
            }
            out.extend(found);
        } else {
            out.push(p.clone());
        }
    }
    Ok(out)
}

fn run(argv: &[String]) -> Result<(), String> {
    let args = parse_args(argv)?;
    let mut model = SmartTurnEndpoint::with_model(&args.model);
    if !model.is_runtime_available() {
        return Err(format!(
            "Smart Turn unavailable (model {} missing/unloadable, or built without `--features onnx`)",
            args.model.display()
        ));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    for path in collect_wavs(&args.inputs)? {
        let samples = load_wav_16k(&path)?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        results.push(rt.block_on(compare_audio(&name, &samples, &args.cfg, &mut model))?);
    }
    let label = args.model.display().to_string();
    let md = to_markdown(&results, &args.cfg, &label);
    if let Some(p) = &args.out_json {
        let json = serde_json::to_string_pretty(&to_json(&results, &args.cfg, &label))
            .map_err(|e| e.to_string())?;
        write(p, &json)?;
    }
    if let Some(p) = &args.out_md {
        write(p, &md)?;
    }
    println!("{md}");
    Ok(())
}

fn write(path: &Path, body: &str) -> Result<(), String> {
    std::fs::write(path, body).map_err(|e| format!("{}: {e}", path.display()))
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match run(&argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("smart-turn-compare: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_flags_and_inputs() {
        let args = parse_args(&a(&["--model", "m.onnx", "--threshold", "0.7", "x.wav"])).unwrap();
        assert_eq!(args.cfg.threshold, 0.7);
        assert_eq!(args.inputs, vec![PathBuf::from("x.wav")]);
    }

    #[test]
    fn refuses_bad_invocations() {
        assert!(parse_args(&a(&["x.wav"])).is_err(), "model required");
        assert!(parse_args(&a(&["--model", "m"])).is_err(), "input required");
        assert!(parse_args(&a(&["--model", "m", "--bogus", "x.wav"])).is_err());
        assert!(parse_args(&a(&["--model", "m", "--threshold", "2", "x.wav"])).is_err());
        assert!(parse_args(&a(&["--model", "m", "--step-ms", "abc", "x.wav"])).is_err());
    }

    #[test]
    fn empty_directory_is_refused() {
        let d = std::env::temp_dir().join(format!("stc-empty-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(collect_wavs(&[d]).unwrap_err().contains("no .wav files"));
    }
}

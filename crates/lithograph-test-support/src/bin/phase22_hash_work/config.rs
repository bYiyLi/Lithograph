use std::path::PathBuf;

use crate::Result;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Smoke,
    Full,
    Long,
    Single,
    Probe,
}

impl Mode {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::Full => "full",
            Self::Long => "long",
            Self::Single => "single",
            Self::Probe => "probe",
        }
    }
}

pub(super) struct Config {
    pub(super) manifest: PathBuf,
    pub(super) output: PathBuf,
    pub(super) provenance: Option<PathBuf>,
    pub(super) case: String,
    pub(super) mode: Mode,
    pub(super) memory_limit: Option<u64>,
    pub(super) sqlite_source_id: Option<String>,
    pub(super) before: bool,
}

impl Config {
    pub(super) fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut config = Self {
            manifest: PathBuf::new(),
            output: PathBuf::new(),
            provenance: None,
            case: "small24".to_owned(),
            mode: Mode::Smoke,
            memory_limit: None,
            sqlite_source_id: None,
            before: false,
        };
        while let Some(argument) = args.next() {
            if argument == "--before" {
                config.before = true;
                continue;
            }
            let value = args.next().ok_or("missing CLI argument value")?;
            match argument.as_str() {
                "--manifest" => config.manifest = value.into(),
                "--output" => config.output = value.into(),
                "--provenance" => config.provenance = Some(value.into()),
                "--sqlite-source-id" => config.sqlite_source_id = Some(value),
                "--case" if matches!(value.as_str(), "full" | "small24") => {
                    config.case = value;
                }
                "--mode" => config.mode = parse_mode(&value)?,
                "--memory-limit-bytes" => {
                    config.memory_limit = Some(value.parse::<u64>()?);
                }
                _ => return Err(format!("unknown argument or value: {argument} {value}").into()),
            }
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.manifest.as_os_str().is_empty() || self.output.as_os_str().is_empty() {
            return Err("usage: lithograph-phase22-hash-work --manifest PATH --output NEW_PATH --case full|small24 --mode smoke|full|long|single|probe [--memory-limit-bytes N] [--before] [--provenance PATH] [--sqlite-source-id EXPECTED]".into());
        }
        if self.output.exists() {
            return Err("output exists; retain prior evidence and choose a new path".into());
        }
        if self.mode == Mode::Full && self.case != "small24" {
            return Err("full mode is only for small24; large output uses long or independent single processes".into());
        }
        if self.memory_limit == Some(0) {
            return Err("memory limit must be positive".into());
        }
        if self
            .sqlite_source_id
            .as_ref()
            .is_some_and(|value| value.is_empty())
        {
            return Err("expected SQLite source ID must not be empty".into());
        }
        Ok(())
    }
}

fn parse_mode(value: &str) -> Result<Mode> {
    match value {
        "smoke" => Ok(Mode::Smoke),
        "full" => Ok(Mode::Full),
        "long" => Ok(Mode::Long),
        "single" => Ok(Mode::Single),
        "probe" => Ok(Mode::Probe),
        _ => Err("mode must be smoke, full, long, single, or probe".into()),
    }
}

use std::path::PathBuf;

use crate::Result;

#[derive(Clone, Copy)]
pub(super) enum Mode {
    Full,
    Smoke,
    Baseline,
}

impl Mode {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Smoke => "smoke",
            Self::Baseline => "baseline-three-fresh",
        }
    }

    pub(super) fn samples(self) -> (usize, usize, usize) {
        match self {
            Self::Full => (5, 20, 100),
            Self::Smoke => (1, 1, 1),
            Self::Baseline => (0, 0, 3),
        }
    }
}

pub(super) struct Config {
    pub(super) manifest: PathBuf,
    pub(super) output: PathBuf,
    pub(super) case: Option<String>,
    pub(super) provenance: Option<PathBuf>,
    pub(super) mode: Mode,
}

impl Config {
    pub(super) fn parse() -> Result<Self> {
        let mut arguments = std::env::args().skip(1);
        let mut config = Self {
            manifest: PathBuf::new(),
            output: PathBuf::new(),
            case: None,
            provenance: None,
            mode: Mode::Full,
        };
        while let Some(argument) = arguments.next() {
            let value = arguments
                .next()
                .ok_or_else(|| format!("missing value for {argument}"))?;
            match argument.as_str() {
                "--manifest" => config.manifest = value.into(),
                "--output" => config.output = value.into(),
                "--case" => config.case = Some(value),
                "--provenance" => config.provenance = Some(value.into()),
                "--mode" => {
                    config.mode = match value.as_str() {
                        "full" => Mode::Full,
                        "smoke" => Mode::Smoke,
                        "baseline" => Mode::Baseline,
                        _ => return Err("mode must be full, smoke, or baseline".into()),
                    }
                }
                _ => return Err(format!("unknown argument {argument}").into()),
            }
        }
        if config.manifest.as_os_str().is_empty() || config.output.as_os_str().is_empty() {
            return Err("usage: lithograph-phase22-join-work --manifest PATH --output NEW_PATH [--case NAME] [--mode full|smoke|baseline] [--provenance PATH]".into());
        }
        if config.output.exists() {
            return Err(
                "output already exists; preserve previous evidence and choose a new path".into(),
            );
        }
        Ok(config)
    }
}

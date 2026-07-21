use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use crate::palette::PaletteMode;

#[derive(Debug, Parser)]
#[command(
    name = "agmsg-tui",
    version,
    about = "agmsg terminal user interface",
    after_help = "ENV (not a flag):\n  AGMSG_IDENTITY  own agent identity name. Unset disables the Agents-screen\n                  self-reset guard, the own-message '▏' marker, and the\n                  composer 'from' default (falls back to roster[0])."
)]
pub struct Cli {
    /// messages.db のパス
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// team config ディレクトリ
    #[arg(long)]
    pub teams_dir: Option<PathBuf>,

    /// agmsg scripts ディレクトリ
    #[arg(long)]
    pub scripts_dir: Option<PathBuf>,

    /// agmsg-audit のパス
    #[arg(long)]
    pub audit_script: Option<PathBuf>,

    /// Markdown report の保存先
    #[arg(long)]
    pub report_dir: Option<PathBuf>,

    /// 実DBを読み取り専用で検査し、team数を出力して終了する
    #[arg(long)]
    pub diagnose: bool,

    /// 80x24 のメモリ内端末へ初回描画して終了する
    #[arg(long, hide = true)]
    pub startup_probe: bool,

    /// 全UIから色を除去する (NO_COLOR env でも同じ効果、このflagが優先)
    #[arg(long)]
    pub no_color: bool,

    /// color palette (safe = 色覚多様性対応 Okabe-Ito palette)
    #[arg(long, value_enum)]
    pub palette: Option<PaletteArg>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum PaletteArg {
    Default,
    Safe,
}

impl From<PaletteArg> for PaletteMode {
    fn from(value: PaletteArg) -> Self {
        match value {
            PaletteArg::Default => PaletteMode::Default,
            PaletteArg::Safe => PaletteMode::Safe,
        }
    }
}

/// Resolves the effective NO_COLOR / palette-mode settings from CLI flags
/// (highest priority) and their env var fallbacks — `--no-color`/`--palette`
/// over `NO_COLOR`/`AGMSG_TUI_PALETTE`, per S10-1/S10-2.
pub fn resolve_palette(cli: &Cli) -> (bool, PaletteMode) {
    let no_color = cli.no_color || env::var_os("NO_COLOR").is_some();
    let mode = cli.palette.map(PaletteMode::from).unwrap_or_else(|| {
        match env::var("AGMSG_TUI_PALETTE").ok().as_deref() {
            Some("safe") => PaletteMode::Safe,
            _ => PaletteMode::Default,
        }
    });
    (no_color, mode)
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub db: PathBuf,
    pub teams_dir: PathBuf,
    pub scripts_dir: PathBuf,
    pub audit_script: PathBuf,
    pub report_dir: PathBuf,
    pub state_file: PathBuf,
}

impl Paths {
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME が設定されていません")?;
        let skill_dir = home.join(".agents/skills/agmsg");

        Ok(Self {
            db: cli
                .db
                .clone()
                .or_else(|| env::var_os("AGMSG_DB").map(PathBuf::from))
                .unwrap_or_else(|| skill_dir.join("db/messages.db")),
            teams_dir: cli
                .teams_dir
                .clone()
                .or_else(|| env::var_os("AGMSG_TEAMS_DIR").map(PathBuf::from))
                .unwrap_or_else(|| skill_dir.join("teams")),
            scripts_dir: cli
                .scripts_dir
                .clone()
                .or_else(|| env::var_os("AGMSG_SCRIPTS_DIR").map(PathBuf::from))
                .unwrap_or_else(|| skill_dir.join("scripts")),
            audit_script: cli
                .audit_script
                .clone()
                .or_else(|| env::var_os("AGMSG_AUDIT").map(PathBuf::from))
                .unwrap_or_else(|| home.join("bin/agmsg-audit")),
            report_dir: cli
                .report_dir
                .clone()
                .or_else(|| env::var_os("AGMSG_REPORT_DIR").map(PathBuf::from))
                .unwrap_or_else(|| home.join("tmp")),
            state_file: home.join(".config/agmsg-tui/state.json"),
        })
    }
}

//! Command-line interface mirroring the Cobra tree in `cmd/mu/cli`.
//!
//! Cobra's help layout, error text, and flag handling are specific enough
//! that clap's derive defaults diverge byte-for-byte. This module hand-parses
//! argv and hand-renders help to match the Go oracle exactly (golden files
//! under `tests/golden/help*.txt`), while dispatching to the ported command
//! implementations (`status::run`, `clean::run`, …).
//!
//! Parse errors exit 1 with cobra's stderr text (not clap's exit 2). `-h`/
//! `--help`/`-v`/`--version`/`help`/`completion` are intercepted before the
//! subcommand flag parse. Positional args after a known subcommand are
//! silently ignored, matching cobra's default `Args` validator.

use std::io::{IsTerminal, Write};

use crate::{analyze, audit, clean, optimize, status, uninstall};

/// `MU_VERSION` — set by `build.rs` from `git describe --tags --always --dirty`
/// (the Go Makefile's `-X` ldflag equivalent). `option_env!` keeps the
/// constant defined even when the build script's env is absent.
const MU_VERSION: &str = match option_env!("MU_VERSION") {
    Some(v) => v,
    None => "dev",
};

// ---------------------------------------------------------------------------
// Help text — byte-exact copies of tests/golden/help*.txt (cobra's template).
// ---------------------------------------------------------------------------

const ROOT_HELP: &str = "\
mu (Mole Ubuntu) — safe, fast system cleaner and optimizer.

Run without arguments to open the interactive TUI menu.
All destructive commands support --dry-run for safe previewing.

Usage:
  mu [flags]
  mu [command]

Available Commands:
  analyze     Interactive disk usage explorer (ncdu-like)
  audit       Diagnose cleanup issues and apply recommended fixes
  clean       Free disk space by removing caches, logs, and old packages
  completion  Generate the autocompletion script for the specified shell
  help        Help about any command
  history     View recent operations and deletion log
  installer   Find and remove downloaded installer packages (.deb, .AppImage, .iso)
  optimize    Run system maintenance: apt autoremove, journal vacuum, cache refresh
  purge       Safely clean heavy project build artifacts (node_modules, target, .venv)
  status      Live system dashboard: CPU, RAM, disk, network, health score
  uninstall   Remove apps and all their config, cache, and data remnants

Flags:
      --debug     Enable verbose debug logging
  -h, --help      help for mu
  -v, --version   version for mu

Use \"mu [command] --help\" for more information about a command.
";

const ANALYZE_HELP: &str = "\
Interactive disk usage explorer (ncdu-like).
Navigate directories, inspect large files, and safely move items to trash.

Interactive (default on a TTY):
  ↑/k, ↓/j to navigate, Enter/→/l to open, Esc/←/h to go up.
  d to delete item to trash (requires confirmation).
  s to change sort order, Tab to view largest files.

Scripting:
  mu analyze --json [PATH]      output JSON directory tree and exit
  mu analyze --dry-run [PATH]   preview scan metrics with deletion disabled

Usage:
  mu analyze [PATH] [flags]

Flags:
      --dry-run   Preview actions without making changes
  -h, --help      help for analyze
      --json      Output JSON directory tree and exit

Global Flags:
      --debug   Enable verbose debug logging
";

const AUDIT_HELP: &str = "\
Scan the system for reclaimable space and health pressure, then
guide you through applying safe fixes (clean targets and optimize steps).

Interactive (default on a TTY):
  scan → select findings → confirm → apply → re-score

Scripting:
  mu audit --report     human-readable report (exit 1=warning, 2=critical)
  mu audit --json       structured JSON report
  mu audit --dry-run    wizard/report apply path without making changes

Opt-in clean categories (browser-cache, docker) appear in the audit but are
not selected unless listed in --include.

Usage:
  mu audit [flags]

Flags:
      --dry-run           Preview apply actions without making changes
  -h, --help              help for audit
      --include strings   Pre-select opt-in clean categories (e.g. browser-cache)
      --json              Print JSON findings and exit
      --report            Print findings and exit (no apply)

Global Flags:
      --debug   Enable verbose debug logging
";

const CLEAN_HELP: &str = "\
Scan and remove:
  • User cache (~/.cache)
  • APT package cache
  • Snap disabled revisions
  • Journal logs (older than 30 days)
  • Complete APT-policy autoremove candidate set
  • Browser caches (Chrome, Firefox, VSCode)
  • Thumbnail cache
  • Docker build cache (if Docker is installed)

Use --dry-run to preview what would be removed without making changes.
Use --include=browser-cache to enable opt-in categories.
Use --yes to skip the confirmation prompt (for scripting).

Usage:
  mu clean [flags]

Flags:
      --dry-run           Preview actions without making changes
      --external string   Clean metadata on an external volume (mount path)
  -h, --help              help for clean
      --include strings   Opt-in categories (e.g. browser-cache)
  -y, --yes               Skip confirmation prompt

Global Flags:
      --debug   Enable verbose debug logging
";

const OPTIMIZE_HELP: &str = "\
Runs the following maintenance tasks:
  1. apt-get update && apt-get autoremove --purge
  2. journalctl --vacuum-size=500M
  3. update-mime-database, fc-cache

Use --dry-run to see what would run without executing.
Use --skip to exclude specific steps (e.g. --skip=apt,journal).
Use --yes to skip the confirmation prompt (for scripting).

Usage:
  mu optimize [flags]

Flags:
      --dry-run        Preview actions without making changes
  -h, --help           help for optimize
      --skip strings   Steps to skip (apt, journal, caches)
  -y, --yes            Skip confirmation prompt

Global Flags:
      --debug   Enable verbose debug logging
";

const STATUS_HELP: &str = "\
Displays a real-time system health dashboard with visual metric bars.

  • Aggregate CPU usage
  • RAM and swap usage
  • Disk usage for mounted filesystems (used % bars)
  • Network I/O rates (active interfaces)
  • Computed health score (0-100)

Use --json to output a structured JSON snapshot and exit.
When piped (stdout is not a terminal), JSON is used automatically.
Use --watch to stream snapshots continuously as NDJSON.
Press q or Ctrl+C to exit the dashboard.

Usage:
  mu status [flags]

Flags:
  -h, --help                help for status
      --interval duration   Refresh interval for --watch (default 1s)
      --json                Output JSON snapshot and exit
      --watch               Stream status snapshots continuously as NDJSON

Global Flags:
      --debug   Enable verbose debug logging
";

const UNINSTALL_HELP: &str = "\
Interactively select installed packages to remove.
Shows estimated disk usage per package including remnants in:
  ~/.config/<app>  ~/.local/share/<app>  ~/.cache/<app>

Executes: apt purge <pkg> + remnant cleanup.
Use --dry-run to preview without removing anything.

Usage:
  mu uninstall [flags] [package...]

Flags:
      --dry-run     Preview actions without making changes
  -h, --help        help for uninstall
      --list        List installed packages and exit
      --permanent   Bypass trash (rejected for safety)
  -y, --yes         Skip confirmation prompt

Global Flags:
      --debug   Enable verbose debug logging
";

/// Cobra's "usage" template — shown on errors and `help <bogus>`. Same as
/// ROOT_HELP but without the `-h`/`-v` flag lines (cobra's UsageTemplate
/// omits the help/version flags that HelpTemplate includes).
const ROOT_USAGE: &str = "\
Usage:
  mu [flags]
  mu [command]

Available Commands:
  analyze     Interactive disk usage explorer (ncdu-like)
  audit       Diagnose cleanup issues and apply recommended fixes
  clean       Free disk space by removing caches, logs, and old packages
  completion  Generate the autocompletion script for the specified shell
  help        Help about any command
  optimize    Run system maintenance: apt autoremove, journal vacuum, cache refresh
  status      Live system dashboard: CPU, RAM, disk, network, health score
  uninstall   Remove apps and all their config, cache, and data remnants

Flags:
      --debug   Enable verbose debug logging

Use \"mu [command] --help\" for more information about a command.
";

const COMPLETION_HELP: &str = "\
Generate the autocompletion script for mu for the specified shell.
See each sub-command's help for details on how to use the generated script.

Usage:
  mu completion [command]

Available Commands:
  bash        Generate the autocompletion script for bash
  fish        Generate the autocompletion script for fish
  powershell  Generate the autocompletion script for powershell
  zsh         Generate the autocompletion script for zsh

Flags:
  -h, --help   help for completion

Global Flags:
      --debug   Enable verbose debug logging

Use \"mu completion [command] --help\" for more information about a command.
";

const COMPLETION_BASH_HELP: &str = "\
Generate the autocompletion script for the bash shell.

This script depends on the 'bash-completion' package.
If it is not installed already, you can install it via your OS's package manager.

To load completions in your current shell session:

	source <(mu completion bash)

To load completions for every new session, execute once:

#### Linux:

	mu completion bash > /etc/bash_completion.d/mu

#### macOS:

	mu completion bash > $(brew --prefix)/etc/bash_completion.d/mu

You will need to start a new shell for this setup to take effect.

Usage:
  mu completion bash

Flags:
  -h, --help              help for bash
      --no-descriptions   disable completion descriptions

Global Flags:
      --debug   Enable verbose debug logging
";

const COMPLETION_ZSH_HELP: &str = "\
Generate the autocompletion script for the zsh shell.

If shell completion is not already enabled in your environment you will need
to enable it.  You can execute the following once:

	echo \"autoload -U compinit; compinit\" >> ~/.zshrc

To load completions in your current shell session:

	source <(mu completion zsh)

To load completions for every new session, execute once:

#### Linux:

	mu completion zsh > \"${fpath[1]}/_mu\"

#### macOS:

	mu completion zsh > $(brew --prefix)/share/zsh/site-functions/_mu

You will need to start a new shell for this setup to take effect.

Usage:
  mu completion zsh [flags]

Flags:
  -h, --help              help for zsh
      --no-descriptions   disable completion descriptions

Global Flags:
      --debug   Enable verbose debug logging
";

const COMPLETION_FISH_HELP: &str = "\
Generate the autocompletion script for the fish shell.

To load completions in your current shell session:

	mu completion fish | source

To load completions for every new session, execute once:

	mu completion fish > ~/.config/fish/completions/mu.fish

You will need to start a new shell for this setup to take effect.

Usage:
  mu completion fish [flags]

Flags:
  -h, --help              help for fish
      --no-descriptions   disable completion descriptions

Global Flags:
      --debug   Enable verbose debug logging
";

const COMPLETION_POWERSHELL_HELP: &str = "\
Generate the autocompletion script for powershell.

To load completions in your current shell session:

	mu completion powershell | Out-String | Invoke-Expression

To load completions for every new session, add the output of the above command
to your powershell profile.

Usage:
  mu completion powershell [flags]

Flags:
  -h, --help              help for powershell
      --no-descriptions   disable completion descriptions

Global Flags:
      --debug   Enable verbose debug logging
";

const HELP_HELP: &str = "\
Help provides help for any command in the application.
Simply type mu help [path to command] for full details.

Usage:
  mu help [command] [flags]

Flags:
  -h, --help   help for help

Global Flags:
      --debug   Enable verbose debug logging
";
const HISTORY_HELP: &str = "\
View recent operations and deletion log from operations.log.

Usage:
  mu history [flags]

Flags:
  -h, --help        help for history
      --json        Output operations log as a JSON array
      --limit int   Number of recent operations to display (default 20)

Global Flags:
      --debug   Enable verbose debug logging
";

const INSTALLER_HELP: &str = "\
Find and remove downloaded installer packages (.deb, .AppImage, .iso).

Usage:
  mu installer [flags]

Flags:
      --dry-run         Preview actions without making changes
  -h, --help            help for installer
      --paths strings   Specific directories to scan
  -y, --yes             Skip confirmation prompt

Global Flags:
      --debug   Enable verbose debug logging
";

const PURGE_HELP: &str = "\
Safely clean heavy project build artifacts (node_modules, target, .venv).

Usage:
  mu purge [flags]

Flags:
      --dry-run         Preview actions without making changes
  -h, --help            help for purge
      --include-empty   Include empty build artifact directories
      --paths strings   Specific project directories to scan
  -y, --yes             Skip confirmation prompt

Global Flags:
      --debug   Enable verbose debug logging
";

// ---------------------------------------------------------------------------
// Subcommand metadata
// ---------------------------------------------------------------------------

const REAL_COMMANDS: &[&str] = &[
    "analyze",
    "audit",
    "clean",
    "history",
    "installer",
    "optimize",
    "purge",
    "status",
    "uninstall",
];
/// Per-command help text for the known real commands.
fn cmd_help(name: &str) -> Option<&'static str> {
    match name {
        "analyze" => Some(ANALYZE_HELP),
        "audit" => Some(AUDIT_HELP),
        "clean" => Some(CLEAN_HELP),
        "history" => Some(HISTORY_HELP),
        "installer" => Some(INSTALLER_HELP),
        "optimize" => Some(OPTIMIZE_HELP),
        "purge" => Some(PURGE_HELP),
        "status" => Some(STATUS_HELP),
        "uninstall" => Some(UNINSTALL_HELP),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Parse outcome
// ---------------------------------------------------------------------------

/// The parsed result of argv — either dispatch to a command, or a terminal
/// output (help/version/completion) that the caller prints and exits on.
enum Outcome {
    /// Run a real subcommand with its parsed flags.
    Run { debug: bool, cmd: Command },
    /// Print text to stdout and exit 0.
    Stdout(String),
    /// Print text to stderr and exit with the given code.
    Stderr { text: String, code: i32 },
    /// No subcommand — run the TUI entry point.
    #[allow(dead_code)]
    Tui { debug: bool },
}

/// A parsed subcommand with its flag values, ready to dispatch.
enum Command {
    Analyze {
        path: Option<std::path::PathBuf>,
        dry_run: bool,
        json: bool,
    },
    Status {
        json: bool,
        watch: bool,
        interval: Option<String>,
    },
    Clean {
        dry_run: bool,
        include: Vec<String>,
        yes: bool,
        external: Option<std::path::PathBuf>,
    },
    Optimize {
        dry_run: bool,
        skip: Vec<String>,
        yes: bool,
    },
    Audit {
        report: bool,
        json: bool,
        dry_run: bool,
        include: Vec<String>,
    },
    Uninstall {
        dry_run: bool,
        list: bool,
        permanent: bool,
        packages: Vec<String>,
        yes: bool,
    },
    History {
        limit: usize,
        json: bool,
    },
    Purge {
        paths: Vec<std::path::PathBuf>,
        dry_run: bool,
        yes: bool,
        include_empty: bool,
    },
    Installer {
        paths: Vec<std::path::PathBuf>,
        dry_run: bool,
        yes: bool,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Parse argv and dispatch. Mirrors `cmd/mu/cli.Execute()` + cobra's
/// `Command.Execute()` error/exit-code mapping.
pub fn run() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args) {
        Outcome::Tui { debug } => {
            // Go: `runTUI()` → `tea.NewProgram(m, tea.WithAltScreen()).Run()`.
            // Non-TTY: `could not open a new TTY: open /dev/tty: …` exit 1.
            // TTY: run the ratatui main menu loop. `debug` is a persistent
            // flag — Go threads it into every menu-dispatched subcommand.
            if std::io::stdout().is_terminal() {
                match crate::tui::run_tui(debug) {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
            } else {
                eprintln!("could not open a new TTY: open /dev/tty: no such device or address");
                std::process::exit(1);
            }
        }
        Outcome::Stdout(text) => {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let _ = out.write_all(text.as_bytes());
            std::process::exit(0);
        }
        Outcome::Stderr { text, code } => {
            // The text already includes its trailing newline(s) — cobra's
            // error strings are single-line + `\n`, and the `help bogus`
            // usage block ends with `\n` from the template.
            eprint!("{text}");
            std::process::exit(code);
        }
        Outcome::Run { debug, cmd } => {
            let _ = debug; // threaded into each command's Options below
            match cmd {
                Command::Analyze {
                    path,
                    dry_run,
                    json,
                } => {
                    let code = analyze::run(&analyze::Options {
                        path,
                        dry_run,
                        json,
                        debug,
                    });
                    std::process::exit(code);
                }
                Command::Status {
                    json,
                    watch,
                    interval,
                } => {
                    let dur = if let Some(i_str) = interval {
                        match parse_duration(&i_str) {
                            Some(d) => d,
                            None => {
                                eprintln!(
                                    "invalid argument \"{i_str}\" for \"--interval\" flag: time: invalid duration \"{i_str}\""
                                );
                                std::process::exit(1);
                            }
                        }
                    } else {
                        std::time::Duration::from_secs(1)
                    };
                    if watch {
                        std::process::exit(status::run_watch(dur));
                    } else {
                        std::process::exit(status::run(json));
                    }
                }
                Command::Optimize { dry_run, skip, yes } => {
                    let code = match optimize::run(&optimize::Options {
                        dry_run,
                        debug,
                        skip,
                        auto_yes: yes,
                    }) {
                        Ok(_) => 0,
                        Err(e) => {
                            eprintln!("{e}");
                            1
                        }
                    };
                    std::process::exit(code);
                }
                Command::Clean {
                    dry_run,
                    include,
                    yes,
                    external,
                } => {
                    let code = match clean::run(&clean::Options {
                        dry_run,
                        debug,
                        include,
                        auto_yes: yes,
                        external,
                    }) {
                        Ok(_) => 0,
                        Err(e) => {
                            eprintln!("{e}");
                            1
                        }
                    };
                    std::process::exit(code);
                }
                Command::Audit {
                    report,
                    json,
                    dry_run,
                    include,
                } => std::process::exit(audit::run(&audit::Options {
                    report,
                    json,
                    dry_run,
                    debug,
                    include,
                })),
                Command::Uninstall {
                    dry_run,
                    list,
                    permanent,
                    packages,
                    yes,
                } => std::process::exit(uninstall::run(&uninstall::Options {
                    dry_run,
                    debug,
                    list,
                    permanent,
                    packages,
                    auto_yes: yes,
                })),
                Command::History { limit, json } => {
                    std::process::exit(crate::history::run(&crate::history::Options {
                        limit,
                        json,
                    }));
                }
                Command::Purge {
                    paths,
                    dry_run,
                    yes,
                    include_empty,
                } => {
                    std::process::exit(crate::purge::run(&crate::purge::Options {
                        paths,
                        dry_run,
                        auto_yes: yes,
                        include_empty,
                        debug,
                    }));
                }
                Command::Installer {
                    paths,
                    dry_run,
                    yes,
                } => {
                    std::process::exit(crate::installer::run(&crate::installer::Options {
                        paths,
                        dry_run,
                        auto_yes: yes,
                        debug,
                    }));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Manual argv parser — cobra `Find`/`stripFlags` + pflag `Parse` emulation.
//
// Verified against the Go binary (see tests/golden + oracle comparisons):
// - `Find` resolves a command from the first NON-FLAG token — but only flags
//   registered at that moment skip flag-only. Here that's just `--debug`, so
//   `--version status`, `-h clean`, `--bogus status` all EAT the next token
//   as a "value" and never resolve a subcommand (root runs instead).
// - `""` args are dropped; `-` counts as a flag token; `--` stops scanning.
// - The resolved command's pflag set parses ALL args (interspersed): first
//   error wins, then help is checked, then version (root only).
// - Unknown command → `unknown command "X" for "mu"` + `Did you mean this?`
//   (levenshtein ≤ 2 or prefix over visible commands; `help` excluded).
// ---------------------------------------------------------------------------

/// cobra `stripFlags`: argv indices of positional/command-candidate tokens.
fn command_candidates(args: &[String]) -> Vec<usize> {
    let mut cmds = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let s = args[i].as_str();
        if s == "--" {
            break;
        }
        if s.is_empty() {
            i += 1;
            continue;
        }
        if let Some(long) = s.strip_prefix("--") {
            i += 1;
            if !long.contains('=') && long != "debug" && i < args.len() {
                i += 1; // unregistered long flag eats the next token as value
            }
        } else if s.starts_with('-') {
            i += 1;
            if s.len() == 2 && i < args.len() {
                i += 1; // single-char short eats the next token; clusters don't
            }
        } else {
            cmds.push(i);
            i += 1;
        }
    }
    cmds
}

/// pflag flag spec for a resolved command's flag set.
struct FlagSpec {
    long: &'static str,
    short: Option<char>,
    takes_value: bool,
    /// pflag's error-name: `-y, --yes` when a shorthand exists, else `--long`.
    display: &'static str,
}

const F_DEBUG: FlagSpec = FlagSpec {
    long: "debug",
    short: None,
    takes_value: false,
    display: "--debug",
};
const F_HELP: FlagSpec = FlagSpec {
    long: "help",
    short: Some('h'),
    takes_value: false,
    display: "-h, --help",
};
const F_VERSION: FlagSpec = FlagSpec {
    long: "version",
    short: Some('v'),
    takes_value: false,
    display: "-v, --version",
};
const F_DRY_RUN: FlagSpec = FlagSpec {
    long: "dry-run",
    short: None,
    takes_value: false,
    display: "--dry-run",
};
const F_YES: FlagSpec = FlagSpec {
    long: "yes",
    short: Some('y'),
    takes_value: false,
    display: "-y, --yes",
};
const F_JSON: FlagSpec = FlagSpec {
    long: "json",
    short: None,
    takes_value: false,
    display: "--json",
};
const F_REPORT: FlagSpec = FlagSpec {
    long: "report",
    short: None,
    takes_value: false,
    display: "--report",
};
const F_INCLUDE: FlagSpec = FlagSpec {
    long: "include",
    short: None,
    takes_value: true,
    display: "--include",
};
const F_SKIP: FlagSpec = FlagSpec {
    long: "skip",
    short: None,
    takes_value: true,
    display: "--skip",
};
const F_NODESC: FlagSpec = FlagSpec {
    long: "no-descriptions",
    short: None,
    takes_value: false,
    display: "--no-descriptions",
};
const F_PERMANENT: FlagSpec = FlagSpec {
    long: "permanent",
    short: None,
    takes_value: false,
    display: "--permanent",
};
const F_LIST: FlagSpec = FlagSpec {
    long: "list",
    short: None,
    takes_value: false,
    display: "--list",
};
const F_EXTERNAL: FlagSpec = FlagSpec {
    long: "external",
    short: None,
    takes_value: true,
    display: "--external",
};
const F_LIMIT: FlagSpec = FlagSpec {
    long: "limit",
    short: None,
    takes_value: true,
    display: "--limit",
};
const F_PATHS: FlagSpec = FlagSpec {
    long: "paths",
    short: None,
    takes_value: true,
    display: "--paths",
};
const F_INCLUDE_EMPTY: FlagSpec = FlagSpec {
    long: "include-empty",
    short: None,
    takes_value: false,
    display: "--include-empty",
};
const F_WATCH: FlagSpec = FlagSpec {
    long: "watch",
    short: None,
    takes_value: false,
    display: "--watch",
};
const F_INTERVAL: FlagSpec = FlagSpec {
    long: "interval",
    short: None,
    takes_value: true,
    display: "--interval",
};

fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(ms_str) = s.strip_suffix("ms") {
        let ms = ms_str.parse::<u64>().ok()?;
        if ms > 0 {
            Some(std::time::Duration::from_millis(ms))
        } else {
            None
        }
    } else if let Some(s_str) = s.strip_suffix('s') {
        let v = s_str.parse::<f64>().ok()?;
        if v > 0.0 && v.is_finite() {
            std::time::Duration::try_from_secs_f64(v).ok()
        } else {
            None
        }
    } else if let Some(m_str) = s.strip_suffix('m') {
        let v = m_str.parse::<f64>().ok()?;
        if v > 0.0 && v.is_finite() {
            std::time::Duration::try_from_secs_f64(v * 60.0).ok()
        } else {
            None
        }
    } else if let Ok(secs) = s.parse::<u64>() {
        if secs > 0 {
            Some(std::time::Duration::from_secs(secs))
        } else {
            None
        }
    } else {
        None
    }
}

const ROOT_FLAGS: &[FlagSpec] = &[F_DEBUG, F_HELP, F_VERSION];
const HELP_FLAGS: &[FlagSpec] = &[F_DEBUG, F_HELP];
const COMPLETION_FLAGS: &[FlagSpec] = &[F_DEBUG, F_HELP];
const SHELL_FLAGS: &[FlagSpec] = &[F_DEBUG, F_HELP, F_NODESC];

fn cmd_flagset(name: &str) -> &'static [FlagSpec] {
    match name {
        "analyze" => &[F_DEBUG, F_HELP, F_DRY_RUN, F_JSON],
        "status" => &[F_DEBUG, F_HELP, F_INTERVAL, F_JSON, F_WATCH],
        "clean" => &[F_DEBUG, F_HELP, F_DRY_RUN, F_YES, F_INCLUDE, F_EXTERNAL],
        "optimize" => &[F_DEBUG, F_HELP, F_DRY_RUN, F_YES, F_SKIP],
        "audit" => &[F_DEBUG, F_HELP, F_DRY_RUN, F_JSON, F_REPORT, F_INCLUDE],
        "uninstall" => &[F_DEBUG, F_HELP, F_DRY_RUN, F_PERMANENT, F_LIST, F_YES],
        "history" => &[F_DEBUG, F_HELP, F_JSON, F_LIMIT],
        "installer" => &[F_DEBUG, F_HELP, F_DRY_RUN, F_YES, F_PATHS],
        "purge" => &[F_DEBUG, F_HELP, F_DRY_RUN, F_YES, F_PATHS, F_INCLUDE_EMPTY],
        _ => &[],
    }
}

/// pflag parse result: positionals + flag values keyed by long name.
#[derive(Default)]
struct Parsed {
    positionals: Vec<String>,
    bools: std::collections::HashMap<&'static str, bool>,
    values: std::collections::HashMap<&'static str, Vec<String>>,
}

impl Parsed {
    fn get(&self, long: &str) -> bool {
        self.bools.get(long).copied().unwrap_or(false)
    }
    fn vals(&self, long: &str) -> Vec<String> {
        self.values.get(long).cloned().unwrap_or_default()
    }
}

fn perr(text: String) -> Outcome {
    Outcome::Stderr { text, code: 1 }
}

fn invalid_bool_arg(v: &str, spec: &FlagSpec) -> Outcome {
    perr(format!(
        "invalid argument \"{v}\" for \"{}\" flag: strconv.ParseBool: parsing \"{v}\": invalid syntax\n",
        spec.display
    ))
}

/// pflag `Parse`: interspersed flags + positionals, first error wins.
fn flag_parse(args: &[String], specs: &[FlagSpec]) -> Result<Parsed, Outcome> {
    let mut p = Parsed::default();
    let mut i = 0;
    while i < args.len() {
        let s = args[i].as_str();
        if s == "--" {
            p.positionals.extend(args[i + 1..].iter().cloned());
            break;
        }
        if let Some(body) = s.strip_prefix("--") {
            // pflag `bad flag syntax`: `--` + name that is empty, or starts
            // with `-` or `=` (checked before splitting the `=value`).
            if body.is_empty() || body.starts_with('-') || body.starts_with('=') {
                return Err(perr(format!("bad flag syntax: {s}\n")));
            }
            let (name, inline) = match body.find('=') {
                Some(k) => (&body[..k], Some(body[k + 1..].to_string())),
                None => (body, None),
            };
            let Some(spec) = specs.iter().find(|f| f.long == name) else {
                return Err(perr(format!("unknown flag: --{name}\n")));
            };
            if spec.takes_value {
                let v = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        if i >= args.len() {
                            return Err(perr(format!("flag needs an argument: --{name}\n")));
                        }
                        args[i].clone()
                    }
                };
                match csv_parse(&v) {
                    Ok(recs) => p.values.entry(spec.long).or_default().extend(recs),
                    Err(e) => {
                        return Err(perr(format!(
                            "invalid argument \"{v}\" for \"{}\" flag: {e}\n",
                            spec.display
                        )));
                    }
                }
            } else {
                let v = match &inline {
                    None => true,
                    Some(v) => match parse_pflag_bool(v) {
                        Some(b) => b,
                        None => return Err(invalid_bool_arg(v, spec)),
                    },
                };
                p.bools.insert(spec.long, v);
            }
            i += 1;
        } else if s.starts_with('-') && s.len() > 1 {
            let cluster: Vec<char> = s[1..].chars().collect();
            let mut k = 0;
            while k < cluster.len() {
                let c = cluster[k];
                let Some(spec) = specs.iter().find(|f| f.short == Some(c)) else {
                    let tail: String = cluster[k..].iter().collect();
                    return Err(perr(format!("unknown shorthand flag: '{c}' in -{tail}\n")));
                };
                let rest: String = cluster[k + 1..].iter().collect();
                if spec.takes_value {
                    let v = if let Some(v) = rest.strip_prefix('=') {
                        v.to_string()
                    } else if !rest.is_empty() {
                        rest
                    } else {
                        i += 1;
                        if i >= args.len() {
                            return Err(perr(format!("flag needs an argument: '{c}' in -{c}\n")));
                        }
                        args[i].clone()
                    };
                    match csv_parse(&v) {
                        Ok(recs) => p.values.entry(spec.long).or_default().extend(recs),
                        Err(e) => {
                            return Err(perr(format!(
                                "invalid argument \"{v}\" for \"{}\" flag: {e}\n",
                                spec.display
                            )));
                        }
                    }
                    break;
                }
                // Bool shorthand: `=v` with nonempty v is an inline value;
                // a trailing `=` alone falls through as the next shorthand
                // char (oracle: `-y=` → `unknown shorthand flag: '=' in -=`).
                if let Some(v) = rest.strip_prefix('=').filter(|v| !v.is_empty()) {
                    match parse_pflag_bool(v) {
                        Some(b) => {
                            p.bools.insert(spec.long, b);
                            break;
                        }
                        None => return Err(invalid_bool_arg(v, spec)),
                    }
                }
                p.bools.insert(spec.long, true);
                k += 1;
            }
            i += 1;
        } else {
            p.positionals.push(args[i].clone());
            i += 1;
        }
    }
    Ok(p)
}

/// Go `encoding/csv` single-record read: comma-separated fields, `"`-quoted
/// fields with `""` escapes. Errors carry Go's parse-error text.
fn csv_parse(s: &str) -> Result<Vec<String>, String> {
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let chars: Vec<char> = s.chars().collect();
    let mut k = 0;
    while k < chars.len() {
        let c = chars[k];
        if in_quotes {
            if c == '"' {
                if k + 1 < chars.len() && chars[k + 1] == '"' {
                    field.push('"');
                    k += 1;
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else if c == ',' {
            out.push(std::mem::take(&mut field));
        } else if c == '"' && field.is_empty() {
            in_quotes = true;
        } else if c == '"' {
            return Err(format!(
                "parse error on line 1, column {}: bare \" in non-quoted-field",
                k + 1
            ));
        } else {
            field.push(c);
        }
        k += 1;
    }
    if in_quotes {
        return Err(format!(
            "parse error on line 1, column {}: extraneous or missing \" in quoted-field",
            chars.len() + 1
        ));
    }
    out.push(field);
    Ok(out)
}

/// `strconv.ParseBool`: 1,t,T,TRUE,true,True → true;
/// 0,f,F,FALSE,false,False → false; anything else → None (parse error).
fn parse_pflag_bool(v: &str) -> Option<bool> {
    match v {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// `strconv.Quote`/`%#q` for one help-topic token: backquoted when the string
/// can appear in a Go raw literal (no backtick/`\r`), else double-quoted.
fn go_quote(s: &str) -> String {
    if !s.contains('`') && !s.contains('\r') {
        format!("`{s}`")
    } else {
        format!("{s:?}")
    }
}

/// Levenshtein distance (case-insensitive) — cobra's `ld`.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(ca != cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Commands cobra suggests for typos — `help` is registered lazily and is
/// never suggested (verified: `mu hlp`/`mu hep` → no suggestion).
const SUGGEST_COMMANDS: &[&str] = &[
    "analyze",
    "audit",
    "clean",
    "completion",
    "history",
    "installer",
    "optimize",
    "purge",
    "status",
    "uninstall",
];

/// `unknown command "X" for "mu"` + `Did you mean this?` suggestions
/// (levenshtein ≤ 2 or command-name prefix match, AddCommand order).
fn unknown_command(tok: &str) -> Outcome {
    let mut text = format!("unknown command \"{tok}\" for \"mu\"\n");
    let sugg: Vec<&&str> = SUGGEST_COMMANDS
        .iter()
        .filter(|c| levenshtein(tok, c) <= 2 || c.to_lowercase().starts_with(&tok.to_lowercase()))
        .collect();
    if !sugg.is_empty() {
        text.push_str("\nDid you mean this?\n");
        for s in sugg {
            text.push_str(&format!("\t{s}\n"));
        }
        text.push('\n');
    }
    Outcome::Stderr { text, code: 1 }
}

/// `mu help` fallback — `Unknown help topic [`a` `b`]` + root usage on
/// stderr, exit 0 (cobra's help RunE prints via `cmd.Printf` → stderr).
fn unknown_help_topic(topics: &[String]) -> Outcome {
    let quoted: Vec<String> = topics.iter().map(|t| go_quote(t)).collect();
    Outcome::Stderr {
        text: format!("Unknown help topic [{}]\n{ROOT_USAGE}", quoted.join(" ")),
        code: 0,
    }
}

/// Parse `args` (argv[1..]) into an [`Outcome`].
fn parse(args: &[String]) -> Outcome {
    let cands = command_candidates(args);
    if let Some(&i0) = cands.first() {
        let mut rest = args.to_vec();
        rest.remove(i0);
        match args[i0].as_str() {
            "help" => return exec_help(&rest),
            "completion" => return exec_completion(&rest),
            n if REAL_COMMANDS.contains(&n) => return exec_subcommand(n, &rest),
            other => return unknown_command(other),
        }
    }
    exec_root(args)
}

/// Root executes: full flag parse, then help → version → TUI (cobra checks
/// helpVal before versionVal — `mu -vh` prints help).
fn exec_root(args: &[String]) -> Outcome {
    match flag_parse(args, ROOT_FLAGS) {
        Err(o) => o,
        Ok(p) => {
            if p.get("help") {
                Outcome::Stdout(ROOT_HELP.to_string())
            } else if p.get("version") {
                Outcome::Stdout(format!("mu version {MU_VERSION}\n"))
            } else {
                Outcome::Tui {
                    debug: p.get("debug"),
                }
            }
        }
    }
}

fn exec_subcommand(name: &str, args: &[String]) -> Outcome {
    let p = match flag_parse(args, cmd_flagset(name)) {
        Ok(p) => p,
        Err(o) => return o,
    };
    if p.get("help") {
        return Outcome::Stdout(cmd_help(name).unwrap().to_string());
    }
    let debug = p.get("debug");
    let cmd = match name {
        "analyze" => Command::Analyze {
            path: p.positionals.first().map(std::path::PathBuf::from),
            dry_run: p.get("dry-run"),
            json: p.get("json"),
        },
        "status" => {
            let interval = p.vals("interval").into_iter().next();
            if let Some(i_str) = interval.as_deref().filter(|s| parse_duration(s).is_none()) {
                return perr(format!(
                    "invalid argument \"{i_str}\" for \"--interval\" flag: time: invalid duration \"{i_str}\"\n"
                ));
            }
            Command::Status {
                json: p.get("json"),
                watch: p.get("watch"),
                interval,
            }
        }
        "clean" => Command::Clean {
            dry_run: p.get("dry-run"),
            include: p.vals("include"),
            yes: p.get("yes"),
            external: p.vals("external").first().map(std::path::PathBuf::from),
        },
        "optimize" => Command::Optimize {
            dry_run: p.get("dry-run"),
            skip: p.vals("skip"),
            yes: p.get("yes"),
        },
        "audit" => Command::Audit {
            report: p.get("report"),
            json: p.get("json"),
            dry_run: p.get("dry-run"),
            include: p.vals("include"),
        },
        "uninstall" => Command::Uninstall {
            dry_run: p.get("dry-run"),
            list: p.get("list"),
            permanent: p.get("permanent"),
            yes: p.get("yes"),
            packages: p.positionals,
        },
        "history" => {
            let limit = if let Some(s) = p.vals("limit").first() {
                match s.parse::<usize>() {
                    Ok(l) => l,
                    Err(_) => {
                        return perr(format!(
                            "invalid argument \"{s}\" for \"--limit\" flag: strconv.ParseInt: parsing \"{s}\": invalid syntax\n"
                        ));
                    }
                }
            } else {
                crate::history::DEFAULT_LIMIT
            };
            Command::History {
                limit,
                json: p.get("json"),
            }
        }
        "purge" => Command::Purge {
            paths: p
                .vals("paths")
                .iter()
                .map(std::path::PathBuf::from)
                .collect(),
            dry_run: p.get("dry-run"),
            yes: p.get("yes"),
            include_empty: p.get("include-empty"),
        },
        "installer" => Command::Installer {
            paths: p
                .vals("paths")
                .iter()
                .map(std::path::PathBuf::from)
                .collect(),
            dry_run: p.get("dry-run"),
            yes: p.get("yes"),
        },
        _ => unreachable!(),
    };
    Outcome::Run { debug, cmd }
}

/// `mu help [topics]` — cobra's help command. Flags parse first (so
/// `help --bogus` errors), then topics go through `Root().Find` — a resolved
/// command prints its help, otherwise `Unknown help topic`.
fn exec_help(args: &[String]) -> Outcome {
    let p = match flag_parse(args, HELP_FLAGS) {
        Ok(p) => p,
        Err(o) => return o,
    };
    if p.get("help") {
        return Outcome::Stdout(HELP_HELP.to_string());
    }
    if p.positionals.is_empty() {
        return Outcome::Stdout(ROOT_HELP.to_string());
    }
    let cands = command_candidates(&p.positionals);
    let Some(&j) = cands.first() else {
        return Outcome::Stdout(ROOT_HELP.to_string());
    };
    match p.positionals[j].as_str() {
        "help" => Outcome::Stdout(HELP_HELP.to_string()),
        "completion" => {
            let inner = command_candidates(&p.positionals[j + 1..]);
            match inner.first() {
                Some(&k) if is_shell(&p.positionals[j + 1..][k]) => {
                    Outcome::Stdout(shell_help(&p.positionals[j + 1..][k]).to_string())
                }
                _ => Outcome::Stdout(COMPLETION_HELP.to_string()),
            }
        }
        n if REAL_COMMANDS.contains(&n) => Outcome::Stdout(cmd_help(n).unwrap().to_string()),
        _ => unknown_help_topic(&p.positionals),
    }
}

/// `mu completion [shell]` — cobra's generated command: shells are real
/// subcommands; anything else falls back to the parent's own help.
fn exec_completion(args: &[String]) -> Outcome {
    let cands = command_candidates(args);
    if let Some(&j) = cands.first().filter(|&&j| is_shell(&args[j])) {
        let shell = args[j].clone();
        let mut rest = args.to_vec();
        rest.remove(j);
        let p = match flag_parse(&rest, SHELL_FLAGS) {
            Ok(p) => p,
            Err(o) => return o,
        };
        if p.get("help") {
            return Outcome::Stdout(shell_help(&shell).to_string());
        }
        if let Some(x) = p.positionals.first() {
            return perr(format!(
                "unknown command \"{x}\" for \"mu completion {shell}\"\n"
            ));
        }
        return Outcome::Stdout(completion_script(&shell));
    }
    match flag_parse(args, COMPLETION_FLAGS) {
        Err(o) => o,
        Ok(_) => Outcome::Stdout(COMPLETION_HELP.to_string()),
    }
}

fn is_shell(s: &str) -> bool {
    matches!(s, "bash" | "zsh" | "fish" | "powershell")
}

/// Per-shell help text — cobra's generated `completion <shell>` Long+Usage.
fn shell_help(shell: &str) -> &'static str {
    match shell {
        "bash" => COMPLETION_BASH_HELP,
        "zsh" => COMPLETION_ZSH_HELP,
        "fish" => COMPLETION_FISH_HELP,
        "powershell" => COMPLETION_POWERSHELL_HELP,
        _ => COMPLETION_HELP,
    }
}

/// Generate a minimal shell completion script. The content differs from
/// cobra's (accepted divergence — functional parity, not byte parity).
fn completion_script(shell: &str) -> String {
    let cmds = "audit clean optimize status uninstall";
    match shell {
        "bash" => format!(
            "# bash completion for mu\n\
_mu() {{\n\
    local cmds=\"{cmds}\"\n\
    COMPREPLY=( $(compgen -W \"$cmds\" -- \"${{COMP_WORDS[COMP_CWORD]}}\") )\n\
}}\n\
complete -F _mu mu\n"
        ),
        "zsh" => format!(
            "#compdef mu\n\
_mu() {{\n\
    local cmds=({cmds})\n\
    _describe 'command' cmds\n\
}}\n\
compdef _mu mu\n"
        ),
        "fish" => format!(
            "# fish completion for mu\n\
complete -c mu -f -a \"{cmds}\"\n"
        ),
        "powershell" => format!(
            "# powershell completion for mu\n\
Register-ArgumentCompleter -Native -CommandName mu -ScriptBlock {{\n\
    param($wordToComplete, $commandAst, $cursorPosition)\n\
    @('{cmds}' -split ' ') | Where-Object {{ $_ -like \"$wordToComplete*\" }} |\n\
        ForEach-Object {{ [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_) }}\n\
}}\n"
        ),
        _ => COMPLETION_HELP.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(slice: &[&str]) -> Vec<String> {
        slice.iter().map(|s| String::from(*s)).collect()
    }

    #[test]
    fn version_long() {
        match parse(&args(&["--version"])) {
            Outcome::Stdout(t) => assert!(t.starts_with("mu version ")),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn version_short() {
        match parse(&args(&["-v"])) {
            Outcome::Stdout(t) => assert!(t.starts_with("mu version ")),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn version_with_trailing_arg() {
        // `mu --version foo` → version wins (cobra short-circuits).
        match parse(&args(&["--version", "foo"])) {
            Outcome::Stdout(t) => assert!(t.starts_with("mu version ")),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn version_not_on_subcommand() {
        match parse(&args(&["status", "--version"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(text, "unknown flag: --version\n");
                assert_eq!(code, 1);
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn version_short_not_on_subcommand() {
        match parse(&args(&["status", "-v"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(text, "unknown shorthand flag: 'v' in -v\n");
                assert_eq!(code, 1);
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn unknown_command_version() {
        match parse(&args(&["version"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(text, "unknown command \"version\" for \"mu\"\n");
                assert_eq!(code, 1);
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn unknown_command_foo() {
        match parse(&args(&["foo"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(text, "unknown command \"foo\" for \"mu\"\n");
                assert_eq!(code, 1);
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn unknown_flag_at_root() {
        match parse(&args(&["--bogus"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(text, "unknown flag: --bogus\n");
                assert_eq!(code, 1);
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn unknown_flag_on_subcommand() {
        match parse(&args(&["status", "--bogus"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(text, "unknown flag: --bogus\n");
                assert_eq!(code, 1);
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn missing_flag_value() {
        match parse(&args(&["clean", "--include"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(text, "flag needs an argument: --include\n");
                assert_eq!(code, 1);
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn positional_args_ignored() {
        match parse(&args(&["status", "foo"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Status { json, .. } => assert!(!json),
                _ => panic!("expected Status"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn clean_positional_ignored() {
        match parse(&args(&["clean", "extra", "args", "--dry-run"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Clean { dry_run, .. } => assert!(dry_run),
                _ => panic!("expected Clean"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn help_root() {
        match parse(&args(&["--help"])) {
            Outcome::Stdout(t) => assert_eq!(t, ROOT_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn help_short() {
        match parse(&args(&["-h"])) {
            Outcome::Stdout(t) => assert_eq!(t, ROOT_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn help_command() {
        match parse(&args(&["help"])) {
            Outcome::Stdout(t) => assert_eq!(t, ROOT_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn help_subcommand() {
        match parse(&args(&["help", "status"])) {
            Outcome::Stdout(t) => assert_eq!(t, STATUS_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn help_subcommand_via_flag() {
        match parse(&args(&["status", "--help"])) {
            Outcome::Stdout(t) => assert_eq!(t, STATUS_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn help_bogus() {
        match parse(&args(&["help", "bogus"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 0);
                assert!(text.starts_with("Unknown help topic [`bogus`]"));
                assert!(text.contains(ROOT_USAGE));
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn completion_no_arg() {
        match parse(&args(&["completion"])) {
            Outcome::Stdout(t) => assert_eq!(t, COMPLETION_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn completion_bogus() {
        match parse(&args(&["completion", "xyz"])) {
            Outcome::Stdout(t) => assert_eq!(t, COMPLETION_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn completion_bash() {
        match parse(&args(&["completion", "bash"])) {
            Outcome::Stdout(t) => assert!(t.contains("# bash completion for mu")),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn no_args_tui() {
        match parse(&[]) {
            Outcome::Tui { .. } => {}
            _ => panic!("expected Tui"),
        }
    }

    #[test]
    fn debug_before_subcommand() {
        match parse(&args(&["--debug", "status"])) {
            Outcome::Run { debug, .. } => assert!(debug),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn debug_after_subcommand() {
        match parse(&args(&["status", "--debug"])) {
            Outcome::Run { debug, .. } => assert!(debug),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn debug_eq_true_at_root() {
        match parse(&args(&["--debug=true", "status"])) {
            Outcome::Run { debug, .. } => assert!(debug),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn debug_eq_false_at_root() {
        match parse(&args(&["--debug=false", "status"])) {
            Outcome::Run { debug, .. } => assert!(!debug),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn bool_flag_eq_false_on_subcommand() {
        // pflag: `--dry-run=false` must NOT enable dry-run.
        match parse(&args(&["clean", "--dry-run=false"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Clean { dry_run, .. } => assert!(!dry_run),
                _ => panic!("expected Clean"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn bool_flag_eq_true_on_subcommand() {
        match parse(&args(&["clean", "--dry-run=true"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Clean { dry_run, .. } => assert!(dry_run),
                _ => panic!("expected Clean"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn bool_flag_invalid_value_errors() {
        // Oracle: `invalid argument "x" for "--dry-run" flag:
        // strconv.ParseBool: parsing "x": invalid syntax`, exit 1.
        match parse(&args(&["clean", "--dry-run=x"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert_eq!(
                    text,
                    "invalid argument \"x\" for \"--dry-run\" flag: strconv.ParseBool: parsing \"x\": invalid syntax\n"
                );
            }
            _ => panic!("expected Stderr"),
        }
        // Shorthand-bearing flags print `-y, --yes`.
        match parse(&args(&["clean", "--yes=x"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert!(text.contains("\"-y, --yes\""), "{text}");
            }
            _ => panic!("expected Stderr"),
        }
        // Root --debug=x errors the same way.
        match parse(&args(&["--debug=x", "status"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert!(text.contains("\"--debug\""), "{text}");
            }
            _ => panic!("expected Stderr"),
        }
        // Full ParseBool set accepted.
        match parse(&args(&["clean", "--dry-run=t"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Clean { dry_run, .. } => assert!(dry_run),
                _ => panic!("expected Clean"),
            },
            _ => panic!("expected Run"),
        }
        match parse(&args(&["clean", "--dry-run=F"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Clean { dry_run, .. } => assert!(!dry_run),
                _ => panic!("expected Clean"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn debug_eq_after_subcommand() {
        // Persistent pflag: `--debug=<v>` parses after the subcommand too.
        match parse(&args(&["status", "--debug=false"])) {
            Outcome::Run { debug, .. } => assert!(!debug),
            _ => panic!("expected Run"),
        }
        match parse(&args(&["status", "--debug=true"])) {
            Outcome::Run { debug, .. } => assert!(debug),
            _ => panic!("expected Run"),
        }
        match parse(&args(&["status", "--debug=x"])) {
            Outcome::Stderr { code, .. } => assert_eq!(code, 1),
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn dashdash_root_is_positional_not_subcommand() {
        // Cobra: `--` terminates flag parsing — trailing tokens are
        // POSITIONAL args to root, never resolved as subcommands.
        // `mu -- status` runs the root TUI, not the status command.
        match parse(&args(&["--", "status"])) {
            Outcome::Tui { debug } => assert!(!debug),
            _ => panic!("expected Tui"),
        }
        match parse(&args(&["--"])) {
            Outcome::Tui { .. } => {}
            _ => panic!("expected Tui"),
        }
        match parse(&args(&["--debug", "--", "status"])) {
            Outcome::Tui { debug } => assert!(debug),
            _ => panic!("expected Tui"),
        }
        // Subcommand-level `--` still breaks flag parsing normally.
        match parse(&args(&["clean", "--", "--dry-run"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Clean { dry_run, .. } => assert!(!dry_run),
                _ => panic!("expected Clean"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn help_debug_eq_value() {
        // `mu help --debug=false` → pflag consumes the flag → bare help.
        match parse(&args(&["help", "--debug=false"])) {
            Outcome::Stdout(t) => assert_eq!(t, ROOT_HELP),
            _ => panic!("expected Stdout"),
        }
        match parse(&args(&["help", "--debug=x"])) {
            Outcome::Stderr { code, .. } => assert_eq!(code, 1),
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn unregistered_root_flags_eat_next_token() {
        // stripFlags: `--version`/`-h`/`--bogus` are NOT registered at Find
        // time → they consume the next token as a "value", so `status` is
        // never resolved — root executes and its flagset parses everything.
        match parse(&args(&["--version", "status"])) {
            Outcome::Stdout(t) => assert!(t.starts_with("mu version "), "{t}"),
            _ => panic!("expected Stdout"),
        }
        match parse(&args(&["-h", "status"])) {
            Outcome::Stdout(t) => assert_eq!(t, ROOT_HELP),
            _ => panic!("expected Stdout"),
        }
        match parse(&args(&["--bogus", "status"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert_eq!(text, "unknown flag: --bogus\n");
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn flag_errors_beat_help_and_version() {
        // pflag parses ALL args before cobra checks help/version.
        match parse(&args(&["--version", "--bogus"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert_eq!(text, "unknown flag: --bogus\n");
            }
            _ => panic!("expected Stderr"),
        }
        match parse(&args(&["status", "--help", "--bogus"])) {
            Outcome::Stderr { code, .. } => assert_eq!(code, 1),
            _ => panic!("expected Stderr"),
        }
        match parse(&args(&["status", "-hj"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert_eq!(text, "unknown shorthand flag: 'j' in -j\n");
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn help_beats_version() {
        // cobra checks helpVal before versionVal — `-vh` prints help.
        match parse(&args(&["-vh"])) {
            Outcome::Stdout(t) => assert_eq!(t, ROOT_HELP),
            _ => panic!("expected Stdout"),
        }
        match parse(&args(&["-hv"])) {
            Outcome::Stdout(t) => assert_eq!(t, ROOT_HELP),
            _ => panic!("expected Stdout"),
        }
        match parse(&args(&["--version", "--help"])) {
            Outcome::Stdout(t) => assert_eq!(t, ROOT_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn unknown_command_suggestions() {
        match parse(&args(&["clea"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert_eq!(
                    text,
                    "unknown command \"clea\" for \"mu\"\n\nDid you mean this?\n\tclean\n\n"
                );
            }
            _ => panic!("expected Stderr"),
        }
        // Multiple matches keep "Did you mean this?" + AddCommand order.
        match parse(&args(&["c"])) {
            Outcome::Stderr { text, .. } => {
                assert!(text.contains("\tclean\n\tcompletion\n"), "{text}");
            }
            _ => panic!("expected Stderr"),
        }
        // `help` is lazily registered — never suggested.
        match parse(&args(&["hlp"])) {
            Outcome::Stderr { text, .. } => {
                assert!(!text.contains("Did you mean"), "{text}");
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn dash_and_empty_arg_handling() {
        // `-` is a flag token → `status` still resolves as the command.
        match parse(&args(&["-", "status"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Status { .. } => {}
                _ => panic!("expected Status"),
            },
            _ => panic!("expected Run"),
        }
        // `""` is dropped entirely — `status` resolves through it.
        match parse(&args(&["", "status"])) {
            Outcome::Run { .. } => {}
            _ => panic!("expected Run"),
        }
        // Bare `-` → root TUI.
        match parse(&args(&["-"])) {
            Outcome::Tui { .. } => {}
            _ => panic!("expected Tui"),
        }
    }

    #[test]
    fn bad_flag_syntax_and_eq_errors() {
        for arg in ["---x", "--=x"] {
            match parse(&args(&[arg])) {
                Outcome::Stderr { text, code } => {
                    assert_eq!(code, 1);
                    assert_eq!(text, format!("bad flag syntax: {arg}\n"));
                }
                _ => panic!("expected Stderr for {arg}"),
            }
        }
        // Unknown-flag messages strip the `=value`.
        match parse(&args(&["--bogus=x"])) {
            Outcome::Stderr { text, .. } => assert_eq!(text, "unknown flag: --bogus\n"),
            _ => panic!("expected Stderr"),
        }
        match parse(&args(&["status", "--bogus=x"])) {
            Outcome::Stderr { text, .. } => assert_eq!(text, "unknown flag: --bogus\n"),
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn shorthand_bool_eq_value() {
        match parse(&args(&["clean", "-y=false", "--dry-run"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Clean { dry_run, yes, .. } => {
                    assert!(dry_run);
                    assert!(!yes);
                }
                _ => panic!("expected Clean"),
            },
            _ => panic!("expected Run"),
        }
        // `-y=` alone: `=` becomes the next shorthand char → unknown.
        match parse(&args(&["clean", "-y="])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert_eq!(text, "unknown shorthand flag: '=' in -=\n");
            }
            _ => panic!("expected Stderr"),
        }
        match parse(&args(&["clean", "-y=x"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert!(text.contains("\"-y, --yes\""), "{text}");
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn help_and_completion_flag_parsing() {
        // cobra parses the help/completion flag sets before running.
        match parse(&args(&["help", "--bogus"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert_eq!(text, "unknown flag: --bogus\n");
            }
            _ => panic!("expected Stderr"),
        }
        match parse(&args(&["completion", "--bogus"])) {
            Outcome::Stderr { code, .. } => assert_eq!(code, 1),
            _ => panic!("expected Stderr"),
        }
        // `help -- status` → `--` ends flags → topic resolves via Find.
        match parse(&args(&["help", "--", "status"])) {
            Outcome::Stdout(t) => assert_eq!(t, STATUS_HELP),
            _ => panic!("expected Stdout"),
        }
        // Extra positional on a shell subcommand → NoArgs error.
        match parse(&args(&["completion", "bash", "extra"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert_eq!(
                    text,
                    "unknown command \"extra\" for \"mu completion bash\"\n"
                );
            }
            _ => panic!("expected Stderr"),
        }
        // Non-shell topic → parent's own help.
        match parse(&args(&["completion", "bogus"])) {
            Outcome::Stdout(t) => assert_eq!(t, COMPLETION_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn help_multi_topic_and_nested_resolution() {
        match parse(&args(&["help", "bogus", "extra"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 0);
                assert!(
                    text.starts_with("Unknown help topic [`bogus` `extra`]"),
                    "{text}"
                );
            }
            _ => panic!("expected Stderr"),
        }
        // `help completion bash` resolves the shell's own help.
        match parse(&args(&["help", "completion", "bash"])) {
            Outcome::Stdout(t) => assert_eq!(t, COMPLETION_BASH_HELP),
            _ => panic!("expected Stdout"),
        }
        match parse(&args(&["completion", "bash", "-h"])) {
            Outcome::Stdout(t) => assert_eq!(t, COMPLETION_BASH_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn help_help_flag() {
        // `mu help --help` → help command's own help (not root help).
        match parse(&args(&["help", "--help"])) {
            Outcome::Stdout(t) => assert_eq!(t, HELP_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn help_help_topic() {
        // `mu help help` → help command's own help.
        match parse(&args(&["help", "help"])) {
            Outcome::Stdout(t) => assert_eq!(t, HELP_HELP),
            _ => panic!("expected Stdout"),
        }
    }

    #[test]
    fn include_comma_split() {
        match parse(&args(&["clean", "--include=browser-cache,docker"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Clean { include, .. } => {
                    assert_eq!(include, vec!["browser-cache", "docker"]);
                }
                _ => panic!("expected Clean"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn yes_short_flag() {
        match parse(&args(&["clean", "-y"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Clean { yes, .. } => assert!(yes),
                _ => panic!("expected Clean"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn clean_external_flag_parses() {
        match parse(&args(&["clean", "--external", "/mnt/usb"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Clean { external, .. } => {
                    assert_eq!(external, Some(std::path::PathBuf::from("/mnt/usb")));
                }
                _ => panic!("expected Clean"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn uninstall_yes_flag_parses() {
        match parse(&args(&["uninstall", "--yes", "bash"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Uninstall { yes, packages, .. } => {
                    assert!(yes);
                    assert_eq!(packages, vec!["bash"]);
                }
                _ => panic!("expected Uninstall"),
            },
            _ => panic!("expected Run"),
        }
    }
    #[test]
    fn analyze_flag_parsing() {
        match parse(&args(&["analyze", "--json", "/var/log"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Analyze {
                    path,
                    dry_run,
                    json,
                } => {
                    assert_eq!(path, Some(std::path::PathBuf::from("/var/log")));
                    assert!(!dry_run);
                    assert!(json);
                }
                _ => panic!("expected Analyze"),
            },
            _ => panic!("expected Run"),
        }
        match parse(&args(&["analyze", "--dry-run"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Analyze {
                    path,
                    dry_run,
                    json,
                } => {
                    assert_eq!(path, None);
                    assert!(dry_run);
                    assert!(!json);
                }
                _ => panic!("expected Analyze"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn golden_analyze_help_byte_exact() {
        assert_eq!(
            ANALYZE_HELP,
            include_str!("../tests/golden/help-analyze.txt")
        );
    }

    #[test]
    fn golden_root_help_byte_exact() {
        assert_eq!(ROOT_HELP, include_str!("../tests/golden/help.txt"));
    }

    #[test]
    fn golden_audit_help_byte_exact() {
        assert_eq!(AUDIT_HELP, include_str!("../tests/golden/help-audit.txt"));
    }

    #[test]
    fn golden_clean_help_byte_exact() {
        assert_eq!(CLEAN_HELP, include_str!("../tests/golden/help-clean.txt"));
    }

    #[test]
    fn golden_optimize_help_byte_exact() {
        assert_eq!(
            OPTIMIZE_HELP,
            include_str!("../tests/golden/help-optimize.txt")
        );
    }

    #[test]
    fn golden_status_help_byte_exact() {
        assert_eq!(STATUS_HELP, include_str!("../tests/golden/help-status.txt"));
    }

    #[test]
    fn golden_uninstall_help_byte_exact() {
        assert_eq!(
            UNINSTALL_HELP,
            include_str!("../tests/golden/help-uninstall.txt")
        );
    }
    #[test]
    fn golden_history_help_byte_exact() {
        assert_eq!(
            HISTORY_HELP,
            include_str!("../tests/golden/help-history.txt")
        );
    }

    #[test]
    fn golden_installer_help_byte_exact() {
        assert_eq!(
            INSTALLER_HELP,
            include_str!("../tests/golden/help-installer.txt")
        );
    }

    #[test]
    fn golden_purge_help_byte_exact() {
        assert_eq!(PURGE_HELP, include_str!("../tests/golden/help-purge.txt"));
    }

    #[test]
    fn history_flag_parsing() {
        match parse(&args(&["history", "--json", "--limit", "50"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::History { limit, json } => {
                    assert_eq!(limit, 50);
                    assert!(json);
                }
                _ => panic!("expected History"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn status_watch_and_interval_flags_parse() {
        match parse(&args(&["status", "--watch", "--interval", "2s"])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Status {
                    json,
                    watch,
                    interval,
                } => {
                    assert!(!json);
                    assert!(watch);
                    assert_eq!(interval.as_deref(), Some("2s"));
                }
                _ => panic!("expected Status"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn purge_flag_parsing() {
        match parse(&args(&[
            "purge",
            "--dry-run",
            "-y",
            "--include-empty",
            "--paths=/tmp/a,/tmp/b",
        ])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Purge {
                    paths,
                    dry_run,
                    yes,
                    include_empty,
                } => {
                    assert_eq!(
                        paths,
                        vec![
                            std::path::PathBuf::from("/tmp/a"),
                            std::path::PathBuf::from("/tmp/b")
                        ]
                    );
                    assert!(dry_run);
                    assert!(yes);
                    assert!(include_empty);
                }
                _ => panic!("expected Purge"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn installer_flag_parsing() {
        match parse(&args(&[
            "installer",
            "--dry-run",
            "-y",
            "--paths=/tmp/install",
        ])) {
            Outcome::Run { cmd, .. } => match cmd {
                Command::Installer {
                    paths,
                    dry_run,
                    yes,
                } => {
                    assert_eq!(paths, vec![std::path::PathBuf::from("/tmp/install")]);
                    assert!(dry_run);
                    assert!(yes);
                }
                _ => panic!("expected Installer"),
            },
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn status_invalid_interval_flag() {
        match parse(&args(&["status", "--interval", "bogus"])) {
            Outcome::Stderr { text, code } => {
                assert_eq!(code, 1);
                assert!(text.contains("invalid duration \"bogus\""), "{text}");
            }
            _ => panic!("expected Stderr"),
        }
    }

    #[test]
    fn parse_duration_formats() {
        assert_eq!(
            parse_duration("1s"),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(
            parse_duration("500ms"),
            Some(std::time::Duration::from_millis(500))
        );
        assert_eq!(
            parse_duration("2m"),
            Some(std::time::Duration::from_secs(120))
        );
        assert_eq!(parse_duration("5"), Some(std::time::Duration::from_secs(5)));
        assert_eq!(parse_duration("bogus"), None);
        assert_eq!(parse_duration("0s"), None);
        assert_eq!(parse_duration("0ms"), None);
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("infs"), None);
        assert_eq!(parse_duration("-1s"), None);
        assert_eq!(parse_duration("1e300s"), None);
    }

    #[test]
    fn status_interval_flag_zero_and_infinite_rejected() {
        for arg in ["0s", "0ms", "0", "infs", "1e300s"] {
            match parse(&args(&["status", "--interval", arg])) {
                Outcome::Stderr { text, code } => {
                    assert_eq!(code, 1);
                    assert!(
                        text.contains(&format!("invalid duration \"{arg}\"")),
                        "for {arg}: {text}"
                    );
                }
                _ => panic!("expected Stderr for {arg}"),
            }
        }
    }
}

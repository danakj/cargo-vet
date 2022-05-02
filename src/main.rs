use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{BufReader, Read, Seek};
use std::ops::Deref;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;
use std::{fs, mem};
use std::{fs::File, io::Write, panic, path::PathBuf};

use cargo_metadata::{Metadata, Package, Version};
use clap::{ArgEnum, CommandFactory, Parser, Subcommand};
use eyre::Context;
use flate2::read::GzDecoder;
use format::{AuditEntry, AuditKind, Delta, DiffCache, DiffStat, MetaConfig};
use log::{error, info, trace, warn};
use reqwest::blocking as req;
use serde::{de::Deserialize, ser::Serialize};
use simplelog::{
    ColorChoice, ConfigBuilder, Level, LevelFilter, TermLogger, TerminalMode, WriteLogger,
};
use tar::Archive;

use crate::format::{
    AuditsFile, ConfigFile, CriteriaEntry, DependencyCriteria, ImportsFile, MetaConfigInstance,
    StableMap, Store, UnauditedDependency,
};

pub mod format;
mod resolver;
#[cfg(test)]
mod tests;

type VetError = eyre::Report;

#[derive(Parser)]
#[clap(version, about, long_about = None)]
#[clap(propagate_version = true)]
#[clap(bin_name = "cargo")]
enum FakeCli {
    Vet(Cli),
}

#[derive(clap::Args)]
#[clap(version)]
#[clap(bin_name = "cargo vet")]
/// Supply-chain security for Rust
struct Cli {
    /// Subcommands ("no subcommand" is its own subcommand)
    #[clap(subcommand)]
    command: Option<Commands>,

    // Cargo flags we support and forward to e.g. 'cargo metadata'
    #[clap(flatten)]
    manifest: clap_cargo::Manifest,
    #[clap(flatten)]
    workspace: clap_cargo::Workspace,
    #[clap(flatten)]
    features: clap_cargo::Features,

    // Top-level flags
    /// Do not pull in new "audits".
    #[clap(long)]
    locked: bool,

    /// How verbose logging should be (log level).
    #[clap(long, arg_enum)]
    #[clap(default_value_t = Verbose::Warn)]
    verbose: Verbose,

    /// Instead of stdout, write output to this file.
    #[clap(long)]
    output_file: Option<PathBuf>,

    /// Instead of stderr, write logs to this file (only used after successful CLI parsing).
    #[clap(long)]
    log_file: Option<PathBuf>,

    /// Use the following path as the diff-cache.
    ///
    /// The diff-cache stores the summary results used by vet's suggestion machinery.
    /// This is automatically managed in vet's tempdir, but if you want to manually store
    /// it somewhere more reliable, you can.
    ///
    /// This mostly exists for testing vet itself.
    #[clap(long)]
    diff_cache: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// initialize cargo-vet for your project
    #[clap(disable_version_flag = true)]
    Init(InitArgs),

    /// Accept changes that a foreign audits.toml made to their criteria
    #[clap(disable_version_flag = true)]
    AcceptCriteriaChange(AcceptCriteriaChangeArgs),

    /// Fetch the source of `$package $version`
    #[clap(disable_version_flag = true)]
    Inspect(InspectArgs),

    /// Yield a diff against the last reviewed version.
    #[clap(disable_version_flag = true)]
    Diff(DiffArgs),

    /// Mark `$package $version` as reviewed with `$message`
    #[clap(disable_version_flag = true)]
    Certify(CertifyArgs),

    /// Suggest some low-hanging fruit to review
    #[clap(disable_version_flag = true)]
    Suggest(SuggestArgs),

    /// Reformat all of vet's files (in case you hand-edited them)
    #[clap(disable_version_flag = true)]
    Fmt(FmtArgs),

    /// Print --help as markdown (for generating docs)
    #[clap(disable_version_flag = true)]
    #[clap(hide = true)]
    HelpMarkdown(HelpMarkdownArgs),
}

#[derive(clap::Args)]
struct InitArgs {}

/// Fetches the crate to a temp location and pushd's to it
#[derive(clap::Args)]
struct InspectArgs {
    package: String,
    version: String,
}

/// Emits a diff of the two versions
#[derive(clap::Args)]
struct DiffArgs {
    package: String,
    version1: String,
    version2: String,
}

/// Cerifies the given version
#[derive(clap::Args)]
struct CertifyArgs {
    package: String,
    version1: String,
    version2: Option<String>,
}

#[derive(clap::Args)]
struct SuggestArgs {
    /// Try to suggest even deeper down the dependency tree (approximate guessing).
    ///
    /// By default, if a dependency doesn't have sufficient audits for *itself* then we won't
    /// try to speculate on anything about its dependencies, because we lack sufficient
    /// information to say for certain what is required of those dependencies. This overrides
    /// that by making us assume the dependencies all need the same criteria as the parent.
    #[clap(long)]
    guess_deeper: bool,
}

#[derive(clap::Args)]
struct FmtArgs {}

#[derive(clap::Args)]
struct AcceptCriteriaChangeArgs {}

#[derive(clap::Args)]
struct HelpMarkdownArgs {}

/// Logging verbosity levels
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ArgEnum)]
enum Verbose {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Cli {
    #[cfg(test)]
    pub fn mock() -> Self {
        use clap_cargo::{Features, Manifest, Workspace};

        Self {
            command: None,
            manifest: Manifest::default(),
            workspace: Workspace::default(),
            features: Features::default(),
            locked: true,
            verbose: Verbose::Off,
            output_file: None,
            log_file: None,
            diff_cache: None,
        }
    }
}

/// Absolutely All The Global Configurations
pub struct Config {
    // file: ConfigFile,
    metacfg: MetaConfig,
    metadata: Metadata,
    cli: Cli,
    _cargo: OsString,
    tmp: PathBuf,
    registry_src: Option<PathBuf>,
}

// tmp cache for various shenanigans
static TEMP_DIR_SUFFIX: &str = "cargo-vet-checkout";
static DIFF_CACHE: &str = "diff-cache.toml";
static EMPTY_PACKAGE: &str = "empty";
static PACKAGES: &str = "packages";
static CARGO_OK_FILE: &str = ".cargo-ok";
static CARGO_OK_BODY: &str = "ok";

static CARGO_ENV: &str = "CARGO";
static CARGO_REGISTRY_SRC: &str = "registry/src/";
static DEFAULT_STORE: &str = "supply-chain";
// package.metadata.vet
static PACKAGE_VET_CONFIG: &str = "vet";
// workspace.metadata.vet
static WORKSPACE_VET_CONFIG: &str = "vet";

static AUDITS_TOML: &str = "audits.toml";
static CONFIG_TOML: &str = "config.toml";
static IMPORTS_LOCK: &str = "imports.lock";

pub trait PackageExt {
    fn is_third_party(&self) -> bool;
}

impl PackageExt for Package {
    fn is_third_party(&self) -> bool {
        self.source
            .as_ref()
            .map(|s| s.is_crates_io())
            .unwrap_or(false)
    }
}

fn main() -> Result<(), VetError> {
    let fake_cli = FakeCli::parse();
    let FakeCli::Vet(cli) = fake_cli;

    //////////////////////////////////////////////////////
    // Setup logging / output
    //////////////////////////////////////////////////////

    // Configure our output formats / logging
    let verbosity = match cli.verbose {
        Verbose::Off => LevelFilter::Off,
        Verbose::Warn => LevelFilter::Warn,
        Verbose::Info => LevelFilter::Info,
        Verbose::Debug => LevelFilter::Debug,
        Verbose::Trace => LevelFilter::Trace,
        Verbose::Error => LevelFilter::Error,
    };

    // Init the logger (and make trace logging less noisy)
    if let Some(log_path) = &cli.log_file {
        let log_file = File::create(log_path).unwrap();
        let _ = WriteLogger::init(
            verbosity,
            ConfigBuilder::new()
                .set_location_level(LevelFilter::Off)
                .set_time_level(LevelFilter::Off)
                .set_thread_level(LevelFilter::Off)
                .set_target_level(LevelFilter::Off)
                .build(),
            log_file,
        )
        .unwrap();
    } else {
        let _ = TermLogger::init(
            verbosity,
            ConfigBuilder::new()
                .set_location_level(LevelFilter::Off)
                .set_time_level(LevelFilter::Off)
                .set_thread_level(LevelFilter::Off)
                .set_target_level(LevelFilter::Off)
                .set_level_color(Level::Trace, None)
                .build(),
            TerminalMode::Stderr,
            ColorChoice::Auto,
        );
    }

    // Set a panic hook to redirect to the logger
    panic::set_hook(Box::new(|panic_info| {
        let (filename, line) = panic_info
            .location()
            .map(|loc| (loc.file(), loc.line()))
            .unwrap_or(("<unknown>", 0));
        let cause = panic_info
            .payload()
            .downcast_ref::<String>()
            .map(String::deref)
            .unwrap_or_else(|| {
                panic_info
                    .payload()
                    .downcast_ref::<&str>()
                    .copied()
                    .unwrap_or("<cause unknown>")
            });
        error!(
            "Panic - A panic occurred at {}:{}: {}",
            filename, line, cause
        );
    }));

    // Setup our output stream
    let mut stdout;
    let mut output_f;
    let out: &mut dyn Write = if let Some(output_path) = &cli.output_file {
        output_f = File::create(output_path).unwrap();
        &mut output_f
    } else {
        stdout = std::io::stdout();
        &mut stdout
    };

    ///////////////////////////////////////////////////
    // Fetch cargo metadata
    ///////////////////////////////////////////////////

    let cargo = std::env::var_os(CARGO_ENV).expect("Cargo failed to set $CARGO, how?");
    let mut cmd = cargo_metadata::MetadataCommand::new();
    cmd.cargo_path(&cargo);
    if let Some(manifest_path) = &cli.manifest.manifest_path {
        cmd.manifest_path(manifest_path);
    }
    if cli.features.all_features {
        cmd.features(cargo_metadata::CargoOpt::AllFeatures);
    }
    if cli.features.no_default_features {
        cmd.features(cargo_metadata::CargoOpt::NoDefaultFeatures);
    }
    if !cli.features.features.is_empty() {
        cmd.features(cargo_metadata::CargoOpt::SomeFeatures(
            cli.features.features.clone(),
        ));
    }
    let mut other_options = Vec::new();
    if cli.workspace.all || cli.workspace.workspace {
        other_options.push("--workspace".to_string());
    }
    for package in &cli.workspace.package {
        other_options.push("--package".to_string());
        other_options.push(package.to_string());
    }
    for package in &cli.workspace.exclude {
        other_options.push("--exclude".to_string());
        other_options.push(package.to_string());
    }
    cmd.other_options(other_options);

    info!("Running: {:#?}", cmd.cargo_command());

    let metadata = match cmd.exec() {
        Ok(metadata) => metadata,
        Err(e) => {
            error!("'cargo metadata' failed: {}", e);
            std::process::exit(-1);
        }
    };

    // trace!("Got Metadata! {:#?}", metadata);
    trace!("Got Metadata!");

    //////////////////////////////////////////////////////
    // Parse out our own configuration
    //////////////////////////////////////////////////////

    let default_config = MetaConfigInstance {
        version: Some(1),
        store: Some(Store {
            path: Some(
                metadata
                    .workspace_root
                    .join(DEFAULT_STORE)
                    .into_std_path_buf(),
            ),
        }),
    };

    let workspace_metacfg = || -> Option<MetaConfigInstance> {
        // FIXME: what is `store.path` relative to here?
        MetaConfigInstance::deserialize(metadata.workspace_metadata.get(WORKSPACE_VET_CONFIG)?)
            .map_err(|e| {
                error!(
                    "Workspace had [{WORKSPACE_VET_CONFIG}] but it was malformed: {}",
                    e
                );
                std::process::exit(-1);
            })
            .ok()
    }();

    let package_metacfg = || -> Option<MetaConfigInstance> {
        // FIXME: what is `store.path` relative to here?
        MetaConfigInstance::deserialize(metadata.root_package()?.metadata.get(PACKAGE_VET_CONFIG)?)
            .map_err(|e| {
                error!(
                    "Root package had [{PACKAGE_VET_CONFIG}] but it was malformed: {}",
                    e
                );
                std::process::exit(-1);
            })
            .ok()
    }();

    if workspace_metacfg.is_some() && package_metacfg.is_some() {
        error!("Both a workspace and a package defined [metadata.vet]! We don't know what that means, if you do, let us know!");
        std::process::exit(-1);
    }

    let mut metacfgs = vec![default_config];
    if let Some(metacfg) = workspace_metacfg {
        metacfgs.push(metacfg);
    }
    if let Some(metacfg) = package_metacfg {
        metacfgs.push(metacfg);
    }
    let metacfg = MetaConfig(metacfgs);

    info!("Final Metadata Config: ");
    info!("  - version: {}", metacfg.version());
    info!("  - store.path: {:#?}", metacfg.store_path());

    //////////////////////////////////////////////////////
    // Run the actual command
    //////////////////////////////////////////////////////

    // These commands don't need an instance and can be run anywhere
    let command_is_freestanding = matches!(
        cli.command,
        Some(Commands::HelpMarkdown { .. })
            | Some(Commands::Inspect { .. })
            | Some(Commands::Diff { .. })
    );

    let init = is_init(&metacfg);
    if matches!(cli.command, Some(Commands::Init { .. })) {
        if init {
            error!(
                "'cargo vet' already initialized (store found at {:#?})",
                metacfg.store_path()
            );
            std::process::exit(-1);
        }
    } else if !init && !command_is_freestanding {
        error!(
            "You must run 'cargo vet init' (store not found at {:#?})",
            metacfg.store_path()
        );
        std::process::exit(-1);
    }

    // TODO: make this configurable
    // TODO: maybe this wants to be actually totally random to allow multi-vets?
    let tmp = std::env::temp_dir().join(TEMP_DIR_SUFFIX);
    let registry_src = home::cargo_home()
        .ok()
        .map(|path| path.join(CARGO_REGISTRY_SRC));
    let cfg = Config {
        metacfg,
        metadata,
        cli,
        _cargo: cargo,
        tmp,
        registry_src,
    };

    use Commands::*;
    match &cfg.cli.command {
        None => cmd_vet(out, &cfg),
        Some(Init(sub_args)) => cmd_init(out, &cfg, sub_args),
        Some(AcceptCriteriaChange(sub_args)) => cmd_accept_criteria_change(out, &cfg, sub_args),
        Some(Inspect(sub_args)) => cmd_inspect(out, &cfg, sub_args),
        Some(Certify(sub_args)) => cmd_certify(out, &cfg, sub_args),
        Some(Suggest(sub_args)) => cmd_suggest(out, &cfg, sub_args),
        Some(Diff(sub_args)) => cmd_diff(out, &cfg, sub_args),
        Some(Fmt(sub_args)) => cmd_fmt(out, &cfg, sub_args),
        Some(HelpMarkdown(sub_args)) => cmd_help_md(out, &cfg, sub_args),
    }
}

fn cmd_init(_out: &mut dyn Write, cfg: &Config, _sub_args: &InitArgs) -> Result<(), VetError> {
    // Initialize vet

    // Create store_path
    // - audits.toml (empty, sample criteria)
    // - imports.lock (empty)
    // - config.toml (populated with defaults and full list of third-party crates)
    trace!("initializing...");

    let store_path = cfg.metacfg.store_path();

    let (config, audits, imports) = init_files(&cfg.metadata)?;

    // In theory we don't need `all` here, but this allows them to specify
    // the store as some arbitrarily nested subdir for whatever reason
    // (maybe multiple parallel instances?)
    std::fs::create_dir_all(store_path)?;
    store_audits(store_path, audits)?;
    store_imports(store_path, imports)?;
    store_config(store_path, config)?;

    Ok(())
}

pub fn init_files(metadata: &Metadata) -> Result<(ConfigFile, AuditsFile, ImportsFile), VetError> {
    // Default audits file is empty
    let audits = AuditsFile {
        criteria: StableMap::new(),
        audits: StableMap::new(),
    };

    // Default imports file is empty
    let imports = ImportsFile {
        audits: StableMap::new(),
    };

    // This is the hard one
    let config = {
        let mut dependencies = StableMap::new();
        for package in foreign_packages(metadata) {
            // NOTE: May have multiple copies of a package!
            let item = UnauditedDependency {
                version: package.version.clone(),
                notes: Some("automatically imported by 'cargo vet init'".to_string()),
                suggest: true,
                // TODO: use whether this is a build_and_dev to make this weaker
                criteria: format::DEFAULT_CRITERIA.to_string(),
            };
            dependencies
                .entry(package.name.clone())
                .or_insert(vec![])
                .push(item);
        }
        ConfigFile {
            default_criteria: format::get_default_criteria(),
            imports: StableMap::new(),
            unaudited: dependencies,
            policy: StableMap::new(),
        }
    };

    Ok((config, audits, imports))
}

fn cmd_inspect(out: &mut dyn Write, cfg: &Config, sub_args: &InspectArgs) -> Result<(), VetError> {
    // Download a crate's source to a temp location for review
    let mut cache = Cache::acquire(cfg)?;

    let package = &*sub_args.package;
    let version = Version::from_str(&sub_args.version).expect("could not parse version");

    let to_fetch = &[(package, &version)];
    let fetched_paths = cache.fetch_packages(to_fetch)?;
    let fetched = &fetched_paths[package][&version];

    #[cfg(target_family = "unix")]
    {
        // Loosely borrowed from cargo crev.
        use std::os::unix::process::CommandExt;
        let shell = std::env::var_os("SHELL").unwrap();
        writeln!(out, "Opening nested shell in: {:#?}", fetched)?;
        writeln!(out, "Use `exit` or Ctrl-D to finish.",)?;
        let mut command = std::process::Command::new(shell);
        command.current_dir(fetched.clone()).env("PWD", fetched);
        command.exec();
    }

    #[cfg(not(target_family = "unix"))]
    {
        writeln!(out, "  fetched to {:#?}", fetched)?;
    }

    Ok(())
}

fn cmd_certify(_out: &mut dyn Write, cfg: &Config, sub_args: &CertifyArgs) -> Result<(), VetError> {
    // Certify that you have reviewed a crate's source for some version / delta
    let store_path = cfg.metacfg.store_path();
    let mut audits = load_audits(store_path)?;
    let config = load_config(store_path)?;

    let dependency_criteria = DependencyCriteria::new();

    // FIXME: better error when this goes bad
    let version1 = Version::parse(&sub_args.version1).expect("version1 wasn't a valid Version");
    let version2 = sub_args
        .version2
        .as_ref()
        .map(|v| Version::parse(v).expect("version2 wasn't a valid Version"));

    let kind = if let Some(version2) = version2 {
        // This is a delta audit
        AuditKind::Delta {
            delta: Delta {
                from: version1,
                to: version2,
            },
            dependency_criteria,
        }
    } else {
        AuditKind::Full {
            version: version1,
            dependency_criteria,
        }
    };

    let criteria = config.default_criteria;

    // TODO: source this from git
    let who = Some("?TODO?".to_string());
    // TODO: start an interactive prompt
    let notes = Some("?TODO?".to_string());

    let new_entry = AuditEntry {
        kind,
        criteria,
        who,
        notes,
    };

    // TODO: check if the version makes sense..?
    if !foreign_packages(&cfg.metadata).any(|pkg| pkg.name == sub_args.package) {
        error!("'{}' isn't one of your foreign packages", sub_args.package);
        std::process::exit(-1);
    }

    audits
        .audits
        .entry(sub_args.package.clone())
        .or_insert(vec![])
        .push(new_entry);
    store_audits(store_path, audits)?;

    Ok(())
}

fn cmd_suggest(out: &mut dyn Write, cfg: &Config, sub_args: &SuggestArgs) -> Result<(), VetError> {
    // Run the checker to validate that the current set of deps is covered by the current cargo vet store
    trace!("suggesting...");

    let store_path = cfg.metacfg.store_path();

    let audits = load_audits(store_path)?;
    let mut config = load_config(store_path)?;

    // FIXME: We should probably check in --locked vets if the config has changed its
    // imports and warn if the imports.lock is inconsistent with that..?
    //
    // TODO: error out if the foreign audits changed their criteria (compare to imports.lock)
    let imports = if !cfg.cli.locked {
        fetch_foreign_audits(out, cfg, &config)?
    } else {
        load_imports(store_path)?
    };

    // Delete all unaudited entries except those that are suggest=false
    for (_package, versions) in &mut config.unaudited {
        versions.retain(|e| !e.suggest);
    }

    // DO THE THING!!!!
    let report = resolver::resolve(
        &cfg.metadata,
        &config,
        &audits,
        &imports,
        sub_args.guess_deeper,
    );
    report.print_suggest(out, cfg)?;

    Ok(())
}
fn cmd_diff(out: &mut dyn Write, cfg: &Config, sub_args: &DiffArgs) -> Result<(), VetError> {
    let mut cache = Cache::acquire(cfg)?;

    let package = &*sub_args.package;
    let version1 = sub_args
        .version1
        .parse()
        .expect("Failed to parse first version");
    let version2 = sub_args
        .version2
        .parse()
        .expect("Failed to parse second version");

    writeln!(
        out,
        "fetching {} {} and {} ...",
        sub_args.package, version1, version2,
    )?;

    let to_fetch = &[(package, &version1), (package, &version2)];
    let fetched_paths = cache.fetch_packages(to_fetch)?;
    let fetched1 = &fetched_paths[package][&version1];
    let fetched2 = &fetched_paths[package][&version2];

    writeln!(out)?;

    diff_crate(out, cfg, fetched1, fetched2)?;

    Ok(())
}

fn cmd_vet(out: &mut dyn Write, cfg: &Config) -> Result<(), VetError> {
    // Run the checker to validate that the current set of deps is covered by the current cargo vet store
    trace!("vetting...");

    let store_path = cfg.metacfg.store_path();

    let audits = load_audits(store_path)?;
    let config = load_config(store_path)?;

    // FIXME: We should probably check in --locked vets if the config has changed its
    // imports and warn if the imports.lock is inconsistent with that..?
    //
    // TODO: error out if the foreign audits changed their criteria (compare to imports.lock)
    let imports = if !cfg.cli.locked {
        fetch_foreign_audits(out, cfg, &config)?
    } else {
        load_imports(store_path)?
    };

    // DO THE THING!!!!
    let report = resolver::resolve(&cfg.metadata, &config, &audits, &imports, false);
    report.print_report(out, cfg)?;

    // Only save imports if we succeeded, to avoid any modifications on error.
    if !report.has_errors() {
        trace!("Saving imports.lock...");
        store_imports(store_path, imports)?;
    }

    Ok(())
}

fn cmd_fmt(_out: &mut dyn Write, cfg: &Config, _sub_args: &FmtArgs) -> Result<(), VetError> {
    // Reformat all the files (just load and store them, formatting is implict).
    trace!("formatting...");

    let store_path = cfg.metacfg.store_path();

    store_audits(store_path, load_audits(store_path)?)?;
    store_config(store_path, load_config(store_path)?)?;
    store_imports(store_path, load_imports(store_path)?)?;

    Ok(())
}

fn cmd_accept_criteria_change(
    _out: &mut dyn Write,
    _cfg: &Config,
    _sub_args: &AcceptCriteriaChangeArgs,
) -> Result<(), VetError> {
    // Accept changes that a foreign audits.toml made to their criteria.
    trace!("accepting...");

    error!("TODO: unimplemented feature!");

    Ok(())
}

/// Perform crimes on clap long_help to generate markdown docs
fn cmd_help_md(
    out: &mut dyn Write,
    _cfg: &Config,
    _sub_args: &HelpMarkdownArgs,
) -> Result<(), VetError> {
    let app_name = "cargo-vet";
    let pretty_app_name = "cargo vet";
    // Make a new App to get the help message this time.

    writeln!(out, "# {pretty_app_name} CLI manual")?;
    writeln!(out)?;
    writeln!(
        out,
        "> This manual can be regenerated with `{pretty_app_name} help-markdown`"
    )?;
    writeln!(out)?;

    let mut fake_cli = FakeCli::command();
    let full_command = fake_cli.get_subcommands_mut().next().unwrap();
    let mut todo = vec![full_command];
    let mut is_full_command = true;

    while let Some(command) = todo.pop() {
        let mut help_buf = Vec::new();
        command.write_long_help(&mut help_buf).unwrap();
        let help = String::from_utf8(help_buf).unwrap();

        // First line is --version
        let mut lines = help.lines();
        let version_line = lines.next().unwrap();
        let subcommand_name = command.get_name();
        let pretty_subcommand_name;

        if is_full_command {
            pretty_subcommand_name = String::new();
            writeln!(out, "Version: `{version_line}`")?;
            writeln!(out)?;
        } else {
            pretty_subcommand_name = format!("{pretty_app_name} {subcommand_name} ");
            // Give subcommands some breathing room
            writeln!(out, "<br><br><br>")?;
            writeln!(out, "## {pretty_subcommand_name}")?;
        }

        let mut in_subcommands_listing = false;
        let mut in_usage = false;
        for line in lines {
            // Use a trailing colon to indicate a heading
            if let Some(heading) = line.strip_suffix(':') {
                if !line.starts_with(' ') {
                    // SCREAMING headers are Main headings
                    if heading.to_ascii_uppercase() == heading {
                        in_subcommands_listing = heading == "SUBCOMMANDS";
                        in_usage = heading == "USAGE";

                        writeln!(out, "### {pretty_subcommand_name}{heading}")?;
                    } else {
                        writeln!(out, "### {heading}")?;
                    }
                    continue;
                }
            }

            if in_subcommands_listing && !line.starts_with("     ") {
                // subcommand names are list items
                let own_subcommand_name = line.trim();
                write!(
                    out,
                    "* [{own_subcommand_name}](#{app_name}-{own_subcommand_name}): "
                )?;
                continue;
            }
            // The rest is indented, get rid of that
            let line = line.trim();

            // Usage strings get wrapped in full code blocks
            if in_usage && line.starts_with(&subcommand_name) {
                writeln!(out, "```")?;
                if is_full_command {
                    writeln!(out, "{line}")?;
                } else {
                    writeln!(out, "{pretty_app_name} {line}")?;
                }

                writeln!(out, "```")?;
                continue;
            }

            // argument names are subheadings
            if line.starts_with('-') || line.starts_with('<') {
                writeln!(out, "#### `{line}`")?;
                continue;
            }

            // escape default/value strings
            if line.starts_with('[') {
                writeln!(out, "\\{line}  ")?;
                continue;
            }

            // Normal paragraph text
            writeln!(out, "{line}")?;
        }
        writeln!(out)?;

        todo.extend(command.get_subcommands_mut());
        is_full_command = false;
    }

    Ok(())
}

// Utils

fn is_init(metacfg: &MetaConfig) -> bool {
    // Probably want to do more here later...
    metacfg.store_path().exists()
}

fn foreign_packages(metadata: &Metadata) -> impl Iterator<Item = &Package> {
    // Only analyze things from crates.io (no source = path-dep / workspace-member)
    metadata
        .packages
        .iter()
        .filter(|package| package.is_third_party())
}

fn load_toml<T>(path: &Path) -> Result<T, VetError>
where
    T: for<'a> Deserialize<'a>,
{
    let mut reader = BufReader::new(File::open(path)?);
    let mut string = String::new();
    reader.read_to_string(&mut string)?;
    let toml = toml::from_str(&string)?;
    Ok(toml)
}
fn store_toml<T>(path: &Path, heading: &str, val: T) -> Result<(), VetError>
where
    T: Serialize,
{
    // FIXME: do this in a temp file and swap it into place to avoid corruption?
    let toml_string = toml::to_string(&val)?;
    let mut output = File::create(path)?;
    writeln!(&mut output, "{}\n{}", heading, toml_string)?;
    Ok(())
}

fn load_audits(store_path: &Path) -> Result<AuditsFile, VetError> {
    // TODO: do integrity checks? (for things like criteria keys being valid)
    let path = store_path.join(AUDITS_TOML);
    let file: AuditsFile = load_toml(&path)?;
    file.validate()?;
    Ok(file)
}

fn load_config(store_path: &Path) -> Result<ConfigFile, VetError> {
    // TODO: do integrity checks?
    let path = store_path.join(CONFIG_TOML);
    let file: ConfigFile = load_toml(&path)?;
    file.validate()?;
    Ok(file)
}

fn load_imports(store_path: &Path) -> Result<ImportsFile, VetError> {
    // TODO: do integrity checks?
    let path = store_path.join(IMPORTS_LOCK);
    let file: ImportsFile = load_toml(&path)?;
    file.validate()?;
    Ok(file)
}

fn load_diff_cache(diff_cache_path: &Path) -> Result<DiffCache, VetError> {
    let file: DiffCache = load_toml(diff_cache_path)?;
    Ok(file)
}

fn store_audits(store_path: &Path, audits: AuditsFile) -> Result<(), VetError> {
    let heading = r###"
# cargo-vet audits file
"###;

    let path = store_path.join(AUDITS_TOML);
    store_toml(&path, heading, audits)?;
    Ok(())
}
fn store_config(store_path: &Path, config: ConfigFile) -> Result<(), VetError> {
    let heading = r###"
# cargo-vet config file
"###;

    let path = store_path.join(CONFIG_TOML);
    store_toml(&path, heading, config)?;
    Ok(())
}
fn store_imports(store_path: &Path, imports: ImportsFile) -> Result<(), VetError> {
    let heading = r###"
# cargo-vet imports lock
"###;

    let path = store_path.join(IMPORTS_LOCK);
    store_toml(&path, heading, imports)?;
    Ok(())
}
fn store_diff_cache(diff_cache_path: &Path, diff_cache: DiffCache) -> Result<(), VetError> {
    let heading = "";

    store_toml(diff_cache_path, heading, diff_cache)?;
    Ok(())
}

pub struct Cache {
    // FIXME: stubbed out
    _lock: Option<()>,
    root: Option<PathBuf>,
    cargo_registry: Option<PathBuf>,
    diff_cache_path: Option<PathBuf>,
    diff_cache: DiffCache,
}

impl Drop for Cache {
    fn drop(&mut self) {
        if let Some(_lock) = self._lock {
            // FIXME: Release the lock
        }
        if let Some(diff_cache_path) = &self.diff_cache_path {
            // Write back the diff_cache
            store_diff_cache(
                diff_cache_path,
                mem::replace(&mut self.diff_cache, DiffCache::new()),
            )
            .unwrap();
        }
    }
}

impl Cache {
    pub fn acquire(cfg: &Config) -> Result<Self, VetError> {
        if cfg.registry_src.is_none() {
            // If the registry_src isn't set, then assume we're running in tests and mocked.
            // In that case, don't read/write disk for the DiffCache
            return Ok(Cache {
                _lock: None,
                root: None,
                cargo_registry: None,
                diff_cache_path: None,
                diff_cache: DiffCache::new(),
            });
        }

        let root = cfg.tmp.clone();
        let empty = root.join(EMPTY_PACKAGE);
        let packages: PathBuf = root.join(PACKAGES);

        // Make sure our cache exists
        if !root.exists() {
            fs::create_dir_all(&root)?;
        }
        // Now acquire the lockfile
        let lock: () = {
            // FIXME: implement some kind of lockfile mechanism to avoid concurrent modification
        };

        // Make sure everything else exists
        if !empty.exists() {
            fs::create_dir_all(&empty)?;
        }
        if !packages.exists() {
            fs::create_dir_all(&packages)?;
        }

        // Setup the diff_cache.
        let diff_cache_path = cfg
            .cli
            .diff_cache
            .clone()
            .unwrap_or_else(|| root.join(DIFF_CACHE));
        let diff_cache = if let Ok(cache) = load_diff_cache(&diff_cache_path) {
            cache
        } else {
            // Might be our first run, create a fresh diff-cache that we'll write back at the end
            DiffCache::new()
        };

        // Try to get the cargo registry
        let cargo_registry = find_cargo_registry(cfg);
        if let Err(e) = &cargo_registry {
            warn!("Couldn't find cargo registry: {e}");
        }

        Ok(Self {
            _lock: Some(lock),
            root: Some(root),
            diff_cache_path: Some(diff_cache_path),
            diff_cache,
            cargo_registry: cargo_registry.ok(),
        })
    }

    pub fn fetch_packages<'a>(
        &mut self,
        packages: &[(&'a str, &'a Version)],
    ) -> Result<BTreeMap<&'a str, BTreeMap<&'a Version, PathBuf>>, VetError> {
        // Don't do anything if we're mocked, or there is no work to do
        if self.root.is_none() || packages.is_empty() {
            return Ok(BTreeMap::new());
        }

        let root = self.root.as_ref().unwrap();
        let fetch_dir = root.join(PACKAGES);
        let cargo_registry = self.cargo_registry.as_ref();

        let mut paths = BTreeMap::<&str, BTreeMap<&Version, PathBuf>>::new();
        let mut to_download = Vec::new();

        // Get all the cached things / find out what needs to be downloaded.
        for (name, version) in packages {
            let path = if **version == resolver::ROOT_VERSION {
                // Empty package
                root.join(EMPTY_PACKAGE)
            } else {
                // First try to get a cached copy from cargo's register or our own
                let dir_name = format!("{}-{}", name, version);

                let cached = cargo_registry
                    .map(|reg| reg.join(&dir_name))
                    .filter(|path| fetch_is_ok(path))
                    .unwrap_or_else(|| fetch_dir.join(&dir_name));

                if !fetch_is_ok(&cached) {
                    // If we don't have a cached copy, push this to the download queue
                    to_download.push((name, version, cached.clone()));
                }

                // Either this path exists or we'll download it, either way, it's right
                cached
            };

            paths.entry(name).or_default().insert(version, path);
        }

        if !to_download.is_empty() {
            trace!("downloading {} packages", to_download.len());
        }
        // If there is anything to download, do it
        for (name, version, to_dir) in to_download {
            trace!("  downloading {}:{} to {}", name, version, to_dir.display());
            // FIXME: make this all async instead of blocking
            self.download_package(name, version, &to_dir)?;
        }

        trace!("all fetched!");

        Ok(paths)
    }

    pub fn fetch_and_diffstat_all(
        &mut self,
        package: &str,
        diffs: &BTreeSet<Delta>,
    ) -> Result<DiffRecommendation, VetError> {
        // If there's no registry path setup, assume we're in tests and mocking.
        let mut all_versions = BTreeSet::new();

        for delta in diffs {
            let is_cached = self
                .diff_cache
                .get(package)
                .and_then(|cache| cache.get(delta))
                .is_some();
            if !is_cached {
                all_versions.insert(&delta.from);
                all_versions.insert(&delta.to);
            }
        }

        let mut best_rec: Option<DiffRecommendation> = None;
        let to_fetch = all_versions
            .iter()
            .map(|v| (package, *v))
            .collect::<Vec<_>>();
        let fetches = self.fetch_packages(&to_fetch)?;

        for delta in diffs {
            let cached = self
                .diff_cache
                .get(package)
                .and_then(|cache| cache.get(delta))
                .cloned();

            let diffstat = if let Some(cached) = cached {
                // Hooray, we have the cached result!
                cached
            } else {
                let from = fetches.get(package).and_then(|m| m.get(&delta.from));
                let to = fetches.get(package).and_then(|m| m.get(&delta.to));

                if let (Some(from), Some(to)) = (from, to) {
                    // Have fetches, do a real diffstat
                    let diffstat = diffstat_crate(from, to)?;
                    self.diff_cache
                        .entry(package.to_string())
                        .or_insert(StableMap::new())
                        .insert(delta.clone(), diffstat.clone());
                    diffstat
                } else {
                    // If we don't have fetches, assume we want mocked results
                    warn!("Missing fetches, assuming we're in tests and mocking");

                    let from_len = delta.from.major * delta.from.major;
                    let to_len: u64 = delta.to.major * delta.to.major;
                    let diff = to_len as i64 - from_len as i64;
                    let count = diff.unsigned_abs();
                    let raw = if diff < 0 {
                        format!("-{}", count)
                    } else {
                        format!("+{}", count)
                    };
                    DiffStat { raw, count }
                }
            };

            let rec = DiffRecommendation {
                from: delta.from.clone(),
                to: delta.to.clone(),
                diffstat,
            };

            if let Some(best) = best_rec.as_ref() {
                if best.diffstat.count > rec.diffstat.count {
                    best_rec = Some(rec);
                }
            } else {
                best_rec = Some(rec);
            }
        }

        Ok(best_rec.unwrap())
    }

    fn download_package(
        &mut self,
        package: &str,
        version: &Version,
        to_dir: &Path,
    ) -> Result<(), VetError> {
        // Download to an anonymous temp file
        let url = format!("https://crates.io/api/v1/crates/{package}/{version}/download");
        let mut tempfile = tempfile::tempfile()?;
        let bytes = req::get(url).and_then(|r| r.bytes())?;
        tempfile.write_all(&bytes[..])?;
        tempfile.rewind()?;

        // Now unpack it
        self.unpack_package(&tempfile, to_dir)?;

        Ok(())
    }

    fn unpack_package(&mut self, tarball: &File, unpack_dir: &Path) -> Result<(), VetError> {
        // If we get here and the unpack_dir exists, this implies we had a previously failed fetch,
        // blast it away so we can have a clean slate!
        if unpack_dir.exists() {
            fs::remove_dir_all(unpack_dir)?;
        }
        fs::create_dir(unpack_dir)?;
        let lockfile = unpack_dir.join(CARGO_OK_FILE);
        let gz = GzDecoder::new(tarball);
        let mut tar = Archive::new(gz);
        let prefix = unpack_dir.file_name().unwrap();
        let parent = unpack_dir.parent().unwrap();
        for entry in tar.entries()? {
            let mut entry = entry.wrap_err("failed to iterate over archive")?;
            let entry_path = entry
                .path()
                .wrap_err("failed to read entry path")?
                .into_owned();

            // We're going to unpack this tarball into the global source
            // directory, but we want to make sure that it doesn't accidentally
            // (or maliciously) overwrite source code from other crates. Cargo
            // itself should never generate a tarball that hits this error, and
            // crates.io should also block uploads with these sorts of tarballs,
            // but be extra sure by adding a check here as well.
            if !entry_path.starts_with(prefix) {
                return Err(eyre::eyre!(
                    "invalid tarball downloaded, contains \
                        a file at {:?} which isn't under {:?}",
                    entry_path,
                    prefix
                ));
            }
            // Unpacking failed
            let result = entry.unpack_in(parent).map_err(VetError::from);
            result.wrap_err_with(|| {
                format!("failed to unpack entry at `{}`", entry_path.display())
            })?;
        }

        // The lock file is created after unpacking so we overwrite a lock file
        // which may have been extracted from the package.
        let mut ok = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lockfile)
            .wrap_err_with(|| format!("failed to open `{}`", lockfile.display()))?;

        // Write to the lock file to indicate that unpacking was successful.
        write!(ok, "ok")?;

        Ok(())
    }
}

fn fetch_is_ok(fetch: &Path) -> bool {
    if !fetch.exists() || !fetch.is_dir() {
        return false;
    }

    let ok_contents = || -> Result<String, std::io::Error> {
        let mut ok_file = File::open(fetch.join(CARGO_OK_FILE))?;
        let mut contents = String::new();
        ok_file.read_to_string(&mut contents)?;
        Ok(contents)
    };

    if let Ok(ok) = ok_contents() {
        ok == CARGO_OK_BODY
    } else {
        false
    }
}

fn find_cargo_registry(cfg: &Config) -> Result<PathBuf, VetError> {
    // Find the cargo registry
    //
    // This is all unstable nonsense so being a bit paranoid here so that we can notice
    // when things get weird and understand corner cases better...
    if cfg.registry_src.is_none() {
        return Err(eyre::eyre!("Could not resolve CARGO_HOME!?"));
    }

    let registry_src = cfg.registry_src.as_ref().unwrap();
    if !registry_src.exists() {
        return Err(eyre::eyre!("Cargo registry src cache doesn't exist!?"));
    }

    // There's some weird opaque directory name here, so no hardcoding of the path
    let mut real_src_dir = None;
    for entry in std::fs::read_dir(registry_src)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if real_src_dir.is_some() {
                warn!("Found multiple subdirectories in CARGO_HOME/registry/src");
                warn!("  Preferring any named github.com-*");
                if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("github.com-")
                {
                    real_src_dir = Some(path);
                }
            } else {
                real_src_dir = Some(path);
            }
        }
    }

    if let Some(real_src_dir) = real_src_dir {
        Ok(real_src_dir)
    } else {
        Err(eyre::eyre!("failed to find cargo package sources"))
    }
}

fn diff_crate(
    _out: &mut dyn Write,
    _cfg: &Config,
    version1: &Path,
    version2: &Path,
) -> Result<(), VetError> {
    // FIXME: mask out .cargo_vcs_info.json
    // FIXME: look into libgit2 vs just calling git

    let status = Command::new("git")
        .arg("diff")
        .arg("--no-index")
        .arg(version1)
        .arg(version2)
        .status()?;

    // TODO: pretty sure this is wrong, should use --exit-code and copy diffstat_crate's logic
    // (not actually sure what the default exit status logic is!)
    if !status.success() {
        return Err(eyre::eyre!(
            "git diff failed! (this is probably just a bug in how we check error codes)"
        ));
    }

    Ok(())
}

pub struct DiffRecommendation {
    from: Version,
    to: Version,
    diffstat: DiffStat,
}

fn diffstat_crate(version1: &Path, version2: &Path) -> Result<DiffStat, VetError> {
    trace!("diffstating {version1:#?} {version2:#?}");
    // FIXME: mask out .cargo_vcs_info.json
    // FIXME: look into libgit2 vs just calling git

    let out = Command::new("git")
        .arg("diff")
        .arg("--no-index")
        .arg("--shortstat")
        .arg(version1)
        .arg(version2)
        .output()?;

    // TODO: don't unwrap this
    let status = out.status.code().unwrap();
    // 0 = empty
    // 1 = some diff
    if status != 0 && status != 1 {
        return Err(eyre::eyre!(
            "command failed!\nout:\n{}\nstderr:\n{}",
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap()
        ));
    }

    let diffstat = String::from_utf8(out.stdout)?;

    let count = if diffstat.is_empty() {
        0
    } else {
        // 3 files changed, 9 insertions(+), 3 deletions(-)
        let mut parts = diffstat.split(',');
        parts.next().unwrap(); // Discard files

        fn parse_diffnum(part: Option<&str>) -> Option<u64> {
            part?.trim().split_once(' ')?.0.parse().ok()
        }

        let added: u64 = parse_diffnum(parts.next()).unwrap_or(0);
        let removed: u64 = parse_diffnum(parts.next()).unwrap_or(0);

        assert_eq!(
            parts.next(),
            None,
            "diffstat had more parts than expected? {}",
            diffstat
        );

        added + removed
    };

    Ok(DiffStat {
        raw: diffstat,
        count,
    })
}

fn fetch_foreign_audits(
    _out: &mut dyn Write,
    _cfg: &Config,
    config: &ConfigFile,
) -> Result<ImportsFile, VetError> {
    // Download all the foreign audits.toml files that we trust
    let mut audits = StableMap::new();
    for (name, import) in &config.imports {
        let url = &import.url;
        // FIXME: this should probably be async but that's a Whole Thing and these files are small.
        let audit_txt = req::get(url).and_then(|r| r.text());
        if let Err(e) = audit_txt {
            return Err(eyre::eyre!("Could not load {name} @ {url} - {e}"));
        }
        let audit_file: Result<AuditsFile, _> = toml::from_str(&audit_txt.unwrap());
        if let Err(e) = audit_file {
            return Err(eyre::eyre!("Could not parse {name} @ {url} - {e}"));
        }

        // TODO: do integrity checks? (share code with load_audits/load_imports here...)

        audits.insert(name.clone(), audit_file.unwrap());
    }

    Ok(ImportsFile { audits })
}

/*

*/

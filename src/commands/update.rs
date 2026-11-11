use super::command::Command;
use super::install::Install;
use super::uninstall::Uninstall;
use crate::alias::{create_alias, list_aliases_in};
use crate::config::FnmConfig;
use crate::current_version::current_version;
use crate::installed_versions;
use crate::outln;
use crate::progress::ProgressConfig;
use crate::remote_node_index;
use crate::user_version::UserVersion;
use crate::version::Version;
use colored::Colorize;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(clap::Parser, Debug, Default)]
pub struct Update {
    /// Only update installed versions matching these versions, aliases or semver ranges
    pub version: Vec<UserVersion>,

    /// Only report available updates, without installing anything or moving aliases
    #[clap(long)]
    pub dry_run: bool,

    /// Uninstall the superseded version after a successful update
    #[clap(long)]
    pub prune: bool,

    /// Show an interactive progress bar for the download
    /// status.
    #[clap(long, default_value_t)]
    #[arg(value_enum)]
    pub progress: ProgressConfig,
}

impl Command for Update {
    type Error = Error;

    fn apply(self, config: &FnmConfig) -> Result<(), Self::Error> {
        let installed = installed_versions_without_creating(config)?;
        let aliases = read_aliases(config).map_err(|source| Error::AliasReading { source })?;
        let selected = filter_installed(&installed, &self.version, &aliases, config)?;

        if selected.is_empty() {
            outln!(config, Info, "No fnm-managed Node.js versions found.");
            return Ok(());
        }

        let installed_semvers = semvers_of(&selected);
        let remote = remote_versions(config)?;
        let candidates = build_candidates(&installed_semvers, &remote, &aliases);

        print_table(config, &candidates);

        if candidates
            .iter()
            .all(|candidate| candidate.update().is_none())
        {
            outln!(
                config,
                Info,
                "{}",
                "All installed Node.js versions are up to date.".green()
            );
            return Ok(());
        }

        if self.dry_run {
            outln!(
                config,
                Info,
                "Run {} to apply these updates.",
                "fnm update".cyan()
            );
            return Ok(());
        }

        let mut updated = 0_usize;
        let mut up_to_date = 0_usize;
        let mut failures = Vec::new();

        for candidate in &candidates {
            let Some(latest) = candidate.update() else {
                up_to_date += 1;
                continue;
            };

            match self.install_and_migrate(&candidate.installed, latest, &candidate.aliases, config)
            {
                Ok(()) => updated += 1,
                Err(failure) => {
                    outln!(config, Error, "{} {}", "error:".red().bold(), failure.red());
                    failures.push(failure);
                }
            }
        }

        if updated > 0 || up_to_date > 0 {
            outln!(
                config,
                Info,
                "{}",
                format!(
                    "Updated {} version{}, {} already up to date.",
                    updated,
                    if updated == 1 { "" } else { "s" },
                    up_to_date
                )
                .cyan()
            );
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::UpdatesFailed {
                count: failures.len(),
            })
        }
    }
}

impl Update {
    fn install_and_migrate(
        &self,
        old: &node_semver::Version,
        new: &node_semver::Version,
        aliases: &[String],
        config: &FnmConfig,
    ) -> Result<(), String> {
        outln!(
            config,
            Info,
            "Updating {} {} {}",
            display_version(old).cyan(),
            "->".dimmed(),
            display_version(new).cyan()
        );

        Install {
            version: Some(UserVersion::Full(Version::Semver(new.clone()))),
            lts: false,
            latest: false,
            progress: self.progress.clone(),
            r#use: false,
        }
        .apply(config)
        .map_err(|err| {
            format!(
                "Can't install {} (v{old} was left in place, aliases untouched): {err}",
                display_version(new)
            )
        })?;

        let mut alias_failures = Vec::new();
        for alias in aliases {
            match create_alias(config, alias, &Version::Semver(new.clone())) {
                Ok(()) => outln!(
                    config,
                    Info,
                    "  moved alias {} {} {}",
                    format!("'{alias}'").cyan(),
                    "->".dimmed(),
                    display_version(new).cyan()
                ),
                Err(source) => alias_failures.push(format!(
                    "Can't move alias '{alias}' to {}: {source}",
                    display_version(new)
                )),
            }
        }

        if !alias_failures.is_empty() {
            return Err(alias_failures.join("\n"));
        }

        let current = current_semver(config);
        if current.as_ref() == Some(old) {
            outln!(
                config,
                Info,
                "  {} this shell is still using {}; run {} to switch",
                "note:".cyan(),
                display_version(old).cyan(),
                format!("fnm use {}", display_version(new)).cyan()
            );
        }

        if self.prune {
            prune_old_version(old, current.as_ref() == Some(old), config);
        }

        Ok(())
    }
}

fn prune_old_version(old: &node_semver::Version, is_current: bool, config: &FnmConfig) {
    if is_current {
        outln!(
            config,
            Error,
            "{} skipping prune of {} because this shell is using it",
            "warning:".yellow().bold(),
            display_version(old)
        );
        return;
    }

    outln!(
        config,
        Info,
        "  removing superseded {}",
        display_version(old).cyan()
    );
    let result = Uninstall {
        version: Some(UserVersion::Full(Version::Semver(old.clone()))),
    }
    .apply(config);

    if let Err(err) = result {
        outln!(
            config,
            Error,
            "{} couldn't remove {}, it was left in place: {}",
            "warning:".yellow().bold(),
            display_version(old),
            err
        );
    }
}

/// A `(major, channel)` pair. Stable releases and each prerelease channel form
/// their own track, so `22.0.0` and `22.0.0-rc.1` never update into each other.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Track {
    major: u64,
    channel: Option<String>,
}

impl Track {
    fn of(version: &node_semver::Version) -> Self {
        Self {
            major: version.major,
            channel: channel_of(version),
        }
    }

    fn contains(&self, version: &node_semver::Version) -> bool {
        version.major == self.major
            && match &self.channel {
                None => !version.is_prerelease(),
                Some(channel) => channel_of(version).as_ref() == Some(channel),
            }
    }
}

fn channel_of(version: &node_semver::Version) -> Option<String> {
    version
        .pre_release
        .first()
        .map(|identifier| match identifier {
            node_semver::Identifier::Numeric(number) => number.to_string(),
            node_semver::Identifier::AlphaNumeric(name) => name.to_lowercase(),
        })
}

#[derive(Debug)]
struct Candidate {
    installed: node_semver::Version,
    aliases: Vec<String>,
    latest: Option<node_semver::Version>,
    stable_notice: Option<node_semver::Version>,
    /// Older installs of the same track. They are never updated or removed,
    /// so a second install of the same line can't fight over its aliases.
    left_unchanged: Vec<node_semver::Version>,
}

impl Candidate {
    /// The version to update to, if the latest release on this track is newer
    /// than what is installed.
    fn update(&self) -> Option<&node_semver::Version> {
        self.latest
            .as_ref()
            .filter(|latest| *latest > &self.installed)
    }
}
/// Enumerate installed versions without creating the installations directory.
fn installed_versions_without_creating(config: &FnmConfig) -> Result<Vec<Version>, Error> {
    let installations_dir = config.base_dir_with_default().join("node-versions");
    match installed_versions::list(&installations_dir) {
        Ok(versions) => Ok(versions),
        Err(installed_versions::Error::IoError { source })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(Vec::new())
        }
        Err(source) => Err(Error::VersionListing { source }),
    }
}

/// Snapshot the aliases by the version they point at, without creating the
/// aliases directory.
fn read_aliases(config: &FnmConfig) -> Result<BTreeMap<String, Vec<String>>, std::io::Error> {
    let aliases_dir = config.base_dir_with_default().join("aliases");
    let aliases = match list_aliases_in(&aliases_dir) {
        Ok(aliases) => aliases,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => return Err(err),
    };

    let mut by_version: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for alias in aliases {
        by_version
            .entry(alias.s_ver().to_string())
            .or_default()
            .push(alias.name().to_string());
    }
    Ok(by_version)
}

fn filter_installed(
    installed: &[Version],
    filters: &[UserVersion],
    aliases: &BTreeMap<String, Vec<String>>,
    config: &FnmConfig,
) -> Result<Vec<Version>, Error> {
    if filters.is_empty() {
        return Ok(installed.to_vec());
    }

    for filter in filters {
        if !installed
            .iter()
            .any(|version| matches_filter(filter, version, aliases, config))
        {
            return Err(Error::VersionNotInstalled {
                version: filter.clone(),
            });
        }
    }

    Ok(installed
        .iter()
        .filter(|version| {
            filters
                .iter()
                .any(|filter| matches_filter(filter, version, aliases, config))
        })
        .cloned()
        .collect())
}

fn matches_filter(
    filter: &UserVersion,
    version: &Version,
    aliases: &BTreeMap<String, Vec<String>>,
    config: &FnmConfig,
) -> bool {
    match filter.alias_name() {
        // Alias filters are resolved against the read-only snapshot so a dry run
        // never touches the aliases directory.
        Some(alias_name) => aliases
            .get(&version.v_str())
            .is_some_and(|version_aliases| {
                version_aliases.iter().any(|alias| alias == &alias_name)
            }),
        None => filter.matches(version, config),
    }
}

fn semvers_of(versions: &[Version]) -> Vec<node_semver::Version> {
    versions
        .iter()
        .filter_map(|version| match version {
            Version::Semver(semver) => Some(semver.clone()),
            _ => None,
        })
        .collect()
}

fn remote_versions(config: &FnmConfig) -> Result<Vec<node_semver::Version>, Error> {
    let remote = remote_node_index::list(&config.node_dist_mirror)?;
    Ok(remote
        .into_iter()
        .filter_map(|indexed| match indexed.version {
            Version::Semver(semver) => Some(semver),
            _ => None,
        })
        .collect())
}

fn build_candidates(
    installed: &[node_semver::Version],
    remote: &[node_semver::Version],
    aliases: &BTreeMap<String, Vec<String>>,
) -> Vec<Candidate> {
    let mut tracks: BTreeMap<Track, Vec<node_semver::Version>> = BTreeMap::new();
    for version in installed {
        tracks
            .entry(Track::of(version))
            .or_default()
            .push(version.clone());
    }

    let mut candidates = Vec::with_capacity(tracks.len());
    for (track, mut versions) in tracks {
        versions.sort();
        let mut newest = versions.split_off(versions.len() - 1);
        let version = newest
            .pop()
            .expect("a track always has at least one installed version");

        candidates.push(Candidate {
            latest: remote
                .iter()
                .filter(|remote_version| track.contains(remote_version))
                .max()
                .cloned(),
            aliases: aliases
                .get(&display_version(&version))
                .cloned()
                .unwrap_or_default(),
            stable_notice: stable_notice(&version, remote),
            installed: version,
            left_unchanged: versions,
        });
    }

    candidates.sort_by(|a, b| a.installed.cmp(&b.installed));
    candidates
}

/// The newest stable release of the same major, when it has reached the
/// prerelease's base version. Prereleases are never moved to stable, only told
/// about it.
fn stable_notice(
    installed: &node_semver::Version,
    remote: &[node_semver::Version],
) -> Option<node_semver::Version> {
    if !installed.is_prerelease() {
        return None;
    }

    let stable = remote
        .iter()
        .filter(|version| version.major == installed.major && !version.is_prerelease())
        .max()?;

    let base = node_semver::Version {
        major: installed.major,
        minor: installed.minor,
        patch: installed.patch,
        pre_release: Vec::new(),
        build: Vec::new(),
    };

    (stable >= &base).then(|| stable.clone())
}

fn display_version(version: &node_semver::Version) -> String {
    format!("v{version}")
}

fn print_table(config: &FnmConfig, candidates: &[Candidate]) {
    let rows: Vec<Row> = candidates.iter().map(Row::of).collect();

    let version_width = column_width("Version", rows.iter().map(|row| row.version.as_str()));
    let latest_width = column_width("Latest", rows.iter().map(|row| row.latest.as_str()));
    let aliases_width = column_width("Aliases", rows.iter().map(|row| row.aliases.as_str()));

    outln!(
        config,
        Info,
        "{:<version_width$}  {:<latest_width$}  {:<aliases_width$}  {}",
        "Version".bold(),
        "Latest".bold(),
        "Aliases".bold(),
        "Status".bold()
    );

    for row in rows {
        outln!(
            config,
            Info,
            "{:<version_width$}  {:<latest_width$}  {:<aliases_width$}  {}",
            row.version,
            row.latest,
            row.aliases.dimmed(),
            row.status
        );
    }

    let left_unchanged: Vec<_> = candidates
        .iter()
        .flat_map(|candidate| candidate.left_unchanged.iter().map(display_version))
        .collect();

    if !left_unchanged.is_empty() {
        outln!(
            config,
            Info,
            "{} also installed on the same track, left unchanged: {}",
            "note:".cyan(),
            left_unchanged.join(", ").dimmed()
        );
    }
}

struct Row {
    version: String,
    latest: String,
    aliases: String,
    status: String,
}

impl Row {
    fn of(candidate: &Candidate) -> Self {
        let version = display_version(&candidate.installed);
        let latest = candidate
            .latest
            .as_ref()
            .map_or_else(|| "-".to_string(), display_version);
        let aliases = if candidate.aliases.is_empty() {
            "-".to_string()
        } else {
            candidate.aliases.join(", ")
        };

        let status = if candidate.update().is_some() {
            "update available".yellow()
        } else if candidate.latest.is_some() {
            "up to date".green()
        } else {
            "no matching release found".dimmed()
        };

        let stable_notice = candidate
            .stable_notice
            .as_ref()
            .map_or_else(String::new, |stable| {
                let notice = format!("(stable {} released)", display_version(stable));
                format!(" {}", notice.cyan())
            });

        Self {
            version,
            latest,
            aliases,
            status: format!("{status}{stable_notice}"),
        }
    }
}

fn column_width<'a, I: Iterator<Item = &'a str>>(header: &str, cells: I) -> usize {
    cells.map(str::len).max().unwrap_or(0).max(header.len())
}

fn current_semver(config: &FnmConfig) -> Option<node_semver::Version> {
    match current_version(config).ok().flatten() {
        Some(Version::Semver(version)) => Some(version),
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    VersionListing {
        #[from]
        source: installed_versions::Error,
    },
    #[error(transparent)]
    RemoteListing {
        #[from]
        source: remote_node_index::Error,
    },
    #[error("Can't read aliases: {source}")]
    AliasReading { source: std::io::Error },
    #[error("Can't find an installed Node.js version matching {version}")]
    VersionNotInstalled { version: UserVersion },
    #[error("{} version(s) could not be updated. See the errors above.", count)]
    UpdatesFailed { count: usize },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn parse(version: &str) -> node_semver::Version {
        match Version::parse(version).expect("Can't parse version") {
            Version::Semver(semver) => semver,
            other => panic!("Expected a semver version, got {other}"),
        }
    }
    fn candidates(installed: &[&str], remote_versions: &[&str]) -> Vec<Candidate> {
        let installed: Vec<_> = installed.iter().map(|version| parse(version)).collect();
        let remote: Vec<_> = remote_versions
            .iter()
            .map(|version| parse(version))
            .collect();
        build_candidates(&installed, &remote, &BTreeMap::new())
    }

    #[test]
    fn picks_latest_stable_of_the_same_major() {
        let candidates = candidates(
            &["20.11.0"],
            &["18.20.8", "20.11.0", "20.19.0", "20.20.0-rc.1", "22.1.0"],
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].latest, Some(parse("20.19.0")));
        assert_eq!(candidates[0].update(), Some(&parse("20.19.0")));
    }

    #[test]
    fn prerelease_updates_within_its_own_channel() {
        let candidates = candidates(
            &["22.0.0-rc.1"],
            &[
                "22.0.0-beta.9",
                "22.0.0-rc.2",
                "22.0.0-rc.10",
                "22.0.0",
                "22.1.0",
            ],
        );

        assert_eq!(candidates[0].latest, Some(parse("22.0.0-rc.10")));
        assert_eq!(candidates[0].update(), Some(&parse("22.0.0-rc.10")));
        // The notice always points at the newest stable of the major line.
        assert_eq!(candidates[0].stable_notice, Some(parse("22.1.0")));
    }

    #[test]
    fn never_reports_the_stable_release_as_an_update_target() {
        let candidates = candidates(&["22.0.0-rc.1"], &["22.0.0-rc.1", "22.0.0"]);

        assert_eq!(candidates[0].update(), None);
        assert_eq!(candidates[0].stable_notice, Some(parse("22.0.0")));
    }

    #[test]
    fn stable_notice_requires_the_stable_release_to_reach_the_base_version() {
        let candidates = candidates(
            &["27.0.0-alpha.0"],
            &["26.9.0", "27.0.0-alpha.2", "26.10.0"],
        );

        assert_eq!(candidates[0].stable_notice, None);
    }

    #[test]
    fn stable_installs_have_no_stable_notice() {
        let candidates = candidates(&["20.11.0"], &["20.11.0", "20.19.0"]);

        assert_eq!(candidates[0].stable_notice, None);
    }

    #[test]
    fn groups_installed_versions_per_track_and_keeps_the_newest() {
        let candidates = candidates(
            &["20.9.0", "20.11.0", "22.0.0-rc.1", "22.0.0-rc.2"],
            &["20.9.0", "20.11.0", "20.19.0", "22.0.0-rc.2", "22.0.0-rc.3"],
        );

        let installed: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.installed.clone())
            .collect();

        assert_eq!(installed, vec![parse("20.11.0"), parse("22.0.0-rc.2")]);
        assert_eq!(candidates[0].left_unchanged, vec![parse("20.9.0")]);
        assert_eq!(candidates[1].left_unchanged, vec![parse("22.0.0-rc.1")]);
    }

    #[test]
    fn no_update_when_the_installed_version_is_not_in_the_remote_index() {
        let candidates = candidates(&["20.11.0"], &["22.1.0"]);

        assert_eq!(candidates[0].latest, None);
        assert_eq!(candidates[0].update(), None);
    }

    #[test]
    fn no_update_when_the_installed_version_is_newer_than_the_mirror() {
        let candidates = candidates(&["20.19.0"], &["20.11.0"]);

        assert_eq!(candidates[0].latest, Some(parse("20.11.0")));
        assert_eq!(candidates[0].update(), None);
    }

    #[test]
    fn filters_narrow_the_installed_versions() {
        let config = FnmConfig::default();
        let installed = vec![
            Version::parse("20.11.0").unwrap(),
            Version::parse("22.1.0").unwrap(),
        ];

        let filtered = filter_installed(
            &installed,
            &[UserVersion::OnlyMajor(20)],
            &BTreeMap::new(),
            &config,
        )
        .expect("Can filter installed versions");

        assert_eq!(filtered, vec![Version::parse("20.11.0").unwrap()]);
    }

    #[test]
    fn errors_when_a_filter_matches_nothing() {
        let config = FnmConfig::default();
        let installed = vec![Version::parse("20.11.0").unwrap()];

        let error = filter_installed(
            &installed,
            &[UserVersion::OnlyMajor(22)],
            &BTreeMap::new(),
            &config,
        )
        .expect_err("Expected a missing version error");

        assert_eq!(
            format!("{error}"),
            "Can't find an installed Node.js version matching v22.x.x"
        );
    }

    #[test]
    fn resolves_alias_filters_from_the_read_only_snapshot() {
        let config = FnmConfig::default();
        let installed = vec![
            Version::parse("20.11.0").unwrap(),
            Version::parse("22.1.0").unwrap(),
        ];
        let mut aliases = BTreeMap::new();
        aliases.insert(
            "v20.11.0".to_string(),
            vec!["snapshot-only-alias".to_string()],
        );
        let filter = UserVersion::Full(Version::Alias("snapshot-only-alias".to_string()));

        let filtered = filter_installed(&installed, &[filter], &aliases, &config)
            .expect("Can filter installed versions by alias");

        assert_eq!(filtered, vec![Version::parse("20.11.0").unwrap()]);
    }

    #[test]
    fn tracks_are_keyed_by_major_and_channel() {
        assert_eq!(Track::of(&parse("20.11.0")).channel, None);
        assert_eq!(
            Track::of(&parse("22.0.0-rc.1")).channel.as_deref(),
            Some("rc")
        );
        assert_eq!(
            Track::of(&parse("27.0.0-alpha.0.0.0")).channel.as_deref(),
            Some("alpha")
        );
    }
}

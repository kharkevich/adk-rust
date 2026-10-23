//! The `[kernel]` table of `harness.toml` (spec 3.2 §8.2): the user config sets how the
//! resident kernel runs; a workspace's `.adk/harness.toml` may only restrict it, as spec
//! 3.1 I3 requires of workspace files.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use adk_core::{AdkError, Result};
use serde::Deserialize;

use super::args::PROJECT_CONFIG_FILE;

/// How `adk-rust up` runs the kernel, from the `[kernel]` tables (spec 3.2 §8.2).
///
/// | Key | User config | Workspace `.adk/harness.toml` |
/// |---|---|---|
/// | `home` | The kernel home: its socket and `harness.db` | Ignored |
/// | `bind` | The HTTP control plane's `HOST:PORT` | Ignored |
/// | `max_concurrent_runs` | Most runs each queue's worker executes at once | Applies only when lower |
/// | `[[kernel.partitions]]`, `coordinator` | The partitions `up` runs, one harness each, and the one whose harness may cross into the others (spec 3.2 §7.8) | Ignored |
///
/// A `[[kernel.partitions]]` entry has `id`, and optionally `class` (default the id),
/// `tenant`, `mode` (`read_only`, `plan`, `ask`, `auto`; default `ask`), `memory_scope`
/// (default the id), `drivers` and `cross_partition` (`deny`, the default, or
/// `coordinator_with_approval`).
///
/// The drivers directory, proactivity policy and quiet hours of the table arrive with their
/// phases; until then they are reported as unknown keys. Flags win over both files.
///
/// # Example
///
/// ```rust
/// use adk_cli::harness::KernelSettings;
///
/// let dir = tempfile::tempdir().unwrap();
/// let user = dir.path().join("harness.toml");
/// std::fs::write(&user, "[kernel]\nbind = \"127.0.0.1:9090\"\nmax_concurrent_runs = 8\n").unwrap();
/// std::fs::create_dir(dir.path().join(".adk")).unwrap();
/// std::fs::write(dir.path().join(".adk/harness.toml"), "[kernel]\nmax_concurrent_runs = 2\nbind = \"0.0.0.0:80\"\n").unwrap();
///
/// let settings = KernelSettings::resolve(Some(&user), dir.path()).unwrap();
/// assert_eq!(settings.bind, Some("127.0.0.1:9090".parse().unwrap()));
/// assert_eq!(settings.max_concurrent_runs, Some(2));
/// assert!(settings.notes.iter().any(|note| note.contains("kernel.bind")));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct KernelSettings {
    /// The kernel home; `None` keeps `<config dir>/adk-rust`.
    pub home: Option<PathBuf>,
    /// The HTTP bind of `adk-rust up`; `None` keeps `127.0.0.1:8080`.
    pub bind: Option<SocketAddr>,
    /// Most runs each queue's worker executes at once; `None` keeps the worker default.
    pub max_concurrent_runs: Option<usize>,
    /// The partitions `up` runs, one harness each, from `[[kernel.partitions]]` in the user
    /// config (spec 3.2 §7.8); empty runs one harness, partitioned by nothing.
    pub partitions: Vec<adk_kernel::Partition>,
    /// The partition whose harness is the coordinator, from `kernel.coordinator`.
    pub coordinator: Option<adk_kernel::PartitionId>,
    /// Settings a file made that were ignored, and why.
    pub notes: Vec<String>,
}

/// One `[[kernel.partitions]]` entry as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartitionTable {
    id: String,
    class: Option<String>,
    tenant: Option<String>,
    mode: Option<String>,
    memory_scope: Option<String>,
    #[serde(default)]
    drivers: Vec<String>,
    cross_partition: Option<String>,
}

impl PartitionTable {
    fn partition(self, origin: &str) -> Result<adk_kernel::Partition> {
        let invalid = |error: adk_kernel::KernelError| {
            AdkError::config(format!("{origin}: partition `{}`: {error}", self.id))
        };
        let class = self.class.clone().unwrap_or_else(|| self.id.clone());
        let mut partition = adk_kernel::Partition::new(self.id.as_str(), class);
        if let Some(tenant) = &self.tenant {
            partition = partition.with_tenant(tenant.clone());
        }
        if let Some(mode) = &self.mode {
            partition = partition.with_mode(mode.parse().map_err(invalid)?);
        }
        if let Some(scope) = &self.memory_scope {
            partition = partition.with_memory_scope(scope.clone());
        }
        for driver in &self.drivers {
            partition = partition.with_driver(driver.clone());
        }
        if let Some(cross) = &self.cross_partition {
            partition = partition.with_cross_partition(cross.parse().map_err(invalid)?);
        }
        partition.validate().map_err(invalid)?;
        Ok(partition)
    }
}

/// The `[kernel]` table as written.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct KernelTable {
    home: Option<PathBuf>,
    bind: Option<SocketAddr>,
    max_concurrent_runs: Option<usize>,
    partitions: Option<Vec<PartitionTable>>,
    coordinator: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

impl KernelTable {
    /// The `[kernel]` table of the file at `path`, if the file and the table exist.
    fn load(path: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(AdkError::config(format!(
                    "failed to read {}: {error}",
                    path.display()
                )));
            }
        };
        let invalid = |error: &dyn std::fmt::Display| {
            AdkError::config(format!("{}: invalid [kernel] table: {error}", path.display()))
        };
        let file: toml::Table = toml::from_str(&text).map_err(|error| invalid(&error))?;
        match file.get("kernel") {
            Some(table) => table.clone().try_into().map(Some).map_err(|error| invalid(&error)),
            None => Ok(None),
        }
    }
}

impl KernelSettings {
    /// Resolves the `[kernel]` table of the user config at `user_config` (normally
    /// `<config dir>/adk-rust/harness.toml`) against the one in `workspace`'s
    /// `.adk/harness.toml`, which may only lower `max_concurrent_runs` (spec 3.2 §8.2).
    ///
    /// # Errors
    ///
    /// Returns a configuration error when a file cannot be read, its `[kernel]` table does
    /// not parse, or `max_concurrent_runs` is 0.
    pub fn resolve(user_config: Option<&Path>, workspace: &Path) -> Result<Self> {
        let mut settings = Self::default();
        if let Some(path) = user_config
            && let Some(user) = KernelTable::load(path)?
        {
            let origin = path.display().to_string();
            settings.home = user.home;
            settings.bind = user.bind;
            settings.max_concurrent_runs = positive(&origin, user.max_concurrent_runs)?;
            for table in user.partitions.unwrap_or_default() {
                let partition = table.partition(&origin)?;
                if settings.partitions.iter().any(|p| p.id == partition.id) {
                    return Err(AdkError::config(format!(
                        "{origin}: partition `{}` is listed twice under [[kernel.partitions]]",
                        partition.id
                    )));
                }
                settings.partitions.push(partition);
            }
            if let Some(coordinator) = user.coordinator {
                let coordinator = adk_kernel::PartitionId::new(coordinator);
                if !settings.partitions.iter().any(|partition| partition.id == coordinator) {
                    return Err(AdkError::config(format!(
                        "{origin}: `kernel.coordinator` names `{coordinator}`, which no \
                         [[kernel.partitions]] entry defines"
                    )));
                }
                settings.coordinator = Some(coordinator);
            }
            for key in user.unknown.keys() {
                settings.notes.push(format!("{origin}: unknown key `kernel.{key}` ignored"));
            }
        }
        if let Some(project) = KernelTable::load(&workspace.join(PROJECT_CONFIG_FILE))? {
            let origin = PROJECT_CONFIG_FILE;
            for (key, set) in [
                ("home", project.home.is_some()),
                ("bind", project.bind.is_some()),
                ("partitions", project.partitions.is_some()),
                ("coordinator", project.coordinator.is_some()),
            ] {
                if set {
                    settings.notes.push(format!(
                        "{origin}: `kernel.{key}` ignored — where the kernel keeps its state, \
                         listens and draws its partitions is yours to choose; set it in your \
                         user config"
                    ));
                }
            }
            if let Some(requested) = positive(origin, project.max_concurrent_runs)? {
                match settings.max_concurrent_runs {
                    Some(current) if requested >= current => {
                        if requested > current {
                            settings.notes.push(format!(
                                "{origin}: `kernel.max_concurrent_runs` ignored — {requested} is \
                                 above the {current} your user config allows"
                            ));
                        }
                    }
                    _ => settings.max_concurrent_runs = Some(requested),
                }
            }
            for key in project.unknown.keys() {
                settings.notes.push(format!("{origin}: unknown key `kernel.{key}` ignored"));
            }
        }
        Ok(settings)
    }
}

fn positive(origin: &str, value: Option<usize>) -> Result<Option<usize>> {
    match value {
        Some(0) => Err(AdkError::config(format!(
            "{origin}: `kernel.max_concurrent_runs` must be at least 1"
        ))),
        value => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(user: &str, project: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("user.toml");
        std::fs::write(&user_path, user).unwrap();
        std::fs::create_dir(dir.path().join(".adk")).unwrap();
        std::fs::write(dir.path().join(PROJECT_CONFIG_FILE), project).unwrap();
        (dir, user_path)
    }

    #[test]
    fn the_user_config_sets_the_kernel_and_unknown_keys_are_noted() {
        let (dir, user) = files(
            "mode = \"ask\"\n[kernel]\nhome = \"/srv/kernel\"\nbind = \"127.0.0.1:9000\"\nquiet_hours = \"22-7\"\n",
            "",
        );
        let settings = KernelSettings::resolve(Some(&user), dir.path()).unwrap();
        assert_eq!(settings.home, Some(PathBuf::from("/srv/kernel")));
        assert_eq!(settings.bind, Some("127.0.0.1:9000".parse().unwrap()));
        assert_eq!(settings.max_concurrent_runs, None);
        assert_eq!(settings.notes.len(), 1);
        assert!(settings.notes[0].contains("kernel.quiet_hours"), "{:?}", settings.notes);
    }

    #[test]
    fn a_workspace_may_only_lower_concurrency() {
        let (dir, user) = files(
            "[kernel]\nmax_concurrent_runs = 4\n",
            "[kernel]\nmax_concurrent_runs = 9\nhome = \"/tmp/x\"\n",
        );
        let settings = KernelSettings::resolve(Some(&user), dir.path()).unwrap();
        assert_eq!((settings.max_concurrent_runs, settings.home), (Some(4), None));
        assert!(
            settings.notes.iter().any(|note| note.contains("above the 4")),
            "{:?}",
            settings.notes
        );
        assert!(
            settings.notes.iter().any(|note| note.contains("kernel.home")),
            "{:?}",
            settings.notes
        );

        let (dir, user) = files("", "[kernel]\nmax_concurrent_runs = 1\n");
        let settings = KernelSettings::resolve(Some(&user), dir.path()).unwrap();
        assert_eq!(settings.max_concurrent_runs, Some(1));
        assert!(settings.notes.is_empty());
    }

    #[test]
    fn partitions_come_from_the_user_config_only() {
        let (dir, user) = files(
            "[kernel]\ncoordinator = \"home\"\n\
             [[kernel.partitions]]\nid = \"home\"\ncross_partition = \"coordinator_with_approval\"\n\
             [[kernel.partitions]]\nid = \"work\"\nclass = \"work-agents\"\nmode = \"auto\"\n\
             memory_scope = \"job\"\ndrivers = [\"calendar\"]\ncross_partition = \"coordinator_with_approval\"\n",
            "[kernel]\ncoordinator = \"work\"\n[[kernel.partitions]]\nid = \"evil\"\n",
        );
        let settings = KernelSettings::resolve(Some(&user), dir.path()).unwrap();
        let expected = vec![
            adk_kernel::Partition::new("home", "home")
                .with_cross_partition(adk_kernel::CrossPartition::CoordinatorWithApproval),
            adk_kernel::Partition::new("work", "work-agents")
                .with_mode(adk_kernel::PartitionMode::Auto)
                .with_memory_scope("job")
                .with_driver("calendar")
                .with_cross_partition(adk_kernel::CrossPartition::CoordinatorWithApproval),
        ];
        assert_eq!(settings.partitions, expected);
        assert_eq!(settings.coordinator, Some("home".into()));
        assert!(settings.notes.iter().any(|note| note.contains("kernel.partitions")), "{:?}", settings.notes);
        assert!(settings.notes.iter().any(|note| note.contains("kernel.coordinator")), "{:?}", settings.notes);
    }

    #[test]
    fn a_bad_partition_table_is_refused() {
        for user in [
            "[kernel]\ncoordinator = \"home\"\n",
            "[[kernel.partitions]]\nid = \"home\"\n[[kernel.partitions]]\nid = \"home\"\n",
            "[[kernel.partitions]]\nid = \"home\"\nmode = \"sometimes\"\n",
            "[[kernel.partitions]]\nid = \"home\"\ncross_partition = \"maybe\"\n",
            "[[kernel.partitions]]\nid = \"\"\n",
            "[[kernel.partitions]]\nid = \"home\"\ncolour = \"blue\"\n",
        ] {
            let (dir, user_path) = files(user, "");
            assert!(KernelSettings::resolve(Some(&user_path), dir.path()).is_err(), "{user}");
        }
    }

    #[test]
    fn zero_concurrency_and_bad_tables_are_refused() {
        let (dir, user) = files("[kernel]\nmax_concurrent_runs = 0\n", "");
        assert!(KernelSettings::resolve(Some(&user), dir.path()).is_err());
        let (dir, user) = files("[kernel]\nbind = \"not an address\"\n", "");
        let error = KernelSettings::resolve(Some(&user), dir.path()).unwrap_err().to_string();
        assert!(error.contains("[kernel]"), "{error}");
    }
}

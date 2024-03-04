use crate::error;
use crate::error::Result;
use bottlerocket_release::BottlerocketRelease;
use chrono::{DateTime, Utc};
use modeled_types::FriendlyVersion;
use serde::{Deserialize, Serialize};
use signpost::State;
use snafu::{OptionExt, ResultExt};
use std::convert::TryInto;
use std::fs;
use std::fs::File;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Output;

pub const UPDATE_LOCKFILE: &str = "/run/lock/thar-be-updates.lock";
pub const UPDATE_STATUS_FILE: &str = "/run/cache/thar-be-updates/status.json";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct UpdatesSettings {
    // Version to update to when updating via the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version_lock: Option<FriendlyVersion>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum UpdateState {
    Idle,
    Available,
    Staged,
    Ready,
}

/// UpdateImage represents a Bottlerocket update image
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpdateImage {
    arch: String,
    version: semver::Version,
    variant: String,
}

impl UpdateImage {
    pub fn version(&self) -> &semver::Version {
        &self.version
    }
}

/// StagedImage represents a Bottlerocket image that is written to a partition set
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StagedImage {
    image: UpdateImage,
    /// Indicates whether this image is marked for next boot
    next_to_boot: bool,
}

impl StagedImage {
    pub(crate) fn set_next_to_boot(&mut self, next_to_boot: bool) {
        self.next_to_boot = next_to_boot
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum CommandStatus {
    Success,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateCommand {
    Refresh,
    Prepare,
    Activate,
    Deactivate,
}

/// CommandResult represents the result of an issued command
#[derive(Debug, Clone, Deserialize, Serialize)]
struct CommandResult {
    cmd_type: UpdateCommand,
    cmd_status: CommandStatus,
    timestamp: DateTime<Utc>,
    exit_status: Option<i32>,
    stderr: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UpdateStatus {
    update_state: UpdateState,
    available_updates: Vec<semver::Version>,
    chosen_update: Option<UpdateImage>,
    active_partition: Option<StagedImage>,
    staging_partition: Option<StagedImage>,
    most_recent_command: Option<CommandResult>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// Loads and returns the update status from disk.
/// This takes the update lock file as an parameter to signal to caller that the update
/// lock needs to be obtained before calling this.
pub fn get_update_status(_lockfile: &File) -> Result<UpdateStatus> {
    let status_file = File::open(UPDATE_STATUS_FILE).context(error::NoStatusFileSnafu {
        path: UPDATE_STATUS_FILE,
    })?;
    serde_json::from_reader(status_file).context(error::StatusParseSnafu {
        path: UPDATE_STATUS_FILE,
    })
}

/// Retrieves settings from the configuration file.
///
fn get_settings<P>(config_path: P) -> Result<UpdatesSettings>
where
    P: AsRef<Path>,
{
    let config_path = config_path.as_ref();
    let config_str = fs::read_to_string(config_path).context(error::ReadConfigSnafu {
        path: config_path.to_path_buf(),
    })?;
    toml::from_str(&config_str).context(error::DeserializationSnafu {
        path: config_path.to_path_buf(),
    })
}

// This is how the UpdateStatus is stored on disk
impl UpdateStatus {
    /// Initializes the update status
    pub fn new() -> Self {
        Self {
            update_state: UpdateState::Idle,
            available_updates: vec![],
            chosen_update: None,
            active_partition: None,
            staging_partition: None,
            most_recent_command: None,
        }
    }

    pub fn update_state(&self) -> &UpdateState {
        &self.update_state
    }

    pub fn set_update_state(&mut self, state: UpdateState) {
        self.update_state = state;
    }

    pub fn chosen_update(&self) -> Option<&UpdateImage> {
        match &self.chosen_update {
            Some(update) => Some(update),
            None => None,
        }
    }

    pub fn staging_partition(&self) -> Option<&StagedImage> {
        match &self.staging_partition {
            Some(partition_info) => Some(partition_info),
            None => None,
        }
    }

    /// Updates the active partition set information
    pub fn update_active_partition_info(&mut self) -> Result<()> {
        // Get current OS release info to determine active partition image information
        let os_info = BottlerocketRelease::new().context(error::ReleaseVersionSnafu)?;
        let active_image = UpdateImage {
            arch: os_info.arch,
            version: os_info.version_id,
            variant: os_info.variant_id,
        };

        // Get partition set information. We can infer the version of the image in the active
        // partition set by checking the os release information
        let gpt_state = State::load().context(error::PartitionTableReadSnafu)?;
        let active_set = gpt_state.active();
        let next_set = gpt_state.next().context(error::NoneSetToBootSnafu)?;
        self.active_partition = Some(StagedImage {
            image: active_image,
            next_to_boot: active_set == next_set,
        });
        Ok(())
    }

    /// Sets the staging partition image information
    pub fn set_staging_partition_image_info(&mut self, image: UpdateImage) {
        self.staging_partition = Some(StagedImage {
            image,
            next_to_boot: false,
        });
    }

    /// Mark staging partition as next to boot
    pub fn mark_staging_partition_next_to_boot(&mut self) -> Result<()> {
        if let Some(staging_partition) = &mut self.staging_partition {
            staging_partition.set_next_to_boot(true);
        } else {
            return error::StagingPartitionSnafu {}.fail();
        }
        if let Some(active_partition) = &mut self.active_partition {
            active_partition.set_next_to_boot(false);
        } else {
            return error::ActivePartitionSnafu {}.fail();
        }
        Ok(())
    }

    /// Unmark staging partition as next to boot
    pub fn unmark_staging_partition_next_to_boot(&mut self) -> Result<()> {
        if let Some(staging_partition) = &mut self.staging_partition {
            staging_partition.set_next_to_boot(false);
        } else {
            return error::StagingPartitionSnafu {}.fail();
        }
        if let Some(active_partition) = &mut self.active_partition {
            active_partition.set_next_to_boot(true);
        } else {
            return error::ActivePartitionSnafu {}.fail();
        }
        Ok(())
    }

    /// Sets information regarding the latest command invocation
    /// Derive success/failure status from exit status when possible.
    pub fn set_recent_command_info(&mut self, cmd_type: UpdateCommand, cmd_output: &Output) {
        let exit_status = match cmd_output.status.code() {
            Some(code) => code,
            None => cmd_output.status.signal().unwrap_or(1),
        };
        let command_result = CommandResult {
            cmd_type,
            cmd_status: if exit_status == 0 {
                CommandStatus::Success
            } else {
                CommandStatus::Failed
            },
            timestamp: Utc::now(),
            exit_status: Some(exit_status),
            stderr: Some(String::from_utf8_lossy(&cmd_output.stderr).to_string()),
        };
        self.most_recent_command = Some(command_result);
    }

    /// Returns the update information of the 'latest' available update
    pub fn get_latest_update(
        updates: Vec<update_metadata::Update>,
    ) -> Result<Option<update_metadata::Update>> {
        let os_info = BottlerocketRelease::new().context(error::ReleaseVersionSnafu)?;
        for update in updates {
            // If the current running version is greater than the max version ever published,
            // or moves us to a valid version <= the maximum version, update.
            // Updates are listed in descending order (in terms of versions) in the manifest,
            // so the first picked out would be the latest update available.
            if os_info.version_id < update.version || os_info.version_id > update.max_version {
                return Ok(Some(update));
            }
        }
        Ok(None)
    }

    /// Checks the list of updates to for an available update.
    /// If the 'version-lock'ed version is available returns true. Otherwise returns false
    pub fn update_available_updates<P>(
        &mut self,
        config_path: P,
        updates: Vec<update_metadata::Update>,
    ) -> Result<bool>
    where
        P: AsRef<Path>,
    {
        // Extract the version to store
        self.available_updates = updates.iter().map(|u| u.version.to_owned()).collect();
        // Check if the 'version-lock'ed update is available as the 'chosen' update
        // Retrieve the 'version-lock' setting
        let settings = get_settings(config_path)?;
        let locked_version: FriendlyVersion =
            settings
                .version_lock
                .context(error::GetSettingOptionSnafu {
                    setting: "version-lock",
                })?;

        if locked_version == "latest" {
            // Set chosen_update to the latest version available
            if let Some(latest_update) = UpdateStatus::get_latest_update(updates)? {
                self.chosen_update = Some(UpdateImage {
                    arch: latest_update.arch,
                    version: latest_update.version,
                    variant: latest_update.variant,
                });
                return Ok(true);
            }
        } else {
            let chosen_version = FriendlyVersion::try_into(locked_version.to_owned()).context(
                error::SemVerSnafu {
                    version: locked_version,
                },
            )?;
            let os_info = BottlerocketRelease::new().context(error::ReleaseVersionSnafu)?;
            if chosen_version != os_info.version_id {
                for update in &updates {
                    if update.version == chosen_version {
                        self.chosen_update = Some(UpdateImage {
                            arch: update.arch.clone(),
                            version: chosen_version,
                            variant: update.variant.clone(),
                        });
                        return Ok(true);
                    }
                }
            }
        }
        // 'version-lock'ed update is unavailable.
        self.chosen_update = None;
        Ok(false)
    }
}

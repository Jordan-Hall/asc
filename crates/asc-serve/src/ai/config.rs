//! `[aikeys]` of `asc-serve.toml`.

use std::path::PathBuf;

use serde::Deserialize;

/// `[aikeys]`: bring-your-own AI keys. Safe when absent.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiKeysConfig {
    /// Keys one owner may hold.
    pub max_keys_per_owner: u32,
    /// Monthly spending cap of the whole instance in micro-USD (0: none). It applies on top of
    /// each key's own cap.
    pub instance_monthly_cap_micro_usd: u64,
    /// The price table: a JSON object from a model pattern to `[input, output]` micro-USD per
    /// million tokens. Without it no model is priced, so a cost limit refuses every call.
    pub price_file: Option<PathBuf>,
    /// Longest a single model call may take, in seconds.
    pub completion_timeout_secs: u64,
    /// Tests of one key per hour.
    pub test_per_key_per_hour: u32,
}

impl Default for AiKeysConfig {
    fn default() -> Self {
        Self {
            max_keys_per_owner: 32,
            instance_monthly_cap_micro_usd: 0,
            price_file: None,
            completion_timeout_secs: 300,
            test_per_key_per_hour: 6,
        }
    }
}

impl AiKeysConfig {
    /// Checks the bounds; the message names the key.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_keys_per_owner == 0 || self.max_keys_per_owner > 1024 {
            return Err("aikeys.max_keys_per_owner must be 1 to 1024".into());
        }
        if self.completion_timeout_secs == 0 || self.completion_timeout_secs > 3600 {
            return Err("aikeys.completion_timeout_secs must be 1 to 3600".into());
        }
        if self.test_per_key_per_hour == 0 || self.test_per_key_per_hour > 600 {
            return Err("aikeys.test_per_key_per_hour must be 1 to 600".into());
        }
        if self.instance_monthly_cap_micro_usd > u64::try_from(i64::MAX).unwrap_or(u64::MAX) {
            return Err("aikeys.instance_monthly_cap_micro_usd is too large".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_unknown_keys_are_refused() {
        assert!(AiKeysConfig::default().validate().is_ok());
        assert!(toml::from_str::<AiKeysConfig>("bogus = 1").is_err());
        let c: AiKeysConfig =
            toml::from_str("max_keys_per_owner = 5\nprice_file = \"/p.json\"").unwrap();
        assert_eq!(c.max_keys_per_owner, 5);
        assert!(
            AiKeysConfig {
                max_keys_per_owner: 0,
                ..AiKeysConfig::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            AiKeysConfig {
                completion_timeout_secs: 0,
                ..AiKeysConfig::default()
            }
            .validate()
            .is_err()
        );
    }
}

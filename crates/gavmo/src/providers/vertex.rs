//! Google Cloud Vertex AI, authenticated with the user's Application
//! Default Credentials (`gcloud auth application-default login`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{FieldSpec, HostContext, ModelSpec, Provider, Values};
use crate::bifrost::ProviderBlock;
use crate::types::ProviderCheck;

pub(crate) struct Vertex;

/// The ADC file `gcloud auth application-default login` writes.
const ADC_FILE: &str = "application_default_credentials.json";

static FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "project",
        label: "Google Cloud project",
        help: "The project Vertex AI requests run (and are billed) in. Its Model Garden must \
               have the Claude models enabled.",
        placeholder: "my-project-123",
        secret: false,
        required: true,
        default: None,
        env: Some("GAVMO_GCP_PROJECT"),
    },
    FieldSpec {
        key: "region",
        label: "Region",
        help: "The Vertex AI region to call; \"global\" lets Google pick one with capacity.",
        placeholder: "global",
        secret: false,
        required: true,
        default: Some("global"),
        env: Some("GAVMO_GCP_REGION"),
    },
];

/// The built-in models, as a models.dev section (from its entries for them).
fn fallback_section() -> serde_json::Value {
    let claude = |name: &str, family: &str, released: &str, cost: [f64; 4]| {
        serde_json::json!({
            "name": name,
            "family": family,
            "attachment": true,
            "reasoning": true,
            "tool_call": true,
            "temperature": false,
            "release_date": released,
            "modalities": { "input": ["text", "image", "pdf"], "output": ["text"] },
            "limit": { "context": 1_000_000, "output": 128_000 },
            "cost": {
                "input": cost[0],
                "output": cost[1],
                "cache_read": cost[2],
                "cache_write": cost[3],
            },
        })
    };
    serde_json::json!({ "models": {
        "claude-haiku-5-5@default":
            claude("Claude Haiku 5.5", "claude-haiku", "2026-10-07", [0.1, 0.5, 0.01, 0.125]),
        "claude-sonnet-5-5@default":
            claude("Claude Sonnet 5.5", "claude-sonnet", "2026-09-28", [2.0, 10.0, 0.1, 2.5]),
        "claude-opus-5-5@default":
            claude("Claude Opus 5.5", "claude-opus", "2026-09-22", [4.0, 20.0, 0.2, 5.0]),
    }})
}

impl Provider for Vertex {
    fn kind(&self) -> &'static str {
        "vertex"
    }

    fn name(&self) -> &'static str {
        "Google Vertex AI"
    }

    fn description(&self) -> &'static str {
        "Claude models through your Google Cloud project, using your gcloud Application \
         Default Credentials. The credentials stay on this machine."
    }

    fn fields(&self) -> &'static [FieldSpec] {
        FIELDS
    }

    fn models_dev_id(&self) -> &'static str {
        "google-vertex-anthropic"
    }

    fn upstream(&self, id: &str) -> String {
        // Bifrost's Vertex provider takes Vertex's own model ids, which
        // models.dev uses (`claude-sonnet-5-5@default`).
        format!("vertex/{id}")
    }

    fn fallback_models(&self) -> Vec<ModelSpec> {
        super::models_dev::models(self, &fallback_section())
    }

    fn suggested_families(&self) -> (&'static str, &'static str) {
        ("claude-sonnet", "claude-haiku")
    }

    fn validate(&self, values: &Values) -> Result<(), String> {
        let field = |key: &str| values.get(key).map_or("", String::as_str);
        validate_project(field("project"))?;
        validate_region(field("region"))
    }

    fn bifrost(
        &self,
        values: &Values,
        _secrets: &Values,
        host: HostContext<'_>,
    ) -> Result<ProviderBlock, String> {
        self.validate(values)?;
        let credentials = adc_path(host.gcloud_dir);
        if !credentials.is_file() {
            return Err(format!(
                "{} not found; run `{}`",
                credentials.display(),
                login_command(host.gcloud_dir)
            ));
        }
        let field = |key: &str| values.get(key).map_or("", String::as_str);
        Ok(block(field("project"), field("region"), &credentials))
    }

    fn checks(&self, host: HostContext<'_>) -> Vec<ProviderCheck> {
        let credentials = adc_path(host.gcloud_dir);
        let check = if credentials.is_file() {
            ProviderCheck {
                ok: true,
                message: format!(
                    "Application Default Credentials found at {}",
                    credentials.display()
                ),
                fix: None,
            }
        } else {
            ProviderCheck {
                ok: false,
                message: format!(
                    "No Application Default Credentials at {}. Sign in with gcloud on this \
                     machine (not in a workspace):",
                    credentials.display()
                ),
                fix: Some(login_command(host.gcloud_dir)),
            }
        };
        vec![check]
    }

    fn suggestions(&self, host: HostContext<'_>) -> Values {
        detect_project(host.gcloud_dir)
            .map(|project| Values::from([("project".to_owned(), project)]))
            .unwrap_or_default()
    }
}

/// Bifrost's `providers.vertex` entry: one key using Application Default
/// Credentials from `credentials`.
pub(crate) fn block(project: &str, region: &str, credentials: &Path) -> ProviderBlock {
    ProviderBlock {
        key: "vertex".to_owned(),
        config: serde_json::json!({
            "keys": [{
                "name": "vertex-adc",
                "value": "",
                "models": ["*"],
                "weight": 1.0,
                "vertex_key_config": {
                    "project_id": project,
                    "region": region,
                    // Empty: Application Default Credentials, i.e. the file
                    // named by GOOGLE_APPLICATION_CREDENTIALS.
                    "auth_credentials": "",
                },
            }],
        }),
        env: vec![(
            "GOOGLE_APPLICATION_CREDENTIALS".to_owned(),
            OsString::from(credentials),
        )],
    }
}

fn adc_path(gcloud_dir: &Path) -> PathBuf {
    gcloud_dir.join(ADC_FILE)
}

/// The command that writes the ADC file GAVMO reads: plain `gcloud` for
/// gcloud's default config dir, pointed at `gcloud_dir` otherwise.
fn login_command(gcloud_dir: &Path) -> String {
    let default = dirs::config_dir().map(|d| d.join("gcloud"));
    if default.as_deref() == Some(gcloud_dir) {
        "gcloud auth application-default login".to_owned()
    } else {
        format!(
            "CLOUDSDK_CONFIG='{}' gcloud auth application-default login",
            gcloud_dir.display()
        )
    }
}

/// Project ids are 6-30 characters of lowercase letters, digits and
/// hyphens, starting with a letter and not ending in a hyphen.
fn validate_project(project: &str) -> Result<(), String> {
    let valid = (6..=30).contains(&project.len())
        && project.starts_with(|c: char| c.is_ascii_lowercase())
        && !project.ends_with('-')
        && project
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{project:?} is not a valid Google Cloud project id (6-30 lowercase letters, \
             digits and hyphens, starting with a letter)"
        ))
    }
}

/// Regions are lowercase letters, digits and hyphens.
fn validate_region(region: &str) -> Result<(), String> {
    let valid = !region.is_empty()
        && region.len() <= 40
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(format!("{region:?} is not a valid Vertex AI region"))
    }
}

/// The user's likely project: the ADC file's quota project, else the active
/// gcloud configuration's `core/project`. Only valid ids are suggested.
fn detect_project(gcloud_dir: &Path) -> Option<String> {
    let from_adc = std::fs::read(adc_path(gcloud_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|adc| adc.get("quota_project_id")?.as_str().map(str::to_owned));
    let valid = |p: &String| validate_project(p).is_ok();
    from_adc
        .filter(valid)
        .or_else(|| gcloud_config_project(gcloud_dir).filter(valid))
}

/// `core/project` from the active gcloud configuration (named in
/// `active_config`, `default` if absent).
fn gcloud_config_project(gcloud_dir: &Path) -> Option<String> {
    let active = std::fs::read_to_string(gcloud_dir.join("active_config"))
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty() && !name.contains(['/', '.']))
        .unwrap_or_else(|| "default".to_owned());
    let config = std::fs::read_to_string(
        gcloud_dir
            .join("configurations")
            .join(format!("config_{active}")),
    )
    .ok()?;
    let mut in_core = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_core = line == "[core]";
        } else if in_core
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "project"
        {
            return Some(value.trim().to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(project: &str, region: &str) -> Values {
        Values::from([
            ("project".to_owned(), project.to_owned()),
            ("region".to_owned(), region.to_owned()),
        ])
    }

    fn host(dir: &Path) -> HostContext<'_> {
        HostContext { gcloud_dir: dir }
    }

    #[test]
    fn projects_and_regions_are_validated() {
        assert!(Vertex.validate(&values("my-project-1", "global")).is_ok());
        assert!(Vertex.validate(&values("my-project-1", "us-east5")).is_ok());
        for bad in [
            "short",
            "1starts-with-digit",
            "ends-with-",
            "Upper-case-1",
            "has_underscore",
            "a\"quote\"x",
            &"x".repeat(31),
        ] {
            assert!(Vertex.validate(&values(bad, "global")).is_err(), "{bad}");
        }
        for bad in ["", "us east", "us\neast5", &"r".repeat(41)] {
            assert!(
                Vertex.validate(&values("my-project-1", bad)).is_err(),
                "{bad:?}"
            );
        }
        assert!(Vertex.validate(&Values::new()).is_err());
    }

    #[test]
    fn the_block_uses_adc_from_the_gcloud_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Vertex
            .bifrost(
                &values("my-project-1", "global"),
                &Values::new(),
                host(tmp.path()),
            )
            .unwrap_err();
        assert!(
            err.contains("application_default_credentials.json not found"),
            "{err}"
        );
        assert!(
            err.contains("gcloud auth application-default login"),
            "{err}"
        );

        std::fs::write(tmp.path().join(ADC_FILE), "{}").unwrap();
        let block = Vertex
            .bifrost(
                &values("my-project-1", "us-east5"),
                &Values::new(),
                host(tmp.path()),
            )
            .unwrap();
        assert_eq!(block.key, "vertex");
        let key = &block.config["keys"][0]["vertex_key_config"];
        assert_eq!(key["project_id"], "my-project-1");
        assert_eq!(key["region"], "us-east5");
        assert_eq!(key["auth_credentials"], "");
        assert_eq!(
            block.env,
            [(
                "GOOGLE_APPLICATION_CREDENTIALS".to_owned(),
                OsString::from(tmp.path().join(ADC_FILE))
            )]
        );

        // Invalid values never reach Bifrost.
        assert!(
            Vertex
                .bifrost(&values("BAD", "global"), &Values::new(), host(tmp.path()))
                .is_err()
        );
    }

    #[test]
    fn the_credentials_check_says_how_to_sign_in() {
        let tmp = tempfile::tempdir().unwrap();
        let [check] = Vertex.checks(host(tmp.path())).try_into().unwrap();
        assert!(!check.ok);
        assert_eq!(
            check.fix.as_deref(),
            Some(
                format!(
                    "CLOUDSDK_CONFIG='{}' gcloud auth application-default login",
                    tmp.path().display()
                )
                .as_str()
            )
        );

        std::fs::write(tmp.path().join(ADC_FILE), "{}").unwrap();
        let [check] = Vertex.checks(host(tmp.path())).try_into().unwrap();
        assert!(check.ok && check.fix.is_none());
        assert!(check.message.contains("found"), "{}", check.message);

        // gcloud's own default dir needs no CLOUDSDK_CONFIG.
        let default = dirs::config_dir().unwrap().join("gcloud");
        assert_eq!(
            login_command(&default),
            "gcloud auth application-default login"
        );
    }

    #[test]
    fn the_project_is_suggested_from_adc_then_gcloud_config() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert!(Vertex.suggestions(host(dir)).is_empty());

        std::fs::create_dir(dir.join("configurations")).unwrap();
        std::fs::write(
            dir.join("configurations/config_default"),
            "[compute]\nproject = not-this-one\n[core]\naccount = me@example.com\nproject = from-config-1\n",
        )
        .unwrap();
        assert_eq!(
            Vertex
                .suggestions(host(dir))
                .get("project")
                .map(String::as_str),
            Some("from-config-1")
        );

        // The active configuration is the one named in `active_config`.
        std::fs::write(dir.join("active_config"), "work\n").unwrap();
        assert!(Vertex.suggestions(host(dir)).is_empty());
        std::fs::write(
            dir.join("configurations/config_work"),
            "[core]\nproject=work-project\n",
        )
        .unwrap();
        assert_eq!(Vertex.suggestions(host(dir))["project"], "work-project");

        // ADC's quota project wins; invalid ids aren't suggested.
        std::fs::write(
            dir.join(ADC_FILE),
            r#"{"quota_project_id":"adc-project-1"}"#,
        )
        .unwrap();
        assert_eq!(Vertex.suggestions(host(dir))["project"], "adc-project-1");
        for unusable in [r#"{"quota_project_id":"Bad"}"#, "not json"] {
            std::fs::write(dir.join(ADC_FILE), unusable).unwrap();
            assert_eq!(Vertex.suggestions(host(dir))["project"], "work-project");
        }
        std::fs::write(
            dir.join("configurations/config_work"),
            "[core]\nproject = Invalid\n",
        )
        .unwrap();
        assert!(Vertex.suggestions(host(dir)).is_empty());
    }

    #[test]
    fn it_describes_itself() {
        assert_eq!(Vertex.kind(), "vertex");
        assert!(Vertex.name().contains("Vertex"));
        assert!(
            Vertex
                .description()
                .contains("Application Default Credentials")
        );
        assert_eq!(Vertex.fields().len(), 2);
        assert_eq!(Vertex.models_dev_id(), "google-vertex-anthropic");
        assert_eq!(
            Vertex.upstream("claude-opus-4-8@default"),
            "vertex/claude-opus-4-8@default"
        );
    }

    #[test]
    fn its_built_in_models_are_the_newest_claudes() {
        let models = Vertex.fallback_models();
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5"]
        );
        let sonnet = &models[1];
        assert_eq!(sonnet.upstream, "vertex/claude-sonnet-5-5@default");
        assert_eq!(sonnet.name, "Claude Sonnet 5.5");
        assert_eq!(sonnet.family.as_deref(), Some("claude-sonnet"));
        assert_eq!(sonnet.details["limit"]["context"], 1_000_000);
        assert_eq!(sonnet.details["tool_call"], true);
        // Each suggested family has a built-in model.
        let (default, small) = Vertex.suggested_families();
        for family in [default, small] {
            assert!(models.iter().any(|m| m.family.as_deref() == Some(family)));
        }
    }
}

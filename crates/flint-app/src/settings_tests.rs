use pretty_assertions::assert_eq;

use super::KeySources;
use super::KeyStatus;
use super::Settings;
use super::display_path;
use flint_agent::ApprovalMode;

#[test]
fn auto_continue_fixes_is_opt_in_and_survives_save_load() {
    assert!(!Settings::default().auto_continue_fixes);
    let old: Settings = toml::from_str("model = \"existing-model\"").unwrap();
    assert!(!old.auto_continue_fixes);
    assert_eq!(old.model, "existing-model");
    let dir = tempfile::tempdir().unwrap();
    for enabled in [true, false] {
        let settings = Settings {
            auto_continue_fixes: enabled,
            ..Settings::default()
        };
        settings.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path(), &KeySources::none()), settings);
    }
}

#[test]
fn displayed_key_status_is_cached_but_agent_credentials_stay_fresh() {
    use std::time::{Duration, Instant};
    let temp = tempfile::tempdir().unwrap();
    let key = temp.path().join("credential");
    std::fs::write(&key, "fixture-key").unwrap();
    let settings = Settings::default();
    let sources = KeySources::none();
    let mut cache = super::KeyStatusCache::default();
    let now = Instant::now();
    assert!(matches!(
        cache.get(&settings, Some(&key), &sources, now),
        KeyStatus::Found(_)
    ));
    std::fs::write(&key, "").unwrap();
    assert!(matches!(
        cache.get(
            &settings,
            Some(&key),
            &sources,
            now + Duration::from_secs(1)
        ),
        KeyStatus::Found(_)
    ));
    assert!(
        settings.resolve_key(Some(&key), &sources).is_none(),
        "Execution must never use cached credentials"
    );
    assert!(matches!(
        cache.get(
            &settings,
            Some(&key),
            &sources,
            now + Duration::from_secs(2)
        ),
        KeyStatus::Missing(_)
    ));
    std::fs::write(&key, "new-fixture-key").unwrap();
    let settings = Settings {
        api_key_file: key.to_string_lossy().into(),
        ..settings
    };
    assert!(matches!(
        cache.get(&settings, None, &sources, now + Duration::from_secs(2)),
        KeyStatus::Found(_)
    ));
}

#[test]
fn displayed_key_status_invalidates_when_provider_or_key_sources_change() {
    fn fixture_env(name: &str) -> Option<String> {
        (name == "FIXTURE_KEY").then(|| "fixture-key".into())
    }
    let now = std::time::Instant::now();
    let mut cache = super::KeyStatusCache::default();
    let settings = Settings {
        api_key_env: "FIXTURE_KEY".into(),
        ..Settings::default()
    };
    let sources = KeySources {
        env: fixture_env,
        legacy_key_file: None,
    };
    assert_eq!(
        cache.get(&settings, None, &sources, now),
        KeyStatus::Found("$FIXTURE_KEY".into())
    );
    assert_eq!(
        cache.get(&settings, None, &KeySources::none(), now),
        KeyStatus::Missing("$FIXTURE_KEY".into())
    );
    let other = Settings {
        api_key_env: "OTHER_KEY".into(),
        base_url: "https://example.invalid/v1".into(),
        ..settings
    };
    assert_eq!(
        cache.get(&other, None, &sources, now),
        KeyStatus::Missing("$OTHER_KEY".into())
    );
}

#[test]
fn transcript_width_local_fallback_preserves_other_settings() {
    use super::TranscriptWidth;
    for value in ["\"unknown\"", "42", "false", "[]", "{ invalid = true }"] {
        let settings: Settings = toml::from_str(&format!(
            "transcript_width = {value}\nmodel = \"kept-model\"\nbase_url = \"https://kept.example/v1\"\napproval = \"auto\"\ntheme = \"kept-theme\"\nsandbox = true\n"
        )).unwrap();
        assert_eq!(settings.transcript_width, TranscriptWidth::Readable);
        assert_eq!(settings.model, "kept-model");
        assert_eq!(settings.base_url, "https://kept.example/v1");
        assert_eq!(settings.approval, "auto");
        assert_eq!(settings.theme, "kept-theme");
        assert!(settings.sandbox);
    }
    let old: Settings = toml::from_str("model = \"old\"").unwrap();
    assert_eq!(old.transcript_width, TranscriptWidth::Readable);
}

#[test]
fn all_transcript_widths_survive_settings_save_load() {
    use super::TranscriptWidth;
    let dir = tempfile::tempdir().unwrap();
    for width in [
        TranscriptWidth::Readable,
        TranscriptWidth::Wide,
        TranscriptWidth::Fill,
    ] {
        let settings = Settings {
            transcript_width: width,
            ..Settings::default()
        };
        settings.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path(), &KeySources::none()), settings);
    }
}

#[test]
fn defaults_point_at_a_public_endpoint() {
    let settings = Settings::default();
    assert_eq!(settings.base_url, "https://api.openai.com/v1");
    assert_eq!(settings.api_key_env, "");
    assert_eq!(settings.api_key_file, "");
    assert_eq!(settings.subagent_model, "");
    assert_eq!(settings.approval_mode(), ApprovalMode::AskForChanges);
}

#[test]
fn queued_verification_policy_is_opt_in_and_preserves_permissions() {
    assert!(!Settings::default().queue_after_verification);
    let old: Settings = toml::from_str("approval = \"auto\"\nsandbox = false\n").unwrap();
    assert!(!old.queue_after_verification);
    let dir = tempfile::tempdir().unwrap();
    for enabled in [true, false] {
        let settings = Settings {
            queue_after_verification: enabled,
            ..old.clone()
        };
        settings.save(dir.path()).unwrap();
        let loaded = Settings::load(dir.path(), &KeySources::none());
        assert_eq!(loaded, settings);
        assert_eq!(loaded.approval_mode(), ApprovalMode::Auto);
        assert!(!loaded.sandbox);
    }
}

#[test]
fn agent_model_provider_defaults_round_trip_without_changing_native_settings() {
    use flint_agent::AgentKind;
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings {
        default_agent: AgentKind::Droid,
        model: "native-selected-model".into(),
        base_url: "https://selected-provider.example/v1".into(),
        ..Settings::default()
    };
    settings.agent_options.insert(
        "droid".into(),
        [
            ("model".into(), "droid-selected-model".into()),
            ("provider".into(), "selected-provider".into()),
        ]
        .into(),
    );
    settings.save(dir.path()).unwrap();
    let loaded = Settings::load(dir.path(), &KeySources::none());
    assert_eq!(loaded, settings);
    assert_eq!(
        loaded.agent_options(AgentKind::Droid)["model"],
        "droid-selected-model"
    );
    assert!(loaded.agent_options(AgentKind::ClaudeCode).is_empty());
}

#[test]
fn opencode_defaults_and_models_round_trip_independently_of_other_agents() {
    use flint_agent::AgentKind;
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings {
        default_agent: AgentKind::OpenCode,
        ..Settings::default()
    };
    settings.agent_options.insert(
        super::agent_key(AgentKind::OpenCode).into(),
        [("model".into(), "provider/model".into())].into(),
    );
    settings.save(dir.path()).unwrap();
    let loaded = Settings::load(dir.path(), &KeySources::none());
    assert_eq!(loaded, settings);
    assert_eq!(
        loaded.agent_options(AgentKind::OpenCode)["model"],
        "provider/model"
    );
    assert!(loaded.agent_options(AgentKind::Droid).is_empty());
}

#[test]
fn existing_config_keeps_model_provider_and_defaults_to_native_agent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        Settings::path(dir.path()),
        "model = \"selected-model\"\nbase_url = \"https://selected-provider.example/v1\"\n",
    )
    .unwrap();
    let loaded = Settings::load(dir.path(), &KeySources::none());
    assert_eq!(loaded.default_agent, flint_agent::AgentKind::Flint);
    assert_eq!(loaded.model, "selected-model");
    assert_eq!(loaded.base_url, "https://selected-provider.example/v1");
}

#[test]
fn saved_auto_run_is_preserved() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(Settings::path(dir.path()), "approval = \"auto\"\n").expect("write");
    assert_eq!(
        Settings::load(dir.path(), &KeySources::none()).approval_mode(),
        ApprovalMode::Auto
    );
}

#[test]
fn missing_malformed_and_unknown_permissions_fail_safe() {
    let dir = tempfile::tempdir().unwrap();
    for config in [
        "",
        "approval = \"bogus\"",
        "approval = 42",
        "not valid toml [",
        "approval = \"AUTO\"",
        "approval = \"\"",
    ] {
        std::fs::write(Settings::path(dir.path()), config).unwrap();
        let settings = Settings::load(dir.path(), &KeySources::none());
        assert_eq!(settings.approval_mode(), ApprovalMode::AskForChanges);
        assert_ne!(settings.permission_choice_pending, Some(true));
    }
}

#[test]
fn legacy_permissions_are_preserved_without_first_run_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("legacy.key");
    std::fs::write(&key, "dummy").unwrap();
    let settings = Settings::load(
        dir.path(),
        &KeySources {
            legacy_key_file: Some(key),
            ..KeySources::none()
        },
    );
    assert_eq!(settings.approval_mode(), ApprovalMode::Auto);
    assert_eq!(settings.permission_choice_pending, Some(false));
}

#[test]
fn subagent_model_round_trips_and_is_wired_into_engine_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings = Settings {
        subagent_model: "  child-model  ".into(),
        ..Settings::default()
    };
    settings.save(dir.path()).expect("save");
    let sources = KeySources {
        env: |name| (name == "OPENAI_API_KEY").then(|| "test-key".into()),
        ..KeySources::none()
    };
    let loaded = Settings::load(dir.path(), &sources);
    assert_eq!(loaded.subagent_model, "  child-model  ");
    let config = crate::engine::config_for(
        dir.path(),
        &loaded,
        None,
        &sources,
        flint_agent::ApprovalMode::Auto,
    )
    .expect("config");
    assert_eq!(config.subagent_model.as_deref(), Some("child-model"));
    let sources = KeySources {
        env: |name| match name {
            "OPENAI_API_KEY" => Some("test-key".into()),
            "FLINT_SUBAGENT_MODEL" => Some("override-model".into()),
            _ => None,
        },
        ..KeySources::none()
    };
    let config = crate::engine::config_for(
        dir.path(),
        &loaded,
        None,
        &sources,
        flint_agent::ApprovalMode::Auto,
    )
    .expect("config");
    assert_eq!(config.subagent_model.as_deref(), Some("override-model"));
}

#[test]
fn an_existing_config_file_is_respected() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        Settings::path(dir.path()),
        "model = \"llama3.2\"\nbase_url = \"http://localhost:11434/v1\"\napi_key_env = \"MY_KEY\"\n",
    )
    .expect("write");
    assert_eq!(
        Settings::load(dir.path(), &KeySources::none()),
        Settings {
            model: "llama3.2".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key_env: "MY_KEY".to_string(),
            ..Settings::default()
        }
    );
}

#[test]
fn the_key_file_override_wins_and_is_trimmed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let key = dir.path().join("key");
    std::fs::write(&key, "  sk-test\n").expect("write");
    let settings = Settings {
        api_key_file: dir.path().join("other").display().to_string(),
        ..Settings::default()
    };
    let found = settings
        .resolve_key(Some(&key), &KeySources::none())
        .expect("key");
    assert_eq!(found.key, "sk-test");
    assert_eq!(
        settings.key_status(Some(&key), &KeySources::none()),
        KeyStatus::Found(display_path(&key))
    );
}

#[test]
fn an_early_install_resolves_to_the_local_shim_and_its_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let legacy = dir.path().join("legacy.key");
    std::fs::write(&legacy, "legacy-key\n").expect("write");
    let sources = KeySources {
        legacy_key_file: Some(legacy.clone()),
        ..KeySources::none()
    };
    let settings = Settings::load(dir.path(), &sources);
    assert_eq!(settings.base_url, "http://127.0.0.1:18433/v1");
    assert_eq!(settings.model, "deepseek-v4.1-flash");
    let found = settings.resolve_key(None, &sources).expect("key");
    assert_eq!(found.key, "legacy-key");
}

#[test]
fn the_legacy_key_is_never_used_for_another_endpoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let legacy = dir.path().join("legacy.key");
    std::fs::write(&legacy, "legacy-key\n").expect("write");
    let sources = KeySources {
        legacy_key_file: Some(legacy),
        ..KeySources::none()
    };
    assert!(Settings::default().resolve_key(None, &sources).is_none());
}

#[test]
fn engine_overrides_cannot_route_legacy_credentials_to_another_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("legacy.key");
    std::fs::write(&key, "dummy").unwrap();
    let sources = KeySources {
        env: |name| (name == "FLINT_BASE_URL").then(|| "http://127.0.0.1:9/v1".into()),
        legacy_key_file: Some(key),
    };
    let settings = Settings::load(dir.path(), &sources);
    assert!(
        crate::engine::config_for(dir.path(), &settings, None, &sources, ApprovalMode::Auto)
            .is_err()
    );
}

#[test]
fn environment_keys_come_from_the_injected_lookup() {
    let sources = KeySources {
        env: |name| (name == "OPENAI_API_KEY").then(|| "env-key".to_string()),
        ..KeySources::none()
    };
    let found = Settings::default()
        .resolve_key(None, &sources)
        .expect("key");
    assert_eq!(
        (found.key.as_str(), found.source.as_str()),
        ("env-key", "$OPENAI_API_KEY")
    );
}

#[test]
fn subtle_text_meets_normal_text_contrast_on_primary_surfaces() {
    fn luminance(color: gpui_kit::Hsla) -> f32 {
        let color = color.to_rgb();
        let linear = |value: f32| {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }
    let p = crate::theme::palette();
    for background in [p.bg, p.surface, p.raised, p.bubble] {
        let contrast = (luminance(p.text_subtle) + 0.05) / (luminance(background) + 0.05);
        assert!(contrast >= 4.5, "contrast {contrast} is below 4.5:1");
    }
}

#[test]
fn sandbox_and_mcp_servers_load_from_the_config_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(Settings::default().sandbox);
    std::fs::write(
        Settings::path(dir.path()),
        "sandbox = false\n\n[mcp_servers.github]\ncommand = \"github-mcp\"\nargs = [\"stdio\"]\n\
         env = { GITHUB_TOKEN = \"t\" }\n\n[mcp_servers.off]\ncommand = \"x\"\nenabled = false\n",
    )
    .expect("write");
    let settings = Settings::load(dir.path(), &KeySources::none());
    assert!(!settings.sandbox);
    let servers = settings.mcp_server_configs();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "github");
    assert_eq!(servers[0].command, "github-mcp");
    assert_eq!(servers[0].args, ["stdio"]);
    assert_eq!(
        servers[0].env,
        [("GITHUB_TOKEN".to_string(), "t".to_string())]
    );
    // Saving keeps them.
    settings.save(dir.path()).expect("save");
    assert_eq!(Settings::load(dir.path(), &KeySources::none()), settings);
}

#[test]
fn unreadable_config_is_kept_before_defaults_replace_it() {
    let dir = tempfile::tempdir().unwrap();
    // One wrong value type fails the whole file, so every setting below it is
    // dropped. The first save then writes defaults over the only copy.
    let broken = "model = \"hand-picked\"\n\
                  base_url = \"https://internal.example/v1\"\n\
                  approval = 3\n\
                  [mcp_servers.repo]\ncommand = \"repo-mcp\"\n";
    std::fs::write(Settings::path(dir.path()), broken).unwrap();

    let loaded = Settings::load(dir.path(), &KeySources::none());
    assert_eq!(loaded, Settings::default());
    loaded.save(dir.path()).unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("config.toml.invalid")).unwrap(),
        broken,
        "the unreadable settings must survive the first save"
    );
}

#[test]
fn readable_or_missing_config_creates_no_unreadable_copy() {
    let dir = tempfile::tempdir().unwrap();
    // Missing file: defaults, nothing to keep.
    Settings::load(dir.path(), &KeySources::none());
    assert!(!dir.path().join(Settings::UNREADABLE_NAME).exists());

    // Partially written but valid TOML still parses, so it is not a failure.
    std::fs::write(Settings::path(dir.path()), "model = \"kept-model\"\n").unwrap();
    let loaded = Settings::load(dir.path(), &KeySources::none());
    assert_eq!(loaded.model, "kept-model");
    assert!(!dir.path().join(Settings::UNREADABLE_NAME).exists());
}

#[test]
fn load_reports_a_discarded_settings_file_and_used_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        Settings::load_with_report(dir.path(), &KeySources::none()).1,
        super::SettingsRead::Used,
        "a missing file is not a rejection"
    );
    std::fs::write(Settings::path(dir.path()), "approval = 3\n").unwrap();
    let (loaded, read) = Settings::load_with_report(dir.path(), &KeySources::none());
    assert_eq!(read, super::SettingsRead::RejectedKept);
    assert_eq!(loaded, Settings::default());
    assert!(dir.path().join(Settings::UNREADABLE_NAME).exists());
    assert_eq!(
        Settings::load(dir.path(), &KeySources::none()),
        Settings::default(),
        "the plain loader keeps its old contract"
    );
}
